//! `taskslash moon`: runs moon targets one job per target, each once, in dependency order, so the
//! daemon decides how many run at once instead of moon's `--concurrency`.

use crate::Error;
use crate::capture::{Stop, pump};
use crate::client::{Demand, Job, Report, run_job};
use crate::moon::{self, Node};
use crate::protocol::Class;
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// How a node ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pass,
    Fail,
    /// A node it depends on, directly or not, failed, so it never ran.
    Skipped,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// A node's end: how, how long its command ran, and whether moon replayed it from cache.
#[derive(Debug, Clone, Copy)]
struct End {
    state: State,
    wall: Duration,
    cached: bool,
}

/// Runs `targets` (and every target they depend on) of the moon workspace above `cwd` through the
/// daemon at `socket`, `force` passing `--force` to each. Prints a summary line per node and
/// returns 0 when every node passed, else 1. A usage error when there is no workspace or a target
/// is wrong; an `Err` when moon cannot be found or asked.
pub fn moon(
    socket: &Path,
    cwd: &Path,
    targets: &[String],
    force: bool,
    class: Class,
) -> Result<i32, Error> {
    let root = moon::find_root(cwd)
        .ok_or_else(|| {
            Error::usage(format!(
                "no moon workspace: no .moon/workspace.yml in {} or above",
                cwd.display()
            ))
        })?
        .to_path_buf();
    let binary = moon::moon_binary(&root)?;
    let mut query = Command::new(&binary);
    query
        .args(["query", "tasks"])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(path) = gate_path(&root, env::var_os("PATH")) {
        query.env("PATH", path);
    }
    let output = query
        .output()
        .map_err(|e| Error::new(format!("cannot run {}: {e}", binary.display())))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "{} query tasks failed: {}",
            binary.display(),
            output.status
        )));
    }
    let tasks = moon::parse(&String::from_utf8_lossy(&output.stdout))?;
    let nodes = moon::graph(&tasks, targets)?;
    let repo = moon::repo_name(&root);

    let ends = schedule(&nodes, |node| {
        run_node(socket, &root, &binary, &repo, node, force, class)
    });

    let mut out = io::stdout().lock();
    let mut text = String::from("\n");
    for (node, end) in nodes.iter().zip(&ends) {
        text.push_str(&summary_line(&node.target, &key(&repo, node), end));
    }
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| Error::new(format!("cannot write the summary: {e}")))?;
    Ok(i32::from(ends.iter().any(|e| e.state != State::Pass)))
}

/// `PATH` with `<root>/node_modules/.bin` in front of `inherited`, as `bun run` sets it for a
/// script at the root, so a moon task's bare package binary (`tsc`, `oxlint`) is found. `None` when
/// `PATH` is unset (a child then gets only the workspace's bin, and cannot find `systemd-run`),
/// when that directory does not exist, or when the joined path is not valid: then `PATH` is left as
/// it was.
fn gate_path(root: &Path, inherited: Option<OsString>) -> Option<OsString> {
    let inherited = inherited?;
    let bin = root.join("node_modules/.bin");
    if !bin.is_dir() {
        return None;
    }
    let dirs = std::iter::once(bin).chain(env::split_paths(&inherited));
    env::join_paths(dirs).ok()
}

/// The key a node's job is recorded under: `<repo>/<target>`.
fn key(repo: &str, node: &Node) -> String {
    format!("{repo}/{}", node.target)
}

/// `<target> <pass|fail|skipped> <wall>[ cached] [<key>]`, a newline, and after a failure the
/// command that prints its log: `  log: taskslash logs --last <key>`.
fn summary_line(target: &str, key: &str, end: &End) -> String {
    let wall = match end.state {
        State::Skipped => "-".to_owned(),
        _ => format!("{:.1}s", end.wall.as_secs_f64()),
    };
    let cached = if end.cached { " cached" } else { "" };
    let mut line = format!("{target} {} {wall}{cached} [{key}]\n", end.state.as_str());
    if end.state == State::Fail {
        line.push_str(&format!("  log: taskslash logs --last {key}\n"));
    }
    line
}

/// Runs each node once on a thread of its own, as soon as every node it depends on has passed,
/// and returns how each ended, in `nodes`' order. A node whose dependency failed, directly or not,
/// is never run and ends `Skipped`. `nodes` is in dependency order (each after its `deps`).
fn schedule(nodes: &[Node], run: impl Fn(&Node) -> End + Sync) -> Vec<End> {
    let mut ends: Vec<Option<End>> = vec![None; nodes.len()];
    let mut started = vec![false; nodes.len()];
    thread::scope(|scope| {
        let (done, results) = mpsc::channel();
        let mut running = 0;
        loop {
            for (i, node) in nodes.iter().enumerate() {
                let ready = !started[i]
                    && node
                        .deps
                        .iter()
                        .all(|&d| ends[d].is_some_and(|e| e.state == State::Pass));
                if ready {
                    started[i] = true;
                    running += 1;
                    let done = done.clone();
                    let run = &run;
                    scope.spawn(move || {
                        // A panic (a write to a closed stderr) is that node's failure, never a
                        // node that never reports and leaves the loop waiting.
                        let end =
                            panic::catch_unwind(AssertUnwindSafe(|| run(node))).unwrap_or(End {
                                state: State::Fail,
                                wall: Duration::ZERO,
                                cached: false,
                            });
                        let _ = done.send((i, end));
                    });
                }
            }
            if running == 0 {
                break;
            }
            let Ok((i, end)) = results.recv() else { break };
            running -= 1;
            ends[i] = Some(end);
        }
    });
    ends.into_iter()
        .map(|end| {
            end.unwrap_or(End {
                state: State::Skipped,
                wall: Duration::ZERO,
                cached: false,
            })
        })
        .collect()
}

