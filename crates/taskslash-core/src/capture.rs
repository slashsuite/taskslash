//! Running a job's command with its output copied into the job's log as well as to the caller.
//!
//! `taskslash run` runs the command on a pseudo-terminal when the caller's stdout is a terminal (so
//! colours and progress bars stay), its window size following the caller's and the caller's stdin
//! forwarded to it when stdin is a terminal too; otherwise on two pipes, stdout and stderr each
//! copied to the caller's matching stream. Either way the command is in a process group of its
//! own, and SIGINT, SIGTERM and SIGHUP sent to the client are forwarded to that group. The caller's
//! terminal is put back as it was when the command ends, and when it is stopped (Ctrl-Z, on the
//! pipe path: on a pseudo-terminal the command's group is orphaned, so the kernel drops its stop).

use crate::phases::{self, Phases};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

/// How long the copy loop waits for more output once the command has exited, and its poll period.
const DRAIN: Duration = Duration::from_millis(100);
/// The signals the client forwards to the command's process group.
const FORWARDED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Opens (creating it, mode 0600, and its directory, mode 0700) the log at `path` for appending.
/// A log that cannot be opened costs a warning, not the job.
pub(crate) fn open_log(path: &Path) -> Option<File> {
    let made = path.parent().map_or(Ok(()), |dir| {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)
    });
    let opened = made.and_then(|()| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
    });
    match opened {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("taskslash: warning: no log at {}: {e}", path.display());
            None
        }
    }
}

/// Copies the command's outputs `sources` until each is at its end, or `done` is set and nothing
/// arrived for `DRAIN` (a process the command left behind may hold an output open). Every source
/// at its end returns at once, `done` or not: there is nothing left to wait for. Each chunk goes
/// into `log`, then to `deliver(index, chunk)`, which returns false when its destination is gone:
/// that source is then closed, so a command writing to it gets SIGPIPE (or SIGHUP on a terminal) as
/// it would have with no client in between.
pub(crate) fn pump(
    sources: Vec<OwnedFd>,
    mut log: Option<&mut File>,
    done: &AtomicBool,
    mut deliver: impl FnMut(usize, &[u8]) -> bool,
) {
    let mut open: Vec<Option<OwnedFd>> = sources.into_iter().map(Some).collect();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Without this, a command whose outputs end before its waiter sets `done` (the usual
        // order) would poll no fds for a whole `DRAIN`, 100 ms on every run.
        if open.iter().all(Option::is_none) {
            break;
        }
        let finished = done.load(Ordering::Acquire);
        let mut fds: Vec<libc::pollfd> = Vec::new();
        let mut index = Vec::new();
        for (i, source) in open.iter().enumerate() {
            if let Some(source) = source {
                fds.push(libc::pollfd {
                    fd: source.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                index.push(i);
            }
        }
        let wait = i32::try_from(DRAIN.as_millis()).unwrap_or(100);
        // SAFETY: `fds` is a valid array of `fds.len()` pollfds for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            if finished {
                break;
            }
            continue;
        }
        for (fd, &i) in fds.iter().zip(&index) {
            if fd.revents == 0 {
                continue;
            }
            // SAFETY: `buf` is valid for `buf.len()` bytes and `fd` is open.
            let got = unsafe { libc::read(fd.fd, buf.as_mut_ptr().cast(), buf.len()) };
            let keep = match usize::try_from(got) {
                Ok(0) => false,
                Ok(n) => {
                    let chunk = &buf[..n];
                    if let Some(file) = log.as_mut()
                        && let Err(e) = file.write_all(chunk)
                    {
                        // A full disk loses the rest of the log, never the caller's copy or the
                        // job; said once, so a short log is not taken for the whole output.
                        eprintln!("taskslash: warning: the log stops here: {e}");
                        log = None;
                    }
                    deliver(i, chunk)
                }
                // EIO is a terminal's end, once the command and everything it started are gone.
                Err(_) => matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ),
            };
            if !keep {
                open[i] = None;
            }
        }
    }
}

/// Writes all of `bytes` to `fd`, unbuffered, waiting for room when `fd` is non-blocking and full.
fn write_fd(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is valid for `bytes.len()` bytes.
        let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        match usize::try_from(wrote) {
            Ok(n) => bytes = &bytes[n..],
            Err(_) => {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::Interrupted => {}
                    io::ErrorKind::WouldBlock => {
                        let mut room = libc::pollfd {
                            fd,
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        // SAFETY: one valid pollfd; -1 waits until there is room or an error.
                        unsafe { libc::poll(&raw mut room, 1, -1) };
                    }
                    _ => return Err(e),
                }
            }
        }
    }
    Ok(())
}

