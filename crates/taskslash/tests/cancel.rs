//! `taskslash cancel` through a real `taskslash daemon` and real `taskslash run`s, on whichever launch
//! path the daemon chose (`scope` with a systemd user session, `plain` without). The fake machine
//! keeps admission off this machine's real pressure and use; a second never-seen job always waits behind the first.

use serde_json::Value;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod common;
#[path = "common/session.rs"]
mod session;

const BIN: &str = env!("CARGO_BIN_EXE_taskslash");

/// A daemon on a socket and database in a directory of its own; killed and removed on drop.
struct Daemon {
    dir: PathBuf,
    child: Child,
}

impl Daemon {
    fn command(dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env("TASKSLASH_SOCKET", dir.join("d.sock"))
            .env("TASKSLASH_DB", dir.join("runs.db"))
            .env("TASKSLASH_FAKE_MACHINE", dir.join("machine.json"))
            .stdin(Stdio::null());
        command
    }

    fn start(tag: &str) -> Self {
        Self::start_on(tag, None)
    }

    /// A daemon on the launch path `TASKSLASH_LAUNCH` names (`None`: whichever the machine has).
    fn start_on(tag: &str, launch: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!("tkc-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        common::write_machine(&dir.join("machine.json"), 0.0);
        let log = fs::File::create(dir.join("daemon.log")).unwrap();
        let mut command = Self::command(&dir, &["daemon"]);
        match launch {
            Some(launch) => command.env("TASKSLASH_LAUNCH", launch),
            None => command.env_remove("TASKSLASH_LAUNCH"),
        };
        let mut child = command
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("the daemon starts");
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(dir.join("d.sock")).is_err() {
            if let Some(status) = child.try_wait().unwrap() {
                let log = fs::read_to_string(dir.join("daemon.log")).unwrap_or_default();
                panic!("the daemon exited early: {status}: {log}");
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not accept in 5 s"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Self { dir, child }
    }

    /// Starts `taskslash run --name <name> -- sh -c <script>`.
    fn spawn(&self, name: &str, script: &str) -> Child {
        Self::command(
            &self.dir,
            &["run", "--name", name, "--", "sh", "-c", script],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("taskslash run starts")
    }

    fn run(&self, args: &[&str]) -> Output {
        Self::command(&self.dir, args).output().unwrap()
    }

    fn status(&self) -> Value {
        let out = self.run(&["status", "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Polls `status --json` every 50 ms until `found` returns something, for at most 5 s.
    fn wait_for<T>(&self, found: impl Fn(&Value) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.status();
            if let Some(value) = found(&status) {
                return value;
            }
            assert!(Instant::now() < deadline, "gave up waiting: {status}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The number of the running job named `name`, once it runs.
    fn running(&self, name: &str) -> u64 {
        self.wait_for(|s| job_in(&s["jobs"], name))
    }

    /// The number of the waiting job named `name`, once it waits.
    fn waiting(&self, name: &str) -> u64 {
        self.wait_for(|s| job_in(&s["waiting"], name))
    }

    fn cancel(&self, job: u64, extra: &[&str]) -> (Option<i32>, String, String) {
        let number = job.to_string();
        let mut args = vec!["cancel", number.as_str()];
        args.extend_from_slice(extra);
        let out = self.run(&args);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// The exit code the newest run of `key` was recorded with.
    fn recorded_exit(&self, key: &str) -> Value {
        let out = self.run(&["history", key, "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let history: Value = serde_json::from_slice(&out.stdout).unwrap();
        history["runs"][0]["exit_code"].clone()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn job_in(list: &Value, name: &str) -> Option<u64> {
    list.as_array()?
        .iter()
        .find(|j| j["key"] == name)
        .and_then(|j| j["job"].as_u64())
}

/// Waits for `child` to end, for at most `secs`; its exit code and stderr.
fn ended(mut child: Child, secs: u64) -> (Option<i32>, String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the run did not end in {secs} s");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_cancelled_waiting_job_leaves_the_queue_and_its_client_exits_1() {
    let daemon = Daemon::start("waiting");
    let holder = daemon.spawn("holder", "sleep 600");
    let held = daemon.running("holder");
    let waiter = daemon.spawn("waiter", "echo ran");
    let job = daemon.waiting("waiter");

    let (code, stdout, stderr) = daemon.cancel(job, &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(stdout, format!("job {job} cancelled (was waiting)\n"));

    let (code, stderr) = ended(waiter, 5);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.ends_with(&format!("taskslash: job {job} cancelled while waiting\n")),
        "{stderr}"
    );
    let status = daemon.status();
    assert_eq!(status["waiting"], serde_json::json!([]), "{status}");
    let why = daemon.run(&["why", &job.to_string()]);
    assert!(
        String::from_utf8_lossy(&why.stdout).contains("ended without running"),
        "{why:?}"
    );

    // The holder is stopped the same way, so the test leaves nothing behind.
    let (code, _, stderr) = daemon.cancel(held, &["--grace", "0"]);
    assert_eq!(code, Some(0), "{stderr}");
    let (code, _) = ended(holder, 5);
    assert_eq!(code, Some(137), "{}", daemon.log());
}

#[test]
fn a_cancelled_running_job_ends_143_and_is_recorded() {
    let daemon = Daemon::start("running");
    let run = daemon.spawn("demo", "sleep 600");
    let job = daemon.running("demo");

    let (code, stdout, stderr) = daemon.cancel(job, &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "job {job} cancelled (was running; SIGTERM sent, SIGKILL in 10s if it has not ended)\n"
        )
    );
    let (code, stderr) = ended(run, 5);
    assert_eq!(code, Some(143), "{stderr}\n{}", daemon.log());
    assert!(
        stderr.contains(&format!("taskslash: job {job} cancelled\n")),
        "{stderr}"
    );
    assert_eq!(daemon.recorded_exit("demo"), 143);

    // Ended now: a cancel says how, and where to look.
    let (code, _, stderr) = daemon.cancel(job, &[]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with(&format!(
            "taskslash: job {job} is not waiting or running\n  job {job} ended: run "
        )),
        "{stderr}"
    );
    assert!(stderr.contains(", exit 143 "), "{stderr}");
    assert!(
        stderr.ends_with("\n  (taskslash status lists jobs)\n"),
        "{stderr}"
    );
}

#[test]
fn a_command_that_ignores_sigterm_is_killed_after_the_grace_and_a_second_cancel_says_so() {
    let daemon = Daemon::start("stubborn");
    let ready = daemon.dir.join("trapped");
    let script = format!(
        "trap '' TERM; touch {}; while :; do sleep 0.1; done",
        ready.display()
    );
    let run = daemon.spawn("stubborn", &script);
    let job = daemon.running("stubborn");
    // Cancelled only once the trap is set, or SIGTERM would end it first.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the trap was never set");
        thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    let (code, stdout, stderr) = daemon.cancel(job, &["--grace", "1"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("SIGKILL in 1s"), "{stdout}");
    let (code, stdout, stderr) = daemon.cancel(job, &[]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        stdout,
        format!("job {job} is already being cancelled (it was running)\n")
    );

    let (code, stderr) = ended(run, 5);
    assert_eq!(code, Some(137), "{stderr}\n{}", daemon.log());
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "killed before its grace"
    );
    assert_eq!(daemon.recorded_exit("stubborn"), 137);
}

#[test]
fn cancelling_a_job_the_daemon_does_not_hold_exits_1() {
    let daemon = Daemon::start("unknown");
    let (code, stdout, stderr) = daemon.cancel(999, &[]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "taskslash: job 999 is not waiting or running\n  (taskslash status lists jobs)\n"
    );
}

/// Whether `pid` has ended: no such process, a zombie its parent has yet to reap, or one being
/// reaped (`X`, for the moment between the two).
fn gone(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with(['Z', 'X', 'x']))
    })
}

/// A `taskslash run` killed before its command ends: the daemon stops the command (the shell and
/// the `sleep` it started) and records the run with no exit code. On the scope path it also stops a
/// `sleep` that left the command's process group, which only the scope still holds.
fn a_run_whose_client_is_killed_has_its_command_stopped(launch: Option<&str>) {
    let daemon = Daemon::start_on(&format!("orphan-{}", launch.unwrap_or("default")), launch);
    let shell = daemon.dir.join("shell");
    let sleep = daemon.dir.join("sleep");
    let escaped = daemon.dir.join("escaped");
    // The escaped sleep writes its own pid once it has left the group, so a stop that comes as soon
    // as all three pids are known cannot catch it still in the group.
    let script = format!(
        "setsid sh -c 'echo $$ > {}; exec sleep 600' & sleep 600 & echo $! > {}; echo $$ > {}; wait",
        escaped.display(),
        sleep.display(),
        shell.display()
    );
    let mut run = daemon.spawn("orphan", &script);
    daemon.running("orphan");
    let deadline = Instant::now() + Duration::from_secs(5);
    let pids = loop {
        let read = |path: &Path| fs::read_to_string(path).ok().map(|s| s.trim().to_owned());
        if let (Some(shell), Some(sleep), Some(escaped)) =
            (read(&shell), read(&sleep), read(&escaped))
            && !shell.is_empty()
            && !sleep.is_empty()
            && !escaped.is_empty()
        {
            break [shell, sleep, escaped];
        }
        assert!(Instant::now() < deadline, "the command never started");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(pids.iter().all(|pid| !gone(pid)), "{pids:?}");

    run.kill().unwrap();
    run.wait().unwrap();

    daemon.wait_for(|s| {
        s["jobs"]
            .as_array()
            .is_some_and(Vec::is_empty)
            .then_some(())
    });
    let [.., escaped] = &pids;
    if session::expected_launch(launch) == "plain" {
        // Out of the group, so out of reach without a scope: the test stops it.
        assert!(!gone(escaped), "the escaped sleep ended on the plain path");
        let _ = Command::new("kill").arg(escaped).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone(escaped) {
            assert!(Instant::now() < deadline, "the escaped sleep outlived kill");
            thread::sleep(Duration::from_millis(20));
        }
    }
    for pid in &pids {
        assert!(
            gone(pid),
            "process {pid} outlived its client\n{}",
            daemon.log()
        );
    }
    assert_eq!(daemon.recorded_exit("orphan"), Value::Null);
    assert!(
        daemon
            .log()
            .contains("its client went away; its command was stopped with SIGTERM"),
        "{}",
        daemon.log()
    );
}

#[test]
fn a_run_whose_client_is_killed_has_its_command_stopped_on_the_machines_path() {
    a_run_whose_client_is_killed_has_its_command_stopped(None);
}

#[test]
fn a_run_whose_client_is_killed_has_its_command_stopped_on_the_plain_path() {
    a_run_whose_client_is_killed_has_its_command_stopped(Some("plain"));
}
