//! Stopping the command of a job whose client went away before it said how the command ended. The
//! client runs the command, so with the client gone nothing else would stop it: the daemon sends
//! what is left of it SIGTERM, then SIGKILL after a grace period, and ends the job only once it is
//! gone, so admission never counts a job as ended while its command still runs.

use crate::capture::tree;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

/// How long the command has after SIGTERM before SIGKILL, unless a cancel gave another grace.
pub(super) const GRACE: Duration = Duration::from_secs(10);
/// The longest grace a stop waits before SIGKILL, whatever a cancel asked for.
const MAX_GRACE: Duration = Duration::from_secs(3600);
/// How long the processes may take to go after SIGKILL before the daemon gives up on them.
const AFTER_KILL: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// What the daemon can reach of a job's command: every process in its scope, and the process the
/// client said it started (in case the client died before the scope was made).
#[derive(Debug, Default)]
pub(super) struct Orphan {
    /// The scope's cgroup directory.
    pub scope: Option<PathBuf>,
    pub process: Option<Started>,
}

/// The process the client started, with its start time, so a pid used again by another process is
/// never signalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Started {
    pid: libc::pid_t,
    since: Option<u64>,
    /// It leads a process group of its own (`taskslash run`): signal the group. Else (a
    /// `taskslash moon` target, in the client's group) signal it and its descendants.
    group: bool,
}

/// What became of the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stopped {
    /// Nothing of it was left.
    Gone,
    /// It ended after SIGTERM.
    Terminated,
    /// It ended after SIGKILL.
    Killed,
    /// Something of it was still there after SIGKILL.
    Survived,
}

impl Stopped {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Gone => "its command had already ended",
            Self::Terminated => "its command was stopped with SIGTERM",
            Self::Killed => "its command was stopped with SIGKILL",
            Self::Survived => "its command outlived SIGKILL",
        }
    }
}

/// Whether `pid` is still the process the client started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Still {
    Same,
    /// No such process, or a zombie.
    Gone,
    /// Another process has the pid now.
    Reused,
}

impl Started {
    /// The process `pid` the client reported, read now; `None` for a pid that cannot be one: init,
    /// or the daemon itself or its process group.
    pub(super) fn new(pid: i32, group: bool) -> Option<Self> {
        // SAFETY: getpid and getpgrp cannot fail.
        let (me, my_group) = unsafe { (libc::getpid(), libc::getpgrp()) };
        (pid > 1 && pid != me && pid != my_group).then(|| Self {
            pid,
            since: stat(pid).and_then(|s| s.since),
            group,
        })
    }

    fn still(&self) -> Still {
        match stat(self.pid) {
            None | Some(Stat { state: 'Z', .. }) => Still::Gone,
            Some(s) if s.since == self.since => Still::Same,
            Some(_) => Still::Reused,
        }
    }

    /// Whether something of the command is left to signal. A group counts only while its leader is
    /// the command's or a live process is still in it: once the group is empty its number is free.
    fn alive(&self) -> bool {
        match (self.group, self.still()) {
            (_, Still::Reused) => false,
            (true, Still::Same) => true,
            (true, Still::Gone) => group_has_members(self.pid),
            (false, still) => still == Still::Same,
        }
    }

    fn signal(&self, signal_number: libc::c_int) {
        if !self.alive() {
            return;
        }
        if self.group {
            signal(-self.pid, signal_number);
        } else {
            for pid in tree(self.pid) {
                signal(pid, signal_number);
            }
        }
    }
}

impl Orphan {
    /// Stops what is left of the command: SIGTERM, then SIGKILL once `grace` has passed (at once
    /// when it is zero), and waits for it to be gone.
    pub(super) fn stop(&self, grace: Duration) -> Stopped {
        // A cancel's grace is the user's number; past an hour the stop waits no longer for it.
        let grace = grace.min(MAX_GRACE);
        if !self.alive() {
            return Stopped::Gone;
        }
        if !grace.is_zero() {
            self.signal(libc::SIGTERM);
            if self.gone_within(grace) {
                return Stopped::Terminated;
            }
        }
        self.signal(libc::SIGKILL);
        if self.gone_within(AFTER_KILL) {
            Stopped::Killed
        } else {
            Stopped::Survived
        }
    }

    fn scope_pids(&self) -> Vec<libc::pid_t> {
        let Some(dir) = &self.scope else {
            return Vec::new();
        };
        fs::read_to_string(dir.join("cgroup.procs"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect()
    }

    fn alive(&self) -> bool {
        !self.scope_pids().is_empty() || self.process.as_ref().is_some_and(Started::alive)
    }

    fn signal(&self, signal_number: libc::c_int) {
        // cgroup.kill takes the whole scope at once, so a process forked mid-loop is not missed.
        let killed = signal_number == libc::SIGKILL
            && self
                .scope
                .as_ref()
                .is_some_and(|dir| fs::write(dir.join("cgroup.kill"), "1").is_ok());
        if !killed {
            for pid in self.scope_pids() {
                signal(pid, signal_number);
            }
        }
        if let Some(process) = &self.process {
            process.signal(signal_number);
        }
    }

    fn gone_within(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if !self.alive() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL);
        }
    }
}

/// Sends `signal_number` to `pid` (a negative one is a process group); whether it was delivered.
fn signal(pid: libc::pid_t, signal_number: libc::c_int) -> bool {
    // SAFETY: kill takes any pid and signal; a gone one is ESRCH.
    unsafe { libc::kill(pid, signal_number) == 0 }
}

/// What the daemon reads of `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    state: char,
    group: libc::pid_t,
    /// The start time, in clock ticks after boot.
    since: Option<u64>,
}