fn is_tty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

fn errno<T>(result: libc::c_int, value: T) -> io::Result<T> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

/// The signals the client keeps for itself while a command runs: the forwarded ones, SIGWINCH,
/// and SIGTTOU (so handing the terminal back and forth never stops the client).
fn held_signals() -> libc::sigset_t {
    // SAFETY: sigemptyset and sigaddset initialise and fill a local set.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&raw mut set);
        for signal in FORWARDED {
            libc::sigaddset(&raw mut set, signal);
        }
        libc::sigaddset(&raw mut set, libc::SIGWINCH);
        libc::sigaddset(&raw mut set, libc::SIGTTOU);
        set
    }
}

/// Blocks the signals the client keeps for itself in the calling thread, so one sent to the client
/// never lands on a thread of its own that does not forward it (it would take the default action).
/// A thread started before `run` calls this first.
pub(crate) fn hold_signals() {
    let held = held_signals();
    // SAFETY: `held` is an initialised set; the old mask is not wanted.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const held, std::ptr::null_mut()) };
}

/// Forwards the signals the client receives to the command's process group, once there is one
/// (a signal that comes first waits for it), and the caller's window size to the pseudo-terminal.
struct Forwarder {
    state: Mutex<(Option<libc::pid_t>, Vec<libc::c_int>)>,
    /// A copy of the pseudo-terminal's master of its own, so a late SIGWINCH never meets a closed fd.
    master: Option<OwnedFd>,
}

impl Forwarder {
    /// Blocks the held signals in this thread (every thread it starts after inherits that) and
    /// starts the thread that waits for them. It lives until the process exits.
    fn start(master: Option<OwnedFd>) -> Arc<Self> {
        hold_signals();
        let held = held_signals();
        let forwarder = Arc::new(Self {
            state: Mutex::new((None, Vec::new())),
            master,
        });
        let waiting = Arc::clone(&forwarder);
        thread::spawn(move || {
            let mut wanted = held;
            // SAFETY: removes one signal from a local, initialised set.
            unsafe { libc::sigdelset(&raw mut wanted, libc::SIGTTOU) };
            loop {
                let mut signal = 0;
                // SAFETY: `wanted` is initialised and blocked in every thread; `signal` is written.
                if unsafe { libc::sigwait(&raw const wanted, &raw mut signal) } != 0 {
                    continue;
                }
                if signal == libc::SIGWINCH {
                    waiting.resize();
                } else {
                    waiting.forward(signal);
                }
            }
        });
        forwarder
    }

    fn forward(&self, signal: libc::c_int) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.0 {
            // SAFETY: kill with a negative pid signals that process group; a gone group is ESRCH.
            Some(group) => unsafe {
                libc::kill(-group, signal);
            },
            None => state.1.push(signal),
        }
    }

    /// The command started as process group `group`: it gets any signal that came first.
    fn started(&self, group: libc::pid_t) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = Some(group);
        for signal in state.1.drain(..) {
            // SAFETY: as in `forward`.
            unsafe { libc::kill(-group, signal) };
        }
    }

    fn resize(&self) {
        if let Some(master) = &self.master
            && let Some(size) = window_size(libc::STDOUT_FILENO)
        {
            // SAFETY: `size` is a valid winsize; the kernel tells the command with SIGWINCH.
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) };
        }
    }
}

/// How a job's command is stopped when the daemon cancels it: SIGTERM to its process group, then
/// SIGKILL after a grace period if it has not ended. The client's reader of the job connection
/// calls [`Stop::stop`]; whoever runs the command calls [`Stop::started`] and [`Stop::ended`]. A
/// stop that comes before the command starts waits for it.
#[derive(Default)]
pub(crate) struct Stop {
    state: Mutex<StopState>,
}

#[derive(Default)]
struct StopState {
    /// The process to signal, and whether it leads a process group of its own (signal the group,
    /// else the process and its descendants).
    target: Option<(libc::pid_t, bool)>,
    /// A stop asked for before the command started: its grace period.
    pending: Option<Duration>,
    ended: bool,
    /// Told the command's pid and whether it leads a group, once, when it starts.
    announce: Option<Box<dyn FnOnce(libc::pid_t, bool) + Send>>,
}

