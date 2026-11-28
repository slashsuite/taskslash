//! `taskslash history` through a real `taskslash daemon`, each test with its own socket and
//! database. Each environment asserts the launch path it has: the scope path where there is a
//! systemd user session (this desktop, and CI, which starts one), the plain path where there is not.

use serde_json::Value;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

mod common;
#[path = "common/session.rs"]
mod session;

const BIN: &str = env!("CARGO_BIN_EXE_taskslash");
const MIB: f64 = (1u64 << 20) as f64;

/// A directory of its own under the temp dir, holding the socket and the database. Removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tkh-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        common::write_machine(&dir.join("machine.json"), 0.0);
        Self { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env("TASKSLASH_SOCKET", self.dir.join("d.sock"))
            .env("TASKSLASH_DB", self.dir.join("runs.db"))
            .env("TASKSLASH_FAKE_MACHINE", self.dir.join("machine.json"))
            .current_dir(&self.dir)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("the binary runs")
    }

    /// `history --json` of `key`: `{"estimate": ..., "runs": [...]}`.
    fn history_json(&self, key: &str) -> Value {
        let out = self.run(&["history", key, "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        serde_json::from_slice(&out.stdout).expect("history prints JSON")
    }

    /// The runs of `key`, from `history --json`.
    fn history(&self, key: &str) -> Vec<Value> {
        match self.history_json(key).get("runs") {
            Some(Value::Array(runs)) => runs.clone(),
            other => panic!("history printed no runs: {other:?}"),
        }
    }

    /// Starts `taskslash daemon` with its stderr in `daemon.log` and waits up to 5 s for it.
    fn daemon(self) -> Daemon {
        let log = fs::File::create(self.dir.join("daemon.log")).unwrap();
        let child = self
            .command(&["daemon"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("the daemon starts");
        let mut daemon = Daemon {
            child,
            scratch: self,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(daemon.scratch.dir.join("d.sock")).is_err() {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                panic!("the daemon exited early: {status}: {}", daemon.log());
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not accept in 5 s"
            );
            thread::sleep(Duration::from_millis(20));
        }
        eprintln!("daemon: {}", daemon.log().trim());
        daemon
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A running daemon, killed on drop (before its scratch directory is removed).
struct Daemon {
    child: Child,
    scratch: Scratch,
}

impl Daemon {
    fn log(&self) -> String {
        fs::read_to_string(self.scratch.dir.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn number(run: &Value, field: &str) -> f64 {
    run[field]
        .as_f64()
        .unwrap_or_else(|| panic!("{field} is not a number in {run}"))
}

fn assert_between(what: &str, value: f64, low: f64, high: f64) {
    assert!(
        (low..=high).contains(&value),
        "{what} is {value}, not between {low} and {high}"
    );
}

#[test]
fn a_run_is_recorded_with_its_exit_code_and_wall_time() {
    let daemon = Scratch::new().daemon();
    let out = daemon
        .scratch
        .run(&["run", "--name", "t1", "--", "sh", "-c", "exit 4"]);
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let runs = daemon.scratch.history("t1");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["exit_code"], 4, "{}", runs[0]);
    assert!(number(&runs[0], "wall_ms") > 0.0, "{}", runs[0]);
    assert_eq!(runs[0]["argv"], serde_json::json!(["sh", "-c", "exit 4"]));
    assert_eq!(runs[0]["kind"], "command", "{}", runs[0]);
}

#[test]
fn history_estimates_from_the_newest_kind_and_says_where_each_figure_came_from() {
    let daemon = Scratch::new().daemon();
    let out = daemon
        .scratch
        .run(&["run", "--name", "t4", "--kind", "build", "--", "sleep", "1"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let history = daemon.scratch.history_json("t4");
    let estimate = &history["estimate"];
    eprintln!("t4: {estimate}");
    assert_eq!(history["runs"][0]["kind"], "build", "{history}");
    if session::scope_expected() {
        assert_eq!(estimate["samples"]["measured"], 1, "{estimate}");
        // One measured run: memory and I/O from it; CPU and duration only when it was calm.
        assert_eq!(estimate["source"]["mem"], "history", "{estimate}");
        assert_eq!(estimate["source"]["io"], "history", "{estimate}");
    } else {
        assert_eq!(estimate["samples"]["measured"], 0, "{estimate}");
        assert_eq!(estimate["cpu_threads"], 4.0, "{estimate}");
        assert_eq!(estimate["mem_mib"], 4096, "{estimate}");
        for figure in ["cpu", "mem", "io", "duration"] {
            assert_eq!(estimate["source"][figure], "default", "{estimate}");
        }
    }
    // A key with no runs is not answered with a default estimate as if it were real.
    let never = daemon.scratch.run(&["history", "never", "--json"]);
    assert_eq!(never.status.code(), Some(1), "{never:?}");
    assert!(never.stdout.is_empty(), "{never:?}");
}

#[test]
fn a_run_costs_what_it_used_on_the_path_this_machine_has() {
    let scoped = session::scope_expected();
    eprintln!("launch path: {}", if scoped { "scope" } else { "plain" });
    let daemon = Scratch::new().daemon();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/probe.py");
    let dir = daemon.scratch.dir.to_string_lossy().into_owned();
    let out = daemon.scratch.run(&[
        "run",
        "--name",
        "t2",
        "--",
        "python3",
        &probe.to_string_lossy(),
        "2",
        "200",
        "50",
        &dir,
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let runs = daemon.scratch.history("t2");
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    eprintln!("t2: {run}");
    assert_eq!(run["exit_code"], 0, "{run}");
    assert_eq!(run["launch"], session::expected_launch(None), "{run}");

    if !scoped {
        assert!(
            daemon
                .log()
                .contains("no systemd user session, jobs run unmeasured"),
            "{}",
            daemon.log()
        );
        for field in ["cpu_usec", "peak_cpu_5s", "peak_mem_bytes", "written_bytes"] {
            assert!(run[field].is_null(), "{field} is not null in {run}");
        }
        return;
    }
    assert_between("CPU seconds", number(run, "cpu_usec") / 1e6, 3.4, 4.6);
    assert_between(
        "peak memory MiB",
        number(run, "peak_mem_bytes") / MIB,
        200.0,
        300.0,
    );
    assert_between(
        "MiB written",
        number(run, "written_bytes") / MIB,
        40.0,
        80.0,
    );
}

#[test]
fn history_with_no_daemon_exits_1() {
    let scratch = Scratch::new();
    let out = scratch.run(&["history", "t1"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no daemon at"));
}

#[test]
fn plain_history_prints_a_header_and_a_line_per_run() {
    let daemon = Scratch::new().daemon();
    for code in ["0", "3"] {
        let script = format!("exit {code}");
        daemon
            .scratch
            .run(&["run", "--name", "t3", "--", "sh", "-c", &script]);
    }
    let out = daemon.scratch.run(&["history", "t3"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    assert!(lines[0].starts_with("estimate  cpu "), "{text}");
    assert!(lines[1].starts_with("samples   "), "{text}");
    let header: Vec<&str> = lines[2].split_whitespace().collect();
    assert_eq!(
        header,
        [
            "id",
            "started_at",
            "wall",
            "cpu_s",
            "peak_cpu_5s",
            "peak_mem_mib",
            "written_mib",
            "exit",
            "cached",
            "where"
        ]
    );
    // Newest first; `-` in `cached` for a run that is not a moon target's; then the worktree.
    let dir = shown(&daemon.scratch);
    assert!(lines[3].ends_with(&format!(" 3       -  {dir}")), "{text}");
    assert!(lines[4].ends_with(&format!(" 0       -  {dir}")), "{text}");
}

/// The scratch directory's name as the `where` column shows it: cut to `..` and its last 10
/// characters past 12 (a 7 digit pid makes it 13).
fn shown(scratch: &Scratch) -> String {
    let name = scratch
        .dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 12 {
        name
    } else {
        format!("..{}", chars[chars.len() - 10..].iter().collect::<String>())
    }
}

fn ok(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn history_with_no_key_lists_the_keys_newest_first() {
    let daemon = Scratch::new().daemon();
    for name in ["alpha", "beta", "alpha"] {
        ok(&daemon.scratch.run(&["run", "--name", name, "--", "true"]));
    }
    let out = daemon
        .scratch
        .run(&["run", "--name", "gamma", "--", "sh", "-c", "exit 5"]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    let out = daemon.scratch.run(&["history"]);
    ok(&out);
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<Vec<&str>> = text
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines[0],
        ["last_run", "runs", "exit", "where", "key"],
        "{text}"
    );
    let dir = shown(&daemon.scratch);
    let rows: Vec<(&str, &str, &str, &str)> = lines[1..]
        .iter()
        .map(|l| (l[1], l[2], l[3], l[4]))
        .collect();
    assert_eq!(
        rows,
        [
            ("1", "5", &*dir, "gamma"),
            ("2", "0", &*dir, "alpha"),
            ("1", "0", &*dir, "beta"),
        ],
        "{text}"
    );

    let out = daemon.scratch.run(&["history", "--json"]);
    ok(&out);
    let keys: Value = serde_json::from_slice(&out.stdout).expect("history prints JSON");
    let keys = keys.as_array().expect("an array");
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert_eq!(keys[0]["key"], "gamma");
    assert_eq!(keys[0]["runs"], 1);
    assert_eq!(keys[0]["last_exit_code"], 5);
    let name = daemon.scratch.dir.file_name().unwrap().to_string_lossy();
    assert_eq!(keys[0]["worktree"], &*name, "JSON keeps the whole name");
    assert_eq!(keys[1]["key"], "alpha");
    assert_eq!(keys[1]["runs"], 2);
    assert!(
        keys[0]["last_started_at"].as_str().unwrap()
            >= keys[1]["last_started_at"].as_str().unwrap()
    );
}

#[test]
fn an_unknown_key_exits_1_naming_keys_like_it() {
    let daemon = Scratch::new().daemon();
    for name in ["shop/web:test", "shop/api:test", "bun test"] {
        ok(&daemon.scratch.run(&["run", "--name", name, "--", "true"]));
    }
    let out = daemon.scratch.run(&["history", "web"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("taskslash: no runs of \"web\"\n"), "{err}");
    assert!(err.contains("    shop/web:test\n"), "{err}");
    assert!(!err.contains("shop/api:test"), "{err}");

    let out = daemon.scratch.run(&["history", "TEST", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    for key in ["shop/web:test", "shop/api:test", "bun test"] {
        assert!(err.contains(&format!("    {key}\n")), "{err}");
    }

    let out = daemon.scratch.run(&["history", "zzzzzzzz"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("with no key lists the keys"),
        "{out:?}"
    );
}

#[test]
fn a_long_or_scripted_command_gets_a_short_key_that_typing_it_whole_finds() {
    let daemon = Scratch::new().daemon();
    let long = "wrap:browser:bunx moon run storybook:visual -- --all --group navigation";
    ok(&daemon.scratch.run(&["run", "--name", long, "--", "true"]));
    ok(&daemon
        .scratch
        .run(&["run", "--", "sh", "-c", "/bin/true --flag", "x"]));

    let out = daemon.scratch.run(&["history", "--json"]);
    ok(&out);
    let keys: Value = serde_json::from_slice(&out.stdout).expect("history prints JSON");
    let mut keys: Vec<&str> = keys
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "true --flag",
            "wrap:browser:bunx moon r...c2fc2...--all --group navigation"
        ]
    );
    for typed in [long, keys[1]] {
        assert_eq!(daemon.scratch.history(typed).len(), 1, "{typed}");
        let out = daemon.scratch.run(&["logs", "--last", typed]);
        ok(&out);
        let out = daemon.scratch.run(&["logs", "--failed", typed]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "no failed run, and no hang: {out:?}"
        );
    }
}

#[test]
fn a_clone_in_another_folder_finds_the_runs_of_its_origin() {
    let daemon = Scratch::new().daemon();
    let clone = daemon.scratch.dir.join("shop-copy");
    fs::create_dir_all(clone.join(".moon")).unwrap();
    fs::write(clone.join(".moon/workspace.yml"), "projects: {}\n").unwrap();
    fs::create_dir_all(clone.join(".git")).unwrap();
    fs::write(
        clone.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@github.com:me/shop.git\n",
    )
    .unwrap();
    ok(&daemon
        .scratch
        .run(&["run", "--name", "shop/cli:test", "--", "true"]));

    let out = daemon
        .scratch
        .command(&["history", "cli:test", "--json"])
        .current_dir(&clone)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "key: shop/cli:test\n");
}

#[test]
fn a_moon_target_inside_a_workspace_finds_its_repos_key() {
    let daemon = Scratch::new().daemon();
    let shop = daemon.scratch.dir.join("shop");
    fs::create_dir_all(shop.join(".moon")).unwrap();
    fs::write(shop.join(".moon/workspace.yml"), "projects: {}\n").unwrap();
    fs::create_dir_all(shop.join("apps/cli")).unwrap();
    ok(&daemon
        .scratch
        .run(&["run", "--name", "shop/cli:test", "--", "true"]));
    let in_dir = |dir: &Path, args: &[&str]| {
        daemon
            .scratch
            .command(args)
            .current_dir(dir)
            .output()
            .unwrap()
    };

    let out = in_dir(&shop.join("apps/cli"), &["history", "cli:test", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "key: shop/cli:test\n",
        "the key used, on stderr"
    );
    let history: Value = serde_json::from_slice(&out.stdout).expect("stdout is only the JSON");
    assert_eq!(history["runs"].as_array().unwrap().len(), 1, "{history}");

    // A key that has runs as typed is never second-guessed: no `key:` line.
    let out = in_dir(&shop, &["history", "shop/cli:test"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");

    // Outside a workspace, the typed key is all there is.
    let out = daemon.scratch.run(&["history", "cli:test"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("taskslash: no runs of \"cli:test\"\n"),
        "{err}"
    );
    assert!(err.contains("    shop/cli:test\n"), "{err}");

    // Inside, a target with runs under neither form names both.
    let out = in_dir(&shop, &["history", "web:test"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("taskslash: no runs of \"web:test\" nor of \"shop/web:test\"\n"),
        "{err}"
    );
}

#[test]
fn limit_and_where_filter_the_runs() {
    let daemon = Scratch::new().daemon();
    let other = daemon.scratch.dir.join("other");
    fs::create_dir(&other).unwrap();
    for _ in 0..4 {
        ok(&daemon.scratch.run(&["run", "--name", "k", "--", "true"]));
    }
    ok(&daemon
        .scratch
        .command(&["run", "--name", "k", "--", "true"])
        .current_dir(&other)
        .output()
        .unwrap());

    let runs = |args: &[&str]| -> Vec<Value> {
        let mut all = vec!["history", "k", "--json"];
        all.extend_from_slice(args);
        let out = daemon.scratch.run(&all);
        ok(&out);
        let history: Value = serde_json::from_slice(&out.stdout).unwrap();
        history["runs"].as_array().unwrap().clone()
    };
    assert_eq!(runs(&[]).len(), 5);
    assert_eq!(runs(&["--limit", "3"]).len(), 3);
    let there = runs(&["--where", "other"]);
    assert_eq!(there.len(), 1, "{there:?}");
    assert_eq!(there[0]["submitter"], other.to_string_lossy().as_ref());
    assert_eq!(runs(&["--where", "other/"]).len(), 1, "a trailing slash");

    let out = daemon.scratch.run(&["history", "k", "--where", "nowhere"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taskslash: no runs of \"k\" in nowhere\n"
    );
    let out = daemon.scratch.run(&["history", "k", "--limit", "0"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn a_daemon_older_than_the_client_is_named_as_such() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let scratch = Scratch::new();
    let listener = UnixListener::bind(scratch.dir.join("d.sock")).unwrap();
    let old = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        (&stream)
            .write_all(
                b"{\"type\":\"error\",\"message\":\"bad request: unknown variant `keys`, expected one of `submit`, `exited`, `history` at line 1 column 14\"}\n",
            )
            .unwrap();
        line
    });
    let out = scratch.run(&["history"]);
    assert_eq!(old.join().unwrap(), "{\"type\":\"keys\"}\n");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "taskslash: the daemon is older than this client; reinstall it (bun scripts/install-service.ts)\n"
    );
}
