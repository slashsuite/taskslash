//! Output capture in `taskslash run` and `taskslash logs`, through a real daemon, each test with
//! its own socket, database and log directory. The terminal and signal tests run on both launch
//! paths: the one this machine has (the scope path where there is a systemd user session, which CI
//! starts) and the plain path (`TASKSLASH_LAUNCH=plain`). Each asserts the path its run recorded.

use serde_json::Value;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

mod common;
#[path = "common/session.rs"]
mod session;

const BIN: &str = env!("CARGO_BIN_EXE_taskslash");

/// The launch paths every terminal and signal test runs on: the machine's own, and plain.
const LAUNCHES: [Option<&str>; 2] = [None, Some("plain")];

/// A directory of its own under the temp dir: socket, database, logs. Removed on drop.
struct Scratch {
    dir: PathBuf,
    launch: Option<&'static str>,
}

impl Scratch {
    fn new(launch: Option<&'static str>) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tkl-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        common::write_machine(&dir.join("machine.json"), 0.0);
        Self { dir, launch }
    }

    fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(&self.dir)
            .env("TASKSLASH_SOCKET", self.dir.join("d.sock"))
            .env("TASKSLASH_DB", self.dir.join("runs.db"))
            .env("TASKSLASH_FAKE_MACHINE", self.dir.join("machine.json"))
            .env_remove("TASKSLASH_LOGS")
            .env_remove("TASKSLASH_LOG_DAYS")
            .env_remove("TASKSLASH_LOG_MIB")
            .env_remove("TASKSLASH_CLASS")
            .stdin(Stdio::null());
        match self.launch {
            Some(launch) => command.env("TASKSLASH_LAUNCH", launch),
            None => command.env_remove("TASKSLASH_LAUNCH"),
        };
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("the binary runs")
    }

    /// Starts `taskslash daemon`, with `env` set, and waits up to 5 s for it to accept.
    fn daemon(&self, env: &[(&str, &str)]) -> Daemon {
        let child = self
            .command(&["daemon"])
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon starts");
        let mut daemon = Daemon(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(self.dir.join("d.sock")).is_err() {
            if let Some(status) = daemon.0.try_wait().unwrap() {
                panic!("the daemon exited early: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not accept in 5 s"
            );
            thread::sleep(Duration::from_millis(20));
        }
        daemon
    }

    /// The runs of `key`, newest first.
    fn history(&self, key: &str) -> Vec<Value> {
        let out = self.run(&["history", key, "--json"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let history: Value = serde_json::from_slice(&out.stdout).unwrap();
        history["runs"].as_array().unwrap().clone()
    }

    /// The newest run of `key`: its id, its launch path and its log's contents.
    fn last(&self, key: &str) -> (i64, String, Vec<u8>) {
        let runs = self.history(key);
        let run = &runs[0];
        let path = run["log_path"].as_str().expect("a log path");
        assert_eq!(Path::new(path).parent(), Some(self.logs().as_path()));
        (
            run["id"].as_i64().unwrap(),
            run["launch"].as_str().unwrap().to_owned(),
            fs::read(path).unwrap(),
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A running daemon, killed on drop. Declare it after its scratch, so it is dropped first.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn stdout_and_stderr_reach_the_callers_streams_and_the_log() {
    let scratch = Scratch::new(None);
    let _daemon = scratch.daemon(&[]);
    let out = scratch.run(&["run", "--", "sh", "-c", "echo out; echo err >&2"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        (text(&out.stdout), text(&out.stderr)),
        ("out\n".into(), "err\n".into())
    );
    let (_, ran, log) = scratch.last("echo out;");
    assert_eq!(ran, session::expected_launch(None));
    let mut lines: Vec<String> = text(&log).lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(lines, ["err", "out"]);

    // Byte for byte, binary and larger than a pipe's buffer.
    let script = "head -c 300000 /dev/urandom > blob; cat blob; cat blob >&2";
    let out = scratch.run(&["run", "--name", "blob", "--", "sh", "-c", script]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let blob = fs::read(scratch.dir.join("blob")).unwrap();
    assert!(
        out.stdout == blob && out.stderr == blob,
        "the caller got every byte"
    );
    let (_, _, log) = scratch.last("blob");
    assert_eq!(log.len(), 2 * blob.len());

    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scratch.logs()), 0o700);
    for entry in fs::read_dir(scratch.logs()).unwrap() {
        assert_eq!(mode(&entry.unwrap().path()), 0o600);
    }
}

/// A pseudo-terminal for the test to be the caller's terminal: (master, slave).
fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes two fds; null settings and size take the defaults.
    let ok = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(ok, 0, "openpty");
    // SAFETY: two fresh fds nothing else owns.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// The settings of the terminal on `fd` that a run must leave as it found them.
fn settings(fd: &OwnedFd) -> (u32, u32, u32, u32, Vec<u8>) {
    // SAFETY: tcgetattr writes one termios.
    let t = unsafe {
        let mut t = std::mem::zeroed::<libc::termios>();
        assert_eq!(libc::tcgetattr(fd.as_raw_fd(), &raw mut t), 0);
        t
    };
    (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_cc.to_vec())
}

/// `command` with the pseudo-terminal `slave` as its controlling terminal and its stdio.
fn on_terminal(command: &mut Command, slave: &OwnedFd) {
    for set in [Command::stdin, Command::stdout, Command::stderr] {
        set(command, Stdio::from(slave.try_clone().unwrap()));
    }
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Reads `master` on a thread until the terminal's other end is closed.
fn drain(master: OwnedFd) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut file = fs::File::from(master);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = file.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    })
}

#[test]
fn under_a_terminal_the_command_sees_one_and_the_terminal_is_put_back() {
    for launch in LAUNCHES {
        let scratch = Scratch::new(launch);
        let _daemon = scratch.daemon(&[]);
        let (master, slave) = open_pty();
        let before = settings(&slave);
        let script = "test -t 1 && echo stdout-is-a-tty; printf '\\033[31mred\\033[0m\\n'";
        let mut command = scratch.command(&["run", "--", "sh", "-c", script]);
        on_terminal(&mut command, &slave);
        let reading = drain(master);
        let status = command.status().unwrap();
        // It holds copies of the terminal, which would keep `reading` from ever ending.
        drop(command);
        assert_eq!(status.code(), Some(0), "{launch:?}");
        assert_eq!(
            settings(&slave),
            before,
            "{launch:?}: the terminal is as it was"
        );
        drop(slave);
        let seen = text(&reading.join().unwrap());
        assert!(seen.contains("stdout-is-a-tty"), "{launch:?}: {seen:?}");
        assert!(seen.contains("\u{1b}[31mred"), "{launch:?}: {seen:?}");
        let (_, ran, log) = scratch.last("test -t");
        assert_eq!(ran, session::expected_launch(launch), "{launch:?}");
        let log = text(&log);
        assert!(log.contains("stdout-is-a-tty"), "{ran}: {log:?}");
        assert!(log.contains("\u{1b}[31mred"), "{ran}: {log:?}");
    }
}

/// Waits up to 10 s for a `sleep` whose parent is `pid`: the command has started.
fn wait_for_sleep(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            // `<pid> (<comm>) <state> <ppid> ...`
            let Some((comm, rest)) = stat.split_once(") ") else {
                return false;
            };
            comm.ends_with("(sleep")
                && rest.split_whitespace().nth(1) == Some(pid.to_string().as_str())
        });
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the command did not start in 10 s"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigint_to_the_client_reaches_the_command_on_pipes_and_on_a_terminal() {
    for launch in LAUNCHES {
        for terminal in [false, true] {
            let scratch = Scratch::new(launch);
            let _daemon = scratch.daemon(&[]);
            let script = "echo started; exec sleep 30";
            let mut command = scratch.command(&["run", "--", "sh", "-c", script]);
            let (reading, slave) = match terminal.then(open_pty) {
                Some((master, slave)) => {
                    on_terminal(&mut command, &slave);
                    (Some(drain(master)), Some(slave))
                }
                None => {
                    command.stdout(Stdio::null()).stderr(Stdio::null());
                    (None, None)
                }
            };
            let mut child = command.spawn().unwrap();
            drop(command);
            wait_for_sleep(child.id());
            let sent = Instant::now();
            // SAFETY: signals our own child.
            unsafe { libc::kill(i32::try_from(child.id()).unwrap(), libc::SIGINT) };
            let status = child.wait().unwrap();
            let took = sent.elapsed();
            let case = format!("{launch:?}, terminal {terminal}");
            assert_eq!(
                status.code(),
                Some(130),
                "{case}: {status:?} {:?}",
                status.signal()
            );
            assert!(took < Duration::from_secs(2), "{case}: took {took:?}");
            drop(slave);
            if let Some(reading) = reading {
                reading.join().unwrap();
            }
            let (_, ran, log) = scratch.last("echo started;");
            assert_eq!(ran, session::expected_launch(launch), "{case}");
            assert!(text(&log).contains("started"), "{case}: {log:?}");
        }
    }
}

#[test]
fn logs_picks_a_run_by_id_newest_and_newest_failed() {
    let scratch = Scratch::new(None);
    let _daemon = scratch.daemon(&[]);
    let run = |name: &str, script: &str| {
        scratch.run(&["run", "--name", name, "--", "sh", "-c", script]);
    };
    run("k", "echo one");
    run("k", "echo two; exit 3");
    run("other", "echo three");
    run("signalled", "echo four; kill -INT $$");
    run("k", "echo five");
    let ids: Vec<i64> = scratch
        .history("k")
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    let logs = |args: &[&str]| {
        let mut all = vec!["logs"];
        all.extend_from_slice(args);
        let out = scratch.run(&all);
        (out.status.code(), text(&out.stdout))
    };
    let ok = |s: &str| (Some(0), s.to_owned());
    let refused = |args: &[&str], s: &str| {
        let mut all = vec!["logs"];
        all.extend_from_slice(args);
        let out = scratch.run(&all);
        assert_eq!(
            (out.status.code(), text(&out.stdout), text(&out.stderr)),
            (Some(1), String::new(), format!("taskslash: {s}\n"))
        );
    };
    assert_eq!(logs(&["--run", &ids[2].to_string()]), ok("one\n"));
    assert_eq!(logs(&["--last", "k"]), ok("five\n"));
    // Every run here was submitted from the scratch directory, so this worktree's newest is the
    // machine's.
    assert_eq!(logs(&["--failed"]), ok("four\n"), "a signal's 128+N counts");
    assert_eq!(logs(&["--failed", "k"]), ok("two\n"));
    refused(
        &["--failed", "other"],
        &format!(
            "no failed run of other in {}; taskslash logs --failed other --all searches the \
             whole machine",
            scratch.dir.file_name().unwrap().to_str().unwrap()
        ),
    );
    refused(&["--last", "nope"], "no run of nope");
    refused(
        &["99"],
        "no job 99 (the daemon restarted or forgot it); find the run with taskslash history \
         <key>, then taskslash logs --run <id>",
    );
    refused(&["--run", "999"], "no run 999");
    assert_eq!(logs(&[]).0, Some(2), "a usage error");
    assert_eq!(logs(&["--all"]).0, Some(2), "--all only with --failed");
    assert_eq!(logs(&["--last", "k", "--all"]).0, Some(2));
}

#[test]
fn failed_shows_this_worktrees_failure_with_a_header_and_all_the_machines() {
    let scratch = Scratch::new(None);
    let _daemon = scratch.daemon(&[]);
    // Three worktrees: `.git` files, as `git worktree add` leaves them.
    let tree = |name: &str| {
        let dir = scratch.dir.join(name);
        fs::create_dir_all(dir.join("deep")).unwrap();
        fs::write(dir.join(".git"), "gitdir: /nowhere\n").unwrap();
        dir.join("deep")
    };
    let (mine, theirs, clean) = (tree("mine"), tree("theirs"), tree("clean"));
    let in_dir = |dir: &Path, args: &[&str]| {
        let out = scratch.command(args).current_dir(dir).output().unwrap();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    };
    in_dir(
        &mine,
        &["run", "--name", "k", "--", "sh", "-c", "echo mine; exit 3"],
    );
    in_dir(
        &theirs,
        &[
            "run",
            "--name",
            "j",
            "--",
            "sh",
            "-c",
            "echo theirs; exit 4",
        ],
    );
    in_dir(
        &clean,
        &["run", "--name", "k", "--", "sh", "-c", "echo fine"],
    );
    let mine_id = scratch.history("k")[1]["id"].as_i64().unwrap();
    let theirs_id = scratch.history("j")[0]["id"].as_i64().unwrap();

    let (code, out, err) = in_dir(&mine, &["logs", "--failed"]);
    assert_eq!((code, out.as_str()), (Some(0), "mine\n"), "{err}");
    let header = format!("run {mine_id} k in mine, exit 3, ");
    assert!(err.starts_with(&header), "{err:?}");
    // `YYYY-MM-DD HH:MM`, local time, and nothing else on stderr.
    assert_eq!(
        err.len(),
        header.len() + "2026-11-17 20:22\n".len(),
        "{err:?}"
    );

    let (code, out, err) = in_dir(&mine, &["logs", "--failed", "--all"]);
    assert_eq!((code, out.as_str()), (Some(0), "theirs\n"), "{err}");
    assert!(
        err.starts_with(&format!("run {theirs_id} j in theirs, exit 4, ")),
        "{err:?}"
    );

    let (code, out, err) = in_dir(&clean, &["logs", "--failed"]);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert_eq!(
        err,
        "taskslash: no failed run in clean; taskslash logs --failed --all searches the whole \
         machine\n"
    );
}

#[test]
fn an_ended_jobs_log_is_found_by_its_number_until_the_daemon_restarts() {
    let scratch = Scratch::new(None);
    {
        let _daemon = scratch.daemon(&[]);
        scratch.run(&["run", "--name", "k", "--", "sh", "-c", "echo first; exit 2"]);
        scratch.run(&["run", "--name", "k", "--", "sh", "-c", "echo second"]);
        let ids: Vec<i64> = scratch
            .history("k")
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        for (job, id, log, exit) in [(1, ids[1], "first\n", 2), (2, ids[0], "second\n", 0)] {
            let out = scratch.run(&["logs", &job.to_string()]);
            assert_eq!(out.status.code(), Some(0), "{out:?}");
            assert_eq!(text(&out.stdout), log);
            let header = format!("run {id} k in ");
            let err = text(&out.stderr);
            assert!(err.starts_with(&header), "{err:?}");
            assert!(err.contains(&format!(", exit {exit}, ")), "{err:?}");
        }
    }
    // A restarted daemon has forgotten them, and says where to look instead.
    let _daemon = scratch.daemon(&[]);
    let out = scratch.run(&["logs", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(&out.stderr),
        "taskslash: no job 1 (the daemon restarted or forgot it); find the run with taskslash \
         history <key>, then taskslash logs --run <id>\n"
    );
}

/// The running jobs' numbers, from `status --json`.
fn running_jobs(scratch: &Scratch) -> Vec<u64> {
    let out = scratch.run(&["status", "--json"]);
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    status["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| {
            assert!(j["log_path"].as_str().unwrap().ends_with(".log"), "{j}");
            j["job"].as_u64().unwrap()
        })
        .collect()
}

#[test]
fn logs_follows_a_running_job_until_it_ends() {
    let scratch = Scratch::new(None);
    let _daemon = scratch.daemon(&[]);
    // It ends only once the test creates `go`, so `-f` always meets it running.
    let script = "echo one; while [ ! -e go ]; do sleep 0.05; done; echo two";
    let mut job = scratch
        .command(&["run", "--", "sh", "-c", script])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let n = loop {
        if let Some(&n) = running_jobs(&scratch).first() {
            break n;
        }
        assert!(Instant::now() < deadline, "the job did not start");
        thread::sleep(Duration::from_millis(20));
    };
    // While it runs, `--last` picks it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !text(&scratch.run(&["logs", "--last", "echo one;"]).stdout).contains("one") {
        assert!(
            Instant::now() < deadline,
            "--last did not pick the running job"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Its header says it runs, and since when.
    let err = text(&scratch.run(&["logs", &n.to_string()]).stderr);
    let dir = scratch.dir.file_name().unwrap().to_str().unwrap();
    assert!(
        err.starts_with(&format!("job {n} echo one; in {dir}, running since ")),
        "{err:?}"
    );
    let mut follow = scratch
        .command(&["logs", &n.to_string(), "-f"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // Once `-f` has printed the first line, it has found the job running: let the job end.
    let mut stdout = follow.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 64];
    while !seen.ends_with(b"one\n") {
        let n = stdout.read(&mut buf).unwrap();
        assert!(n > 0, "-f ended early: {seen:?}");
        seen.extend_from_slice(&buf[..n]);
    }
    fs::write(scratch.dir.join("go"), "").unwrap();
    stdout.read_to_end(&mut seen).unwrap();
    assert_eq!(follow.wait().unwrap().code(), Some(0));
    assert_eq!(text(&seen), "one\ntwo\n");
    assert_eq!(job.wait().unwrap().code(), Some(0));
    // Ended, it is still found by its number, now as its recorded run.
    let after = scratch.run(&["logs", &n.to_string()]);
    assert_eq!(after.status.code(), Some(0), "{after:?}");
    assert_eq!(text(&after.stdout), "one\ntwo\n");
    assert!(text(&after.stderr).starts_with("run "), "{after:?}");
}

#[test]
fn a_run_from_before_logs_has_none_and_an_expired_log_says_so() {
    let scratch = Scratch::new(None);
    {
        // A database as it was before logs were kept, with one run.
        let conn = rusqlite::Connection::open(scratch.dir.join("runs.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (
                id INTEGER PRIMARY KEY, key TEXT NOT NULL, kind TEXT NOT NULL,
                argv TEXT NOT NULL, cwd TEXT NOT NULL, class TEXT NOT NULL,
                submitter TEXT NOT NULL, launch TEXT NOT NULL, started_at TEXT NOT NULL,
                wall_ms INTEGER NOT NULL, cpu_usec INTEGER, peak_cpu_5s REAL,
                peak_mem_bytes INTEGER, written_bytes INTEGER, exit_code INTEGER,
                machine_cpu_some REAL, machine_mem_some REAL, machine_io_some REAL,
                cached INTEGER, moon_hash TEXT
            );
            INSERT INTO runs (key, kind, argv, cwd, class, submitter, launch, started_at,
                wall_ms, exit_code)
            VALUES ('echo kept', 'command', '[\"sh\",\"-c\",\"echo kept\"]', '/w', 'normal', '/w',
                'plain', '2026-11-01T00:00:00.000Z', 5, 0);
            INSERT INTO runs (key, kind, argv, cwd, class, submitter, launch, started_at,
                wall_ms, exit_code)
            VALUES ('/w/build.sh --all', 'command', '[\"/w/build.sh\",\"--all\"]', '/w',
                'normal', '/w', 'plain', '2026-11-01T00:00:00.000Z', 5, 0);",
        )
        .unwrap();
    }
    {
        let _daemon = scratch.daemon(&[]);
        let out = scratch.run(&["logs", "--run", "1"]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert_eq!(text(&out.stderr), "taskslash: run 1 has no log\n");
        let out = scratch.run(&["run", "--", "sh", "-c", "echo kept"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let runs = scratch.history("echo kept");
        assert_eq!(runs.len(), 2, "the old row is intact");
        // A key recorded before keys were shaped is still found as it was typed.
        assert_eq!(scratch.history("/w/build.sh --all").len(), 1);
        assert!(runs[1]["log_path"].is_null());
        assert_eq!(text(&scratch.run(&["logs", "--run", "3"]).stdout), "kept\n");
    }
    // A daemon that keeps logs for no time deletes it at startup; the row keeps its path.
    let _daemon = scratch.daemon(&[("TASKSLASH_LOG_DAYS", "0")]);
    let out = scratch.run(&["logs", "--run", "3"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "taskslash: the log of run 3 has expired\n"
    );
    assert!(scratch.history("echo kept")[0]["log_path"].is_string());
}

#[test]
fn logs_lands_beside_the_database_unless_taskslash_logs_says_otherwise() {
    let scratch = Scratch::new(None);
    let elsewhere = scratch.dir.join("elsewhere");
    let elsewhere_text = elsewhere.to_str().unwrap();
    let _daemon = scratch.daemon(&[("TASKSLASH_LOGS", elsewhere_text)]);
    let out = scratch.run(&["run", "--", "sh", "-c", "echo moved"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let runs = scratch.history("echo moved");
    let path = runs[0]["log_path"].as_str().unwrap();
    assert!(Path::new(path).starts_with(&elsewhere), "{path}");
    assert_eq!(fs::read_to_string(path).unwrap(), "moved\n");
    assert!(!scratch.logs().exists());
}