fn stat(pid: libc::pid_t) -> Option<Stat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_stat(text: &str) -> Option<Stat> {
    // The name in parentheses may hold spaces and parentheses, so read from the last `)`.
    let mut fields = text.rsplit_once(") ")?.1.split_whitespace();
    // The state is field 3, the process group field 5 and the start time field 22.
    let state = fields.next()?.chars().next()?;
    let group = fields.nth(1)?.parse().ok()?;
    let since = fields.nth(16).and_then(|s| s.parse().ok());
    Some(Stat {
        state,
        group,
        since,
    })
}

/// Whether a live process (not a zombie) is in process group `group`.
fn group_has_members(group: libc::pid_t) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()?
                .parse::<libc::pid_t>()
                .ok()
        })
        .filter_map(stat)
        .any(|s| s.group == group && s.state != 'Z')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    /// A shell that ignores SIGTERM and starts a `sleep` of its own, in a group of its own; the
    /// shell, and the sleep's pid. The shell is reaped on a thread, as an orphan's new parent would.
    fn stubborn(group: bool) -> (libc::pid_t, libc::pid_t) {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; sleep 600 & echo $!; wait; wait"])
            .stdout(Stdio::piped());
        if group {
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let shell = libc::pid_t::try_from(child.id()).unwrap();
        thread::spawn(move || child.wait());
        (shell, line.trim().parse().unwrap())
    }

    fn gone(pid: libc::pid_t) -> bool {
        matches!(stat(pid), None | Some(Stat { state: 'Z', .. }))
    }

    #[test]
    fn a_group_that_ignores_sigterm_is_killed_after_the_grace() {
        let (shell, sleep) = stubborn(true);
        let orphan = Orphan {
            scope: None,
            process: Started::new(shell, true),
        };
        let clock = Instant::now();
        assert_eq!(orphan.stop(Duration::from_millis(300)), Stopped::Killed);
        assert!(
            clock.elapsed() >= Duration::from_millis(300),
            "killed before its grace"
        );
        // The sleep is killed with its group; nobody reaps it here, so it may linger as a zombie.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(gone(shell) && gone(sleep)) {
            assert!(Instant::now() < deadline, "the group outlived the stop");
            thread::sleep(POLL);
        }
        assert_eq!(orphan.stop(Duration::from_secs(1)), Stopped::Gone);
    }

    #[test]
    fn a_tree_is_stopped_and_sigterm_is_enough_for_a_shell_that_takes_it() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 600 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let sleep: libc::pid_t = line.trim().parse().unwrap();
        let shell = libc::pid_t::try_from(child.id()).unwrap();
        thread::spawn(move || child.wait());
        let orphan = Orphan {
            scope: None,
            process: Started::new(shell, false),
        };
        assert_eq!(orphan.stop(Duration::from_secs(5)), Stopped::Terminated);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone(sleep) {
            assert!(Instant::now() < deadline, "sleep {sleep} outlived the stop");
            thread::sleep(POLL);
        }
    }

    /// A `sleep 600` in a group of its own, reaped on a thread; its pid.
    fn sleeper() -> libc::pid_t {
        let mut child = Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        thread::spawn(move || child.wait());
        pid
    }

    #[test]
    fn a_reused_pid_is_never_signalled() {
        // A live process stands for a pid given out again: its start time differs.
        for group in [false, true] {
            let other = sleeper();
            let orphan = Orphan {
                scope: None,
                process: Some(Started {
                    pid: other,
                    since: stat(other).and_then(|s| s.since).map(|s| s + 1),
                    group,
                }),
            };
            assert_eq!(orphan.stop(Duration::ZERO), Stopped::Gone);
            thread::sleep(POLL);
            assert!(!gone(other), "a reused pid was signalled (group {group})");
            signal(other, libc::SIGKILL);
        }
    }

    #[test]
    fn a_group_whose_leader_has_gone_is_stopped_while_it_has_members_and_never_after() {
        // The shell leads the group, starts a sleep in it and exits at once.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 600 & echo $!"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let leader = libc::pid_t::try_from(child.id()).unwrap();
        let started = Started::new(leader, true);
        child.wait().unwrap();
        let sleep: libc::pid_t = line.trim().parse().unwrap();
        assert_eq!(stat(sleep).map(|s| s.group), Some(leader));
        assert!(group_has_members(leader));
        let orphan = Orphan {
            scope: None,
            process: started,
        };
        assert_eq!(orphan.stop(Duration::from_secs(5)), Stopped::Terminated);
        assert!(gone(sleep));
        // Empty now, so its number is free: nothing is signalled.
        assert!(!group_has_members(leader));
        assert_eq!(orphan.stop(Duration::ZERO), Stopped::Gone);
    }

    #[test]
    fn the_daemon_itself_is_never_a_reported_process() {
        // SAFETY: getpid and getpgrp cannot fail.
        let (me, my_group) = unsafe { (libc::getpid(), libc::getpgrp()) };
        assert_eq!(Started::new(me, false), None);
        assert_eq!(Started::new(my_group, true), None);
        assert_eq!(Started::new(1, true), None);
    }

    #[test]
    fn a_grace_past_any_clock_waits_no_longer_than_the_cap_and_does_not_panic() {
        let pid = sleeper();
        let orphan = Orphan {
            scope: None,
            process: Started::new(pid, true),
        };
        assert_eq!(
            orphan.stop(Duration::from_secs(u64::MAX)),
            Stopped::Terminated
        );
    }

    #[test]
    fn stat_reads_the_state_and_start_time_past_a_name_with_parentheses() {
        let line = "42 (a) (b) c) S 1 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(
            parse_stat(line),
            Some(Stat {
                state: 'S',
                group: 42,
                since: Some(98765)
            })
        );
        assert_eq!(stat(-5), None);
    }
}
