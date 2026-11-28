//! `taskslash moon` through a real daemon and the repo's own moon, on a copy of the fixture
//! workspace in `tests/fixtures/moon`: `a:gen` (cache off) appends a line to `a/gen.log` and both
//! `a:typecheck` and `a:test` depend on it; `b:test` passes, `b:typecheck` fails and `b:lint`
//! depends on it. `b:test` writes `b-test-out` to stdout and `b-test-err` to stderr.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod common;
#[path = "moon/fixture.rs"]
mod fixture;

use fixture::{BIN, Scratch};

/// The summary at the end of `taskslash moon`'s stdout: target to (state, cached). Each line ends
/// with the target's key in brackets (the fixture repository is named `fixture`); the indented
/// `log:` line after a failure is skipped.
fn summary(out: &Output) -> BTreeMap<String, (String, bool)> {
    let text = String::from_utf8_lossy(&out.stdout);
    let block = text.rsplit("\n\n").next().unwrap_or_default();
    block
        .lines()
        .filter(|line| !line.starts_with("  log: "))
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert!(
                (4..=5).contains(&words.len()),
                "a summary line: {line:?}\n{text}"
            );
            assert_eq!(
                words.last().copied(),
                Some(format!("[fixture/{}]", words[0]).as_str()),
                "the key ends the line: {line:?}"
            );
            (
                words[0].to_owned(),
                (words[1].to_owned(), words.get(3) == Some(&"cached")),
            )
        })
        .collect()
}

fn states(summary: &BTreeMap<String, (String, bool)>) -> BTreeMap<&str, &str> {
    summary
        .iter()
        .map(|(t, (s, _))| (t.as_str(), s.as_str()))
        .collect()
}

/// What a plain `moon run` reports per target it finished: a target with a finished line
/// (`<target> (<time>, <hash>)`) passed, unless moon also says `Task <target> failed to run`.
fn moon_states(out: &Output) -> BTreeMap<String, &'static str> {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut states = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line
            .trim()
            .strip_prefix("\u{25ae}\u{25ae}\u{25ae}\u{25ae} ")
        else {
            continue;
        };
        if let Some((target, inside)) = rest.split_once(" (")
            && inside.contains(", ")
        {
            states.insert(target.to_owned(), "pass");
        }
    }
    for line in text.lines() {
        if let Some(target) = line
            .split_once("Task ")
            .and_then(|(_, rest)| rest.strip_suffix(" failed to run."))
        {
            states.insert(target.to_owned(), "fail");
        }
    }
    states
}