impl StopState {
    fn signal(&self, signal: libc::c_int) {
        if self.ended {
            return;
        }
        match self.target {
            // SAFETY: a negative pid signals that process group; a gone one is ESRCH.
            Some((pid, true)) => unsafe {
                libc::kill(-pid, signal);
            },
            // Not a group of its own (it shares the client's): the process and everything under
            // it, gathered before any is signalled, so none is reparented out of reach first.
            Some((pid, false)) => {
                for pid in tree(pid) {
                    // SAFETY: signals one process; a gone one is ESRCH.
                    unsafe { libc::kill(pid, signal) };
                }
            }
            None => {}
        }
    }
}

/// `pid` and its descendants, from each thread's `/proc/<pid>/task/<tid>/children`.
pub(crate) fn tree(pid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut found = vec![pid];
    let mut next = 0;
    while let Some(&parent) = found.get(next) {
        next += 1;
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let children =
                std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
            found.extend(
                children
                    .split_whitespace()
                    .filter_map(|c| c.parse::<libc::pid_t>().ok()),
            );
        }
    }
    found
}

impl Stop {
    /// Stops the command: SIGTERM now and SIGKILL after `grace` if it is still running, or SIGKILL
    /// straight away when `grace` is zero.
    pub(crate) fn stop(self: &Arc<Self>, grace: Duration) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.target.is_none() {
            state.pending = Some(grace);
            return;
        }
        drop(state);
        self.term_then_kill(grace);
    }

    fn term_then_kill(self: &Arc<Self>, grace: Duration) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if grace.is_zero() {
            state.signal(libc::SIGKILL);
            return;
        }
        state.signal(libc::SIGTERM);
        drop(state);
        let stop = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(grace);
            stop.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .signal(libc::SIGKILL);
        });
    }

    /// Has `announce` told the command's pid, and whether it leads a group, when it starts.
    pub(crate) fn announce(&self, announce: impl FnOnce(libc::pid_t, bool) + Send + 'static) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .announce = Some(Box::new(announce));
    }

    /// The command started as process `pid`, leading a process group of its own when `group`. A
    /// stop asked for earlier is sent now.
    pub(crate) fn started(self: &Arc<Self>, pid: libc::pid_t, group: bool) {
        let (pending, announce) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.target = Some((pid, group));
            (state.pending.take(), state.announce.take())
        };
        if let Some(announce) = announce {
            announce(pid, group);
        }
        if let Some(grace) = pending {
            self.term_then_kill(grace);
        }
    }

    /// The command has ended: nothing is signalled after this.
    pub(crate) fn ended(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ended = true;
    }
}

fn window_size(fd: RawFd) -> Option<libc::winsize> {
    // SAFETY: TIOCGWINSZ writes one winsize.
    unsafe {
        let mut size = std::mem::zeroed::<libc::winsize>();
        (libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut size) == 0).then_some(size)
    }
}

fn termios(fd: RawFd) -> Option<libc::termios> {
    // SAFETY: tcgetattr writes one termios.
    unsafe {
        let mut termios = std::mem::zeroed::<libc::termios>();
        (libc::tcgetattr(fd, &raw mut termios) == 0).then_some(termios)
    }
}

/// The caller's terminal as the client found it, and what it changes while the command runs:
/// raw mode on stdin (the pseudo-terminal path, so each key goes to the command's terminal), or
/// the terminal's foreground handed to the command's group (the pipe path with a terminal on stdin,
/// so the command may read it). `Drop` puts it back.
struct Caller {
    saved: Option<libc::termios>,
    raw: bool,
    handoff: bool,
    group: Option<libc::pid_t>,
}

