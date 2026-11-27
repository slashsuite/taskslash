//! `taskslash-floor` passes its command's exit code and output through unchanged, and
//! `floor-bench` skips the `taskslash` row when no daemon answers. No network, no daemon.

use std::process::{Command, Output, Stdio};

const FLOOR: &str = env!("CARGO_BIN_EXE_taskslash-floor");
const BENCH: &str = env!("CARGO_BIN_EXE_floor-bench");

fn floor(args: &[&str]) -> Output {
    Command::new(FLOOR)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("taskslash-floor runs")
}

#[test]
fn exit_codes_come_back_unchanged() {
    for code in [0, 3, 42] {
        let out = floor(&["--", "sh", "-c", &format!("exit {code}")]);
        assert_eq!(out.status.code(), Some(code));
    }
}

#[test]
fn a_signal_death_exits_128_plus_the_signal() {
    let out = floor(&["--", "sh", "-c", "kill -TERM $$"]);
    assert_eq!(out.status.code(), Some(128 + 15));
}

#[test]
fn stdout_and_stderr_arrive_on_their_own_streams() {
    let out = floor(&["--", "sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
}

#[test]
fn a_mebibyte_on_each_stream_at_once_arrives_intact() {
    // Both streams are written at the same time, each far past a pipe's buffer: a pass-through
    // that drained one before the other would deadlock here.
    let out = floor(&[
        "--",
        "sh",
        "-c",
        "(yes err | head -c 1048576) >&2 & yes out | head -c 1048576; wait",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout.len(), 1 << 20);
    assert_eq!(out.stderr.len(), 1 << 20);
    assert!(out.stdout.chunks(4).all(|c| b"out\n".starts_with(c)));
    assert!(out.stderr.chunks(4).all(|c| b"err\n".starts_with(c)));
}

#[test]
fn a_missing_command_exits_127() {
    let out = floor(&["--", "taskslash-floor-no-such-command"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot start"));
}

#[test]
fn no_command_exits_127() {
    assert_eq!(floor(&[]).status.code(), Some(127));
    assert_eq!(floor(&["--"]).status.code(), Some(127));
}

#[test]
fn the_taskslash_row_is_skipped_when_no_daemon_answers() {
    // `false status --json` exits 1, as `taskslash status --json` does with no daemon.
    let out = Command::new(BENCH)
        .args(["--runs", "3", "--taskslash", "false"])
        .stdin(Stdio::null())
        .output()
        .expect("floor-bench runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{stdout}");
    assert!(lines[0].starts_with("machine: load "), "{}", lines[0]);
    let lines = &lines[1..];
    for (line, name) in lines.iter().zip(["bare", "floor"]) {
        assert!(line.starts_with(name), "{line}");
        for field in ["min", "median", "p95", "p99", "max"] {
            assert!(line.contains(field), "{line}");
        }
    }
    assert!(lines[2].starts_with("taskslash"), "{}", lines[2]);
    assert!(
        lines[2].contains("skipped: daemon not answering"),
        "{}",
        lines[2]
    );
    let json = lines[3];
    assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
    for key in [
        "\"runs\":3",
        "\"floor_version\":",
        "\"taskslash_version\":null",
        "\"commit\":",
        "\"load_1m\":",
        "\"cpu_pressure_some_avg10\":",
        "\"busy\":",
        "\"skipped\":\"skipped: daemon not answering\"",
    ] {
        assert!(json.contains(key), "{key} not in {json}");
    }
}

#[test]
fn the_taskslash_row_breaks_its_time_into_the_phases_each_run_reports() {
    // A stand-in for `taskslash` that answers `status` and reports fixed phases from `run`, only
    // when asked to (TASKSLASH_PHASES).
    let dir = std::env::temp_dir().join(format!("floor-phases-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("taskslash");
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'taskslash 9.9.9' ;;\nrun) [ -n \"$TASKSLASH_PHASES\" ] && \
         echo 'taskslash: phases connect=0.5 admit=1 launch=2 command=3 drain=4 report=5 ms' >&2 ;;\nesac\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = Command::new(BENCH)
        .args(["--runs", "2", "--taskslash"])
        .arg(&fake)
        .stdin(Stdio::null())
        .output()
        .expect("floor-bench runs");
    std::fs::remove_dir_all(&dir).unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[3].starts_with("taskslash "), "{stdout}");
    for (line, (name, ms)) in lines[4..10].iter().zip([
        ("connect", "0.500"),
        ("admit", "1.000"),
        ("launch", "2.000"),
        ("command", "3.000"),
        ("drain", "4.000"),
        ("report", "5.000"),
    ]) {
        assert!(line.trim_start().starts_with(name), "{line}");
        assert!(line.contains(&format!("median {ms:>8}")), "{line}");
    }
    assert!(lines[10].trim_start().starts_with("other"), "{stdout}");
    let json = lines[11];
    assert!(
        json.contains("\"taskslash_version\":\"taskslash 9.9.9\""),
        "{json}"
    );
    assert!(
        json.contains("\"phases\":{\"connect\":{\"min_ms\":0.500,\"median_ms\":0.500"),
        "{json}"
    );
    assert!(json.contains("\"other\":{"), "{json}");
}