fn gen_lines(workspace: &Path) -> usize {
    fs::read_to_string(workspace.join("a/gen.log"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[test]
fn a_shared_uncached_dependency_runs_once_and_a_failure_exits_1() {
    let daemon = Scratch::new().daemon();
    let out = daemon.scratch.taskslash(&["moon", ":typecheck", ":test"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(gen_lines(&daemon.scratch.workspace), 1, "a:gen ran once");
    let summary = summary(&out);
    assert_eq!(
        states(&summary),
        BTreeMap::from([
            ("a:gen", "pass"),
            ("a:test", "pass"),
            ("a:typecheck", "pass"),
            ("b:test", "pass"),
            ("b:typecheck", "fail"),
        ])
    );
    // The summary is in dependency order: a:gen before what needs it.
    let text = String::from_utf8_lossy(&out.stdout);
    let at = |t: &str| text.rfind(&format!("\n{t} ")).unwrap();
    assert!(
        at("a:gen") < at("a:test") && at("a:gen") < at("a:typecheck"),
        "{text}"
    );
}

#[test]
fn a_failure_skips_what_depends_on_it() {
    let daemon = Scratch::new().daemon();
    let out = daemon.scratch.taskslash(&["moon", "b:lint"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        states(&summary(&out)),
        BTreeMap::from([("b:lint", "skipped"), ("b:typecheck", "fail")])
    );
    // The failure is followed by the command that prints its log, and that command works; the
    // skipped target never ran, so it has none.
    let text = String::from_utf8_lossy(&out.stdout);
    let log = "  log: taskslash logs --last fixture/b:typecheck\n";
    assert!(
        text.contains(&format!("[fixture/b:typecheck]\n{log}")),
        "{text}"
    );
    assert!(
        text.ends_with("b:lint skipped - [fixture/b:lint]\n"),
        "{text}"
    );
    assert_eq!(text.matches("  log: ").count(), 1, "{text}");
    let logs = daemon
        .scratch
        .taskslash(&["logs", "--last", "fixture/b:typecheck"]);
    assert_eq!(logs.status.code(), Some(0), "{logs:?}");
    assert!(!logs.stdout.is_empty(), "{logs:?}");
}

/// moon 2.6.5 stops at its first failure, so on `:typecheck :test` it finishes only some targets:
/// each of those has the same result here. On `:test`, where nothing fails, the sets are equal.
#[test]
fn the_results_match_moon_run_on_a_fresh_copy() {
    let daemon = Scratch::new().daemon();
    let ours = daemon.scratch.taskslash(&["moon", ":typecheck", ":test"]);
    let ours = summary(&ours);
    let fresh = Scratch::new();
    let theirs = fresh
        .command(
            "node_modules/.bin/moon",
            &["run", "--concurrency", "3", ":typecheck", ":test"],
        )
        .output()
        .unwrap();
    assert_eq!(theirs.status.code(), Some(1), "{theirs:?}");
    let theirs_states = moon_states(&theirs);
    assert_eq!(
        theirs_states.get("b:typecheck"),
        Some(&"fail"),
        "{theirs:?}"
    );
    for (target, state) in &theirs_states {
        assert_eq!(states(&ours).get(target.as_str()), Some(state), "{target}");
    }

    let ours = daemon.scratch.taskslash(&["moon", ":test", "--force"]);
    assert_eq!(ours.status.code(), Some(0), "{ours:?}");
    let fresh = Scratch::new();
    let theirs = fresh
        .command(
            "node_modules/.bin/moon",
            &["run", "--concurrency", "3", ":test"],
        )
        .output()
        .unwrap();
    assert_eq!(theirs.status.code(), Some(0), "{theirs:?}");
    let ours = summary(&ours);
    let ours: BTreeMap<String, &str> = states(&ours)
        .into_iter()
        .map(|(t, s)| (t.to_owned(), s))
        .collect();
    assert_eq!(ours, moon_states(&theirs));
}

#[test]
fn a_cache_hit_is_recorded_as_cached_with_its_hash() {
    let daemon = Scratch::new().daemon();
    let first = daemon.scratch.taskslash(&["moon", "b:test"]);
    let second = daemon.scratch.taskslash(&["moon", "b:test"]);
    assert_eq!(
        summary(&first)["b:test"],
        ("pass".to_owned(), false),
        "{first:?}"
    );
    assert_eq!(
        summary(&second)["b:test"],
        ("pass".to_owned(), true),
        "{second:?}"
    );

    let out = daemon
        .scratch
        .taskslash(&["history", "fixture/b:test", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let history: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let runs = history["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(
        (runs[0]["cached"].as_bool(), runs[1]["cached"].as_bool()),
        (Some(true), Some(false))
    );
    let hash = runs[0]["moon_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64, "{hash}");
    assert_eq!(
        runs[1]["moon_hash"].as_str(),
        Some(hash),
        "the same inputs, the same hash"
    );
    assert_eq!(runs[0]["kind"], "test");
}

#[test]
fn outside_a_workspace_or_with_a_bad_target_is_a_usage_error() {
    let daemon = Scratch::new().daemon();
    let out = daemon.scratch.taskslash(&["moon", "a:nope"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("a:nope matches no task\n  tasks in a: gen, test, typecheck\n"),
        "{err}"
    );
    let out = daemon
        .scratch
        .command(BIN, &["moon", ":test"])
        .current_dir(&daemon.scratch.dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no moon workspace"));
}

#[test]
fn each_targets_stdout_and_stderr_land_in_its_own_log() {
    let daemon = Scratch::new().daemon();
    let out = daemon.scratch.taskslash(&["moon", "b:test", "b:typecheck"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // Passed through to the caller's matching streams.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("b-test-out"),
        "{out:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("b-test-err"),
        "{out:?}"
    );
    let log = |target: &str| {
        let out = daemon
            .scratch
            .taskslash(&["history", &format!("fixture/{target}"), "--json"]);
        let history: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let path = history["runs"][0]["log_path"].as_str().unwrap().to_owned();
        assert!(
            Path::new(&path).starts_with(daemon.scratch.dir.join("logs")),
            "{path}"
        );
        (path.clone(), fs::read_to_string(&path).unwrap())
    };
    let (test_path, test_log) = log("b:test");
    let (typecheck_path, typecheck_log) = log("b:typecheck");
    assert_ne!(test_path, typecheck_path);
    assert!(test_log.contains("b-test-out"), "{test_log}");
    assert!(test_log.contains("b-test-err"), "{test_log}");
    assert!(!typecheck_log.contains("b-test"), "{typecheck_log}");
    assert!(typecheck_log.contains("b:typecheck"), "{typecheck_log}");
}

/// A bare package binary (`tsc`, `oxlint`) is a moon command that moon finds on the workspace's
/// `node_modules/.bin`, which `bun run` puts on `PATH` for the same command. The test's `PATH` has
/// no `node_modules/.bin` in it, so only `taskslash moon` putting the workspace's `.bin` in front
/// can find the binary: without that it fails with 127.
#[test]
fn a_bare_binary_in_the_workspaces_node_modules_bin_runs_without_a_path_prefix() {
    let daemon = Scratch::new().daemon();
    let bin = daemon.scratch.workspace.join("node_modules/.bin");
    let tool = bin.join("fixture-tool");
    fs::write(&tool, "#!/bin/sh\necho fixture-tool-ran\n").unwrap();
    fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::env::split_paths(&inherited).filter(|dir| !dir.ends_with("node_modules/.bin")),
    )
    .unwrap();
    let out = daemon
        .scratch
        .command(BIN, &["moon", "b:bare"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(states(&summary(&out))["b:bare"], "pass", "{out:?}");
}

/// A repository with `scripts/moon.ts` runs moon through it, as its own scripts do (it finds a
/// toolchain, sets what a task needs): `taskslash moon` asks it for the tasks and runs each target
/// through it. The wrapper here records its arguments and sets `FIXTURE_WRAPPED`, which `b:wrapped`
/// needs to pass; plain moon fails it.
#[test]
fn a_repos_moon_wrapper_runs_the_query_and_every_target() {
    let daemon = Scratch::new().daemon();
    let workspace = &daemon.scratch.workspace;
    let plain = daemon.scratch.taskslash(&["moon", "b:wrapped"]);
    assert_eq!(plain.status.code(), Some(1), "no wrapper yet: {plain:?}");

    fs::create_dir_all(workspace.join("scripts")).unwrap();
    fs::write(
        workspace.join("scripts/moon.ts"),
        r#"import { appendFileSync } from "node:fs";
import { join } from "node:path";
const root = join(import.meta.dir, "..");
const args = process.argv.slice(2);
appendFileSync(join(root, "wrapped.log"), args.join(" ") + "\n");
const moon = Bun.spawnSync([process.execPath, join(root, "node_modules/.bin/moon"), ...args], {
  env: { ...process.env, FIXTURE_WRAPPED: "yes" },
  stdio: ["inherit", "inherit", "inherit"],
});
process.exit(moon.exitCode ?? 1);
"#,
    )
    .unwrap();
    let out = daemon
        .scratch
        .taskslash(&["moon", "b:wrapped", "b:test", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        states(&summary(&out)),
        BTreeMap::from([("b:test", "pass"), ("b:wrapped", "pass")])
    );
    let log = fs::read_to_string(workspace.join("wrapped.log")).unwrap();
    let mut calls: Vec<&str> = log.lines().collect();
    calls.sort_unstable();
    assert_eq!(
        calls,
        [
            "query tasks",
            "run b:test --upstream none --force",
            "run b:wrapped --upstream none --force",
        ],
        "{log}"
    );
}

/// The processes (not zombies) but `except` whose working directory is `dir` or under it.
fn processes_in(dir: &Path, except: u32) -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .is_ok_and(|pid| pid != except)
        })
        .filter(|entry| {
            fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd.starts_with(dir))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// A `taskslash moon` killed as soon as the daemon has admitted its target, so mostly while it
/// forks moon and before it has told the daemon moon's pid: no moon and nothing it started is left
/// running, and the daemon frees the job. (The daemon runs in the workspace too.)
fn a_moon_whose_client_is_killed_leaves_nothing_running(launch: Option<&str>) {
    let daemon = Scratch::new().daemon_on(launch);
    let scratch = &daemon.scratch;
    let mut client = scratch
        .command(BIN, &["moon", "b:hang"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let jobs = || {
        let out = scratch.taskslash(&["status", "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let status: Value = serde_json::from_slice(&out.stdout).unwrap();
        status["jobs"].as_array().cloned().unwrap_or_default()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !jobs().iter().any(|job| job["key"] == "fixture/b:hang") {
        assert!(Instant::now() < deadline, "b:hang was never admitted");
        thread::sleep(Duration::from_millis(5));
    }
    client.kill().unwrap();
    client.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = processes_in(&scratch.workspace, daemon.pid());
        if left.is_empty() && jobs().is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            for pid in &left {
                let _ = Command::new("kill").args(["-9", pid]).status();
            }
            panic!("still running after its client was killed: {left:?}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_moon_whose_client_is_killed_leaves_nothing_running_on_the_machines_path() {
    a_moon_whose_client_is_killed_leaves_nothing_running(None);
}

#[test]
fn a_moon_whose_client_is_killed_leaves_nothing_running_on_the_plain_path() {
    a_moon_whose_client_is_killed_leaves_nothing_running(Some("plain"));
}