impl Caller {
    /// Whether the client's group is the terminal on stdin's foreground.
    fn in_foreground() -> bool {
        // SAFETY: tcgetpgrp and getpgrp only read.
        is_tty(libc::STDIN_FILENO)
            && unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() }
    }

    /// Takes over the terminal: raw mode when `raw`, the foreground to `group` when `handoff`.
    fn take(&self) {
        if !Self::in_foreground() {
            return;
        }
        if self.raw
            && let Some(saved) = self.saved
        {
            let mut raw = saved;
            // SAFETY: cfmakeraw edits a local copy; tcsetattr applies it to stdin.
            unsafe {
                libc::cfmakeraw(&raw mut raw);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw const raw);
            }
        }
        if self.handoff
            && let Some(group) = self.group
        {
            // SAFETY: SIGTTOU is blocked, so this never stops the client.
            unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, group) };
        }
    }

    /// Puts the terminal back as it was found.
    fn give_back(&self) {
        // SAFETY: as in `take`; restoring a saved termios and the client's own group.
        unsafe {
            if self.handoff && self.group.is_some() {
                libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
            }
            if self.raw
                && let Some(saved) = self.saved
            {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw const saved);
            }
        }
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.give_back();
    }
}

/// Runs `command` to its end with its output copied to the caller and into `log`, forwarding
/// signals to it and restoring the caller's terminal after; `stop` cancels it. Returns how it ended;
/// an `Err` means it never started. `phases` gets `launch`, `command` and `drain`.
pub(crate) fn run(
    mut command: Command,
    mut log: Option<File>,
    stop: &Arc<Stop>,
    phases: Option<&Phases>,
) -> io::Result<ExitStatus> {
    let pty = is_tty(libc::STDOUT_FILENO);
    let stdin_tty = is_tty(libc::STDIN_FILENO);
    let mut caller = Caller {
        saved: termios(libc::STDIN_FILENO),
        raw: pty && stdin_tty,
        handoff: !pty && Caller::in_foreground(),
        group: None,
    };
    let mut sources = Vec::new();
    // Where each source's bytes go: an fd of the caller's, or the pseudo-terminal.
    let mut sinks = Vec::new();
    let master = if pty {
        let (master, slave) = open_pty()?;
        command.stdout(Stdio::from(slave.try_clone()?));
        if stdin_tty {
            command.stdin(Stdio::from(slave.try_clone()?));
        }
        if is_tty(libc::STDERR_FILENO) {
            command.stderr(Stdio::from(slave));
        } else {
            command.stderr(Stdio::piped());
        }
        Some(master)
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        None
    };
    let handoff = caller.handoff;
    // SAFETY: the closure runs in the forked child before exec and only makes async-signal-safe
    // calls.
    unsafe {
        command.pre_exec(move || {
            if pty {
                errno(libc::setsid(), ())?;
                errno(libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSCTTY, 0), ())?;
            } else {
                errno(libc::setpgid(0, 0), ())?;
                if handoff {
                    // SIGTTOU is still blocked, as it is in the client, so this cannot stop it.
                    libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
                }
            }
            let mut none = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&raw mut none);
            errno(
                libc::pthread_sigmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut()),
                (),
            )
        });
    }
    // The stdin copy writes to a master of its own, which outlives the copy loop's.
    let to_master = master.as_ref().map(OwnedFd::try_clone).transpose()?;
    let forwarder = Forwarder::start(master.as_ref().map(OwnedFd::try_clone).transpose()?);
    let mut child = command.spawn()?;
    phases::mark(phases, "launch");
    // The command's copies of the terminal's other end are closed with it, so reading the master
    // ends when the command and what it started are gone.
    drop(command);
    let pid = libc::pid_t::try_from(child.id()).unwrap_or(0);
    forwarder.started(pid);
    stop.started(pid, true);
    caller.group = Some(pid);
    caller.take();

    // A client in the terminal's background would be stopped (SIGTTIN) for reading it.
    if let Some(to_master) = to_master
        && stdin_tty
        && Caller::in_foreground()
    {
        forward_input(to_master);
    }
    if let Some(master) = master {
        sinks.push(libc::STDOUT_FILENO);
        sources.push(master);
    } else if let Some(stdout) = child.stdout.take() {
        sinks.push(libc::STDOUT_FILENO);
        sources.push(stdout.into());
    }
    if let Some(stderr) = child.stderr.take() {
        sinks.push(libc::STDERR_FILENO);
        sources.push(stderr.into());
    }

    let done = AtomicBool::new(false);
    let status = thread::scope(|scope| {
        let done = &done;
        let log = log.as_mut();
        let sinks = &sinks;
        let pumping = scope.spawn(move || {
            pump(sources, log, done, |i, chunk| {
                write_fd(sinks[i], chunk).is_ok()
            });
        });
        let status = wait(pid, &caller);
        phases::mark(phases, "command");
        stop.ended();
        done.store(true, Ordering::Release);
        if let Err(panic) = pumping.join() {
            std::panic::resume_unwind(panic);
        }
        phases::mark(phases, "drain");
        status
    });
    drop(caller);
    status
}