/// Runs one node as a daemon job: `<moon> run <target> --upstream none [--force]`.
fn run_node(
    socket: &Path,
    root: &Path,
    binary: &Path,
    repo: &str,
    node: &Node,
    force: bool,
    class: Class,
) -> End {
    let mut argv: Vec<OsString> = vec![
        binary.into(),
        "run".into(),
        node.target.clone().into(),
        "--upstream".into(),
        "none".into(),
    ];
    if force {
        argv.push("--force".into());
    }
    let job = Job {
        name: Some(key(repo, node)),
        kind: moon::kind(&node.task),
        class,
        demand: Demand::default(),
        argv: &argv,
        cwd: root,
        label: Some(&node.target),
    };
    let mut wall = Duration::ZERO;
    let result = run_job(socket, &job, None, |mut command, log, stop| {
        if let Some(path) = gate_path(root, env::var_os("PATH")) {
            command.env("PATH", path);
        }
        let clock = Instant::now();
        let ran = exec(command, &node.target, log, stop);
        wall = clock.elapsed();
        let (status, cached) = ran?;
        let report = Report {
            cached: Some(cached),
            moon_hash: last_hash(root, node),
        };
        Ok((status, report))
    });
    match result {
        Ok((code, report)) => End {
            state: if code == 0 { State::Pass } else { State::Fail },
            wall,
            cached: report.cached == Some(true),
        },
        Err(e) => {
            eprintln!("taskslash: {}: {e}", node.target);
            End {
                state: State::Fail,
                wall,
                cached: false,
            }
        }
    }
}

/// Runs `command` with its stdout and stderr passed through line by line, each to the matching
/// stream and both into `log`, and says whether moon replayed `target` from its cache: moon prints
/// `<target> (cached, <hash>)` on stdout then. (The target's `lastRun.json` cannot say: moon
/// rewrites its `lastRunTime` on a cache hit too.)
fn exec(
    mut command: Command,
    target: &str,
    mut log: Option<File>,
    stop: &Arc<Stop>,
) -> io::Result<(ExitStatus, bool)> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // moon shares this process's group, so a cancel signals moon itself, which stops its task.
    stop.started(libc::pid_t::try_from(child.id()).unwrap_or(0), false);
    let mut sources = Vec::new();
    // Whether each source is stdout (else stderr).
    let mut is_stdout = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        sources.push(stdout.into());
        is_stdout.push(true);
    }
    if let Some(stderr) = child.stderr.take() {
        sources.push(stderr.into());
        is_stdout.push(false);
    }
    let marker = format!("{target} (cached");
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let done = &done;
        let log = log.as_mut();
        let is_stdout = &is_stdout;
        let marker = &marker;
        let pumping = scope.spawn(move || {
            let mut cached = false;
            let mut partial: Vec<Vec<u8>> = vec![Vec::new(); is_stdout.len()];
            let emit = |i: usize, line: &[u8], cached: &mut bool| {
                // A closed stream is no reason to stop the job; keep reading so it never blocks.
                if is_stdout[i] {
                    *cached |= plain(line).contains(marker.as_str());
                    let _ = io::stdout().lock().write_all(line);
                } else {
                    let _ = io::stderr().lock().write_all(line);
                }
            };
            pump(sources, log, done, |i, chunk| {
                partial[i].extend_from_slice(chunk);
                while let Some(at) = partial[i].iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = partial[i].drain(..=at).collect();
                    emit(i, &line, &mut cached);
                }
                true
            });
            for (i, rest) in partial.iter().enumerate() {
                if !rest.is_empty() {
                    emit(i, rest, &mut cached);
                }
            }
            cached
        });
        let status = child.wait();
        stop.ended();
        done.store(true, Ordering::Release);
        let cached = pumping.join().unwrap_or(false);
        Ok((status?, cached))
    })
}

/// `line` as text with its terminal escape sequences (`ESC [ ... <letter>`) taken out.
fn plain(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The hash in `node`'s `.moon/cache/states/<project>/<task>/lastRun.json`, when it names this
/// target and the hash is not empty (an uncached task has none).
fn last_hash(root: &Path, node: &Node) -> Option<String> {
    let path: PathBuf = [
        ".moon",
        "cache",
        "states",
        &node.project,
        &node.task,
        "lastRun.json",
    ]
    .iter()
    .fold(root.to_path_buf(), |path, part| path.join(part));
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    (value.get("target")?.as_str()? == node.target).then_some(())?;
    let hash = value.get("hash")?.as_str()?;
    (!hash.is_empty()).then(|| hash.to_owned())
}

#[cfg(test)]
mod tests;
