//! `taskslash status` through a real `taskslash daemon` with its own socket and database. The
//! machine differs between this desktop and CI (CPU count, disk names, mounts), so the test asserts
//! the shape and the ranges, never one machine's values.

use serde_json::Value;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_taskslash");

/// A daemon on a socket and database in a directory of its own; killed and removed on drop.
struct Daemon {
    dir: PathBuf,
    child: Child,
}

impl Daemon {
    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tks-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn command(dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env("TASKSLASH_SOCKET", dir.join("d.sock"))
            .env("TASKSLASH_DB", dir.join("runs.db"))
            .stdin(Stdio::null());
        command
    }

    fn start() -> Self {
        let dir = Self::dir("daemon");
        let log = fs::File::create(dir.join("daemon.log")).unwrap();
        let mut child = Self::command(&dir, &["daemon"])
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

    fn run(&self, args: &[&str]) -> Output {
        Self::command(&self.dir, args)
            .output()
            .expect("the binary runs")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn number(value: &Value, path: &str) -> f64 {
    value
        .pointer(path)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("{path} is not a number in {value}"))
}

fn assert_share(value: &Value, path: &str) {
    let share = number(value, path);
    assert!((0.0..=1.0).contains(&share), "{path} is {share}");
}

fn assert_pressure(value: &Value, path: &str) {
    for field in ["some_avg10", "some_avg60", "full_avg10", "full_avg60"] {
        let p = number(value, &format!("{path}/{field}"));
        assert!((0.0..=100.0).contains(&p), "{path}/{field} is {p}");
    }
}

#[test]
fn status_json_has_every_field_in_range() {
    let daemon = Daemon::start();
    // Two samples after the first, so the busy shares are deltas.
    thread::sleep(Duration::from_millis(2500));
    let out = daemon.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "not one line: {text}");
    let status: Value = serde_json::from_str(&text).expect("status prints JSON");
    eprintln!("status: {status}");

    assert_eq!(status["type"], "status");
    let d = &status["daemon"];
    assert!(
        d["version"]
            .as_str()
            .is_some_and(|v| v.starts_with("0.1.0 (")),
        "{d}"
    );
    assert!(
        d["started_at"].as_str().is_some_and(|s| s.ends_with('Z')),
        "{d}"
    );
    assert!(d["pid"].as_u64().is_some_and(|p| p > 0), "{d}");
    assert_eq!(status["jobs"], serde_json::json!([]));
    assert_eq!(status["waiting"], serde_json::json!([]));
    let m = &status["machine"];
    assert!(
        m["sampled_at"].as_str().is_some_and(|s| s.ends_with('Z')),
        "{m}"
    );

    let threads = number(m, "/cpu/threads");
    assert!(threads >= 1.0, "{m}");
    assert!(threads <= number(m, "/cpu/threads_online"), "{m}");
    assert_share(m, "/cpu/busy");
    for resource in ["cpu", "memory", "io"] {
        assert_pressure(m, &format!("/{resource}/pressure"));
    }

    assert!(
        number(m, "/memory/available_mib") <= number(m, "/memory/total_mib"),
        "{m}"
    );
    assert!(
        number(m, "/memory/swap_free_mib") <= number(m, "/memory/swap_total_mib"),
        "{m}"
    );

    for (i, disk) in m["io"]["disks"]
        .as_array()
        .expect("disks")
        .iter()
        .enumerate()
    {
        assert!(disk["name"].is_string(), "disk {i}: {disk}");
        assert_share(disk, "/busy");
        assert!(number(disk, "/write_mib_s") >= 0.0, "{disk}");
    }

    let mounts = m["mounts"].as_array().expect("mounts");
    assert!(!mounts.is_empty(), "{m}");
    for mount in mounts {
        assert!(mount["path"].is_string(), "{mount}");
        assert!(
            number(mount, "/free_mib") <= number(mount, "/total_mib"),
            "{mount}"
        );
        assert_share(mount, "/used");
    }

    let load = m["load"].as_array().expect("load");
    assert_eq!(load.len(), 3, "{m}");
    assert!(
        load.iter().all(|l| l.as_f64().is_some_and(|l| l >= 0.0)),
        "{m}"
    );

    // The plain form answers from the same snapshot.
    let out = daemon.run(&["status"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("daemon    0.1.0 ("), "{text}");
    assert!(text.contains("\ncpu "), "{text}");
    assert!(text.contains("\npressure "), "{text}");
    assert!(text.contains("\nmount "), "{text}");
}

#[test]
fn status_with_no_daemon_exits_1_as_run_does() {
    let dir = Daemon::dir("none");
    let status = Daemon::command(&dir, &["status"]).output().unwrap();
    let run = Daemon::command(&dir, &["run", "--", "true"])
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(status.status.code(), Some(1), "{status:?}");
    assert_eq!(status.stderr, run.stderr);
    assert!(String::from_utf8_lossy(&status.stderr).contains("no daemon at"));
}
