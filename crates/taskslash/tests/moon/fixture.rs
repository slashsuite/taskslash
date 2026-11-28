//! The fixture workspace for `tests/moon.rs`: a temp dir with the daemon's socket and database
//! and a fresh git repository copied from `tests/fixtures/moon`, and the daemon on it.

use super::common;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_taskslash");
const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// A directory of its own under the temp dir, holding the socket, the database and a fresh git
/// repository copied from the fixture. Removed on drop.
pub struct Scratch {
    pub dir: PathBuf,
    socket: PathBuf,
    pub workspace: PathBuf,
}

impl Scratch {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tksm-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("d.sock");
        assert!(socket.as_os_str().len() < 108, "{}", socket.display());
        common::write_machine(&dir.join("machine.json"), 0.0);
        let workspace = dir.join("fixture");
        copy(&Path::new(MANIFEST).join("tests/fixtures/moon"), &workspace);
        let bin = workspace.join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let moon = Path::new(MANIFEST).join("../../node_modules/.bin/moon");
        assert!(
            moon.exists(),
            "run bun install first: no {}",
            moon.display()
        );
        std::os::unix::fs::symlink(fs::canonicalize(moon).unwrap(), bin.join("moon")).unwrap();
        fs::write(
            workspace.join(".gitignore"),
            "node_modules/\n.moon/cache/\n",
        )
        .unwrap();
        git(&workspace, &["init", "-q", "-b", "main"]);
        git(&workspace, &["add", "-A"]);
        git(&workspace, &["commit", "-q", "-m", "fixture"]);
        Self {
            dir,
            socket,
            workspace,
        }
    }

    /// A command in the workspace, with nothing of an outer moon run (this test may run as a
    /// moon task) leaking into the inner one.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("TASKSLASH_SOCKET", &self.socket)
            .env("TASKSLASH_DB", self.dir.join("runs.db"))
            .env("TASKSLASH_FAKE_MACHINE", self.dir.join("machine.json"))
            .env_remove("TASKSLASH_CLASS")
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("MOON_") {
                command.env_remove(name);
            }
        }
        command
    }

    pub fn taskslash(&self, args: &[&str]) -> Output {
        self.command(BIN, args).output().expect("the binary runs")
    }

    pub fn daemon(self) -> Daemon {
        self.daemon_on(None)
    }

    /// The daemon on the launch path `TASKSLASH_LAUNCH` names (`None`: whichever the machine has).
    pub fn daemon_on(self, launch: Option<&str>) -> Daemon {
        let mut command = self.command(BIN, &["daemon"]);
        match launch {
            Some(launch) => command.env("TASKSLASH_LAUNCH", launch),
            None => command.env_remove("TASKSLASH_LAUNCH"),
        };
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon starts");
        let mut daemon = Daemon {
            child,
            scratch: self,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&daemon.scratch.socket).is_err() {
            if let Some(status) = daemon.child.try_wait().unwrap() {
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub struct Daemon {
    child: Child,
    pub scratch: Scratch,
}

impl Daemon {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}