/// Copies the caller's stdin to the command's terminal on a thread of its own, so input the command
/// does not read waits there (as it would on a terminal) without holding up the copy of its
/// output. The thread ends at stdin's end or when the terminal is gone, else with the process.
fn forward_input(to_master: OwnedFd) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let got = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
            match usize::try_from(got) {
                Ok(0) => return,
                Ok(n) => {
                    if write_fd(to_master.as_raw_fd(), &buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
}

/// Waits for `pid` to end. When it stops (Ctrl-Z), the client gives the terminal back and stops
/// its own group the same way; when continued, it takes the terminal again and continues it.
fn wait(pid: libc::pid_t, caller: &Caller) -> io::Result<ExitStatus> {
    loop {
        let mut status = 0;
        // SAFETY: waits for our own child; `status` is written.
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, libc::WUNTRACED) };
        if reaped < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if libc::WIFSTOPPED(status) {
            caller.give_back();
            // SAFETY: stops the client the way the command was stopped: its whole group (a
            // shell's job) under job control, else itself. It returns once continued.
            unsafe {
                if is_tty(libc::STDIN_FILENO) {
                    libc::kill(0, libc::SIGTSTP);
                } else {
                    libc::kill(libc::getpid(), libc::SIGSTOP);
                }
            }
            caller.take();
            // SAFETY: continues the command's group.
            unsafe { libc::kill(-pid, libc::SIGCONT) };
            continue;
        }
        return Ok(ExitStatus::from_raw(status));
    }
}

/// A pseudo-terminal with the caller's stdout's settings and window size: (master, slave), both
/// close-on-exec.
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let settings = termios(libc::STDOUT_FILENO);
    let size = window_size(libc::STDOUT_FILENO);
    // SAFETY: openpty writes two fds; the settings and size are valid or null.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            settings
                .as_ref()
                .map_or(std::ptr::null(), std::ptr::from_ref),
            size.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
        )
    };
    errno(opened, ())?;
    // SAFETY: openpty returned two open fds that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: sets a flag on an fd we own.
        errno(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            (),
        )?;
    }
    Ok((master, slave))
}

#[cfg(test)]
mod tests {
    use super::{Stop, pump};
    use std::io::{BufRead, BufReader};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Whether `pid` is gone, or a zombie nobody has reaped yet.
    fn gone(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit(") ").next().is_some_and(|s| s.starts_with('Z'))
        })
    }

    /// A pipe with `bytes` written into it and its write end closed: its read end.
    fn ended_pipe(bytes: &[u8]) -> OwnedFd {
        let mut fds = [0; 2];
        // SAFETY: pipe writes two fds into `fds`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both fds are open and ours alone.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut write = std::fs::File::from(write);
        std::io::Write::write_all(&mut write, bytes).unwrap();
        read
    }

    #[test]
    fn the_copy_ends_when_every_output_has_ended_before_the_command_is_reaped() {
        // `done` is never set: the waiter has not seen the exit yet. Before, this polled no fds
        // for 100 ms at a time until `done`, so it never returned here.
        let (sent, got) = mpsc::channel();
        thread::spawn(move || {
            let done = AtomicBool::new(false);
            let mut copied = vec![Vec::new(), Vec::new()];
            pump(
                vec![ended_pipe(b"out"), ended_pipe(b"err")],
                None,
                &done,
                |i, chunk| {
                    copied[i].extend_from_slice(chunk);
                    true
                },
            );
            let _ = sent.send(copied);
        });
        let copied = got
            .recv_timeout(Duration::from_secs(5))
            .expect("the copy waited for done with every output at its end");
        assert_eq!(copied, [b"out".to_vec(), b"err".to_vec()]);
    }

    #[test]
    fn a_stop_without_a_group_kills_the_process_and_what_it_started() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 600 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild = line.trim().to_owned();
        let stop = Arc::new(Stop::default());
        stop.started(libc::pid_t::try_from(child.id()).unwrap(), false);
        stop.stop(Duration::ZERO);
        let status = child.wait().unwrap();
        stop.ended();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(9)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gone(&grandchild) {
            assert!(
                Instant::now() < deadline,
                "sleep {grandchild} outlived the stop"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}
