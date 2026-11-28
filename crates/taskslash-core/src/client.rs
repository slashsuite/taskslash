//! The client side: `taskslash run` asks the daemon for a job, runs the command as this process's
//! child the way the daemon says (its output copied into the log the daemon names, `capture`),
//! reports how it ended, and returns its exit code (`taskslash moon`
//! does the same for many jobs at once, one thread and one connection each);
//! `taskslash history` asks for a key's recorded runs, `taskslash status` for what is running, and
//! `taskslash why` why a job waits, and `taskslash events` and `taskslash top` follow the job events.

use crate::Error;
use crate::admission::{Reason, Waiting, waiting_line};
use crate::capture::{self, Stop};
use crate::estimate::Estimate;
use crate::launch;
use crate::phases::{self, Phases};
use crate::protocol::{
    Class, ClientMessage, DaemonMessage, JobEnded, JobEvent, JobState, KeySummary, Kind, Launch,
    Run, Status, Was, receive, send, submitted_from,
};
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// The exit code when the command cannot be started, as a shell uses.
const CANNOT_RUN: i32 = 127;

mod logs;
pub use logs::logs;

/// A connection to the daemon: its two halves, and how to word an I/O failure on it.
struct Connection<'a> {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    socket: &'a Path,
}

impl<'a> Connection<'a> {
    fn open(socket: &'a Path) -> Result<Self, Error> {
        let stream = UnixStream::connect(socket).map_err(|e| {
            Error::new(format!(
                "no daemon at {} ({e}). Start it with 'systemctl --user start taskslash' or 'taskslash daemon'.",
                socket.display()
            ))
        })?;
        let writer = stream
            .try_clone()
            .map_err(|e| Error::new(format!("cannot use the daemon's socket: {e}")))?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            socket,
        })
    }

    /// Sends `message` and reads the reply.
    fn ask(&mut self, message: &ClientMessage) -> Result<DaemonMessage, Error> {
        send(&mut self.writer, message).map_err(|e| self.lost(e))?;
        self.next()
    }

    /// Reads the next message from the daemon.
    fn next(&mut self) -> Result<DaemonMessage, Error> {
        match receive(&mut self.reader).map_err(|e| self.lost(e))? {
            Some(DaemonMessage::Error { message }) => {
                Err(Error::new(format!("the daemon refused: {message}")))
            }
            Some(reply) => Ok(reply),
            None => Err(Error::new("the daemon closed the connection")),
        }
    }

    fn lost(&self, e: io::Error) -> Error {
        Error::new(format!("lost the daemon at {}: {e}", self.socket.display()))
    }
}

/// Asks the daemon at `socket` `message`. When none answers there, a question the database alone
/// answers (history, keys, a run's log) is answered from it, read-only, with a line on stderr
/// saying so; anything else, or no database, is the error that no daemon answered.
fn ask_or_read(socket: &Path, message: &ClientMessage) -> Result<DaemonMessage, Error> {
    let mut daemon = match Connection::open(socket) {
        Ok(daemon) => daemon,
        Err(no_daemon) => {
            let Ok(db) = crate::store::db_path() else {
                return Err(no_daemon);
            };
            return crate::answers::offline(socket, &db, message).unwrap_or(Err(no_daemon));
        }
    };
    daemon.ask(message)
}

/// What the caller says a job costs, replacing the learned estimate: CPU threads and MiB.
#[derive(Debug, Clone, Copy, Default)]
pub struct Demand {
    pub cpu: Option<f64>,
    pub mem_mib: Option<u64>,
}

/// Runs `argv` as a job of kind `kind` and class `class` of the daemon at `socket`, once the daemon admits it (a job
/// that has to wait says why on stderr, when queued and every 15 s). Returns the command's exit code (128+N when a
/// signal N killed it, 127 when it cannot be started). An `Err` means the command was never run.
///
/// `argv` is passed to the child as it is; the daemon is sent a lossy UTF-8 copy.
pub fn run(
    socket: &Path,
    name: Option<String>,
    kind: Kind,
    class: Class,
    demand: Demand,
    argv: &[OsString],
) -> Result<i32, Error> {
    let cwd = env::current_dir()
        .map_err(|e| Error::new(format!("cannot read the current directory: {e}")))?;
    let job = Job {
        name,
        kind,
        class,
        demand,
        argv,
        cwd: &cwd,
        label: None,
    };
    let phases = Phases::from_env();
    let ran = run_job(socket, &job, phases.as_ref(), |command, log, stop| {
        capture::run(command, log, stop, phases.as_ref()).map(|status| (status, Report::default()))
    })
    .map(|(code, _)| code);
    if let Some(phases) = &phases {
        eprintln!("{}", phases.line());
    }
    ran
}

/// One job to submit: `run`'s arguments, and the directory it runs in.
pub(crate) struct Job<'a> {
    pub name: Option<String>,
    pub kind: Kind,
    pub class: Class,
    pub demand: Demand,
    pub argv: &'a [OsString],
    pub cwd: &'a Path,
    /// Put in front of each waiting line, when several jobs of one process wait together.
    pub label: Option<&'a str>,
}

/// What the client tells the daemon about a run besides its exit code.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Report {
    pub cached: Option<bool>,
    pub moon_hash: Option<String>,
}

/// Submits `job` on a connection of its own, waits for admission, has `exec` run the prepared
/// command (in `job.cwd`) to its end with the job's log open for it (`None` when it cannot be
/// opened), and reports the end to the daemon. While the command runs, a thread reads the job's
/// connection, and a `cancel` from the daemon stops the command through the `Stop` handed to
/// `exec`. Several may run at once, one per thread. Returns the exit code and `exec`'s report; an
/// `Err` means the command never ran (it was cancelled while waiting, or the daemon is gone).
/// `phases` gets the marks the client can see (`phases` module); `exec` makes its own.
pub(crate) fn run_job(
    socket: &Path,
    job: &Job,
    phases: Option<&Phases>,
    exec: impl FnOnce(Command, Option<File>, &Arc<Stop>) -> io::Result<(ExitStatus, Report)>,
) -> Result<(i32, Report), Error> {
    let mut daemon = Connection::open(socket)?;
    phases::mark(phases, "connect");
    let submit = ClientMessage::Submit {
        name: job.name.clone(),
        kind: job.kind,
        argv: job
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        cwd: job.cwd.to_string_lossy().into_owned(),
        cpu: job.demand.cpu,
        mem_mib: job.demand.mem_mib,
        class: job.class,
        submitter: submitter(job.cwd).to_string_lossy().into_owned(),
    };
    let mut reply = daemon.ask(&submit)?;
    // The never-run hint goes on the first waiting line only, and not when the caller gave a cost.
    let mut hint_due = job.demand.cpu.is_none() && job.demand.mem_mib.is_none();
    let (id, unit, launch, log, announce) = loop {
        match reply {
            DaemonMessage::Start {
                job,
                unit,
                launch,
                log,
                running,
            } => break (job, unit, launch, log, running),
            DaemonMessage::Waiting {
                job: number,
                reason,
                waited_s,
                position,
                never_seen,
                assumed_threads,
            } => {
                let never_run = assumed_threads.filter(|_| hint_due && never_seen);
                hint_due = false;
                let line = waiting_line(&Waiting {
                    job: number,
                    reason: &reason,
                    waited_s,
                    position,
                    never_run,
                });
                match job.label {
                    Some(label) => eprintln!(
                        "taskslash: {label}: {}",
                        line.trim_start_matches("taskslash: ")
                    ),
                    None => eprintln!("{line}"),
                }
            }
            DaemonMessage::Cancel { job: number, .. } => {
                return Err(Error::new(format!("job {number} cancelled while waiting")));
            }
            other => return Err(Error::new(format!("unexpected reply {other:?}"))),
        }
        reply = daemon.next()?;
    };
    phases::mark(phases, "admit");

    // From here the daemon speaks only to cancel the job, and to answer `exited`.
    let stop = Arc::new(Stop::default());
    let Connection {
        reader,
        writer: mut daemon,
        socket: _,
    } = daemon;
    let listening = listen(reader, id, Arc::clone(&stop));
    // So the daemon can stop the command should this client die before it says how it ended.
    if let Some(mut to_daemon) = announce.then(|| daemon.try_clone().ok()).flatten() {
        stop.announce(move |pid, group| {
            let _ = send(
                &mut to_daemon,
                &ClientMessage::Running {
                    job: id,
                    pid,
                    group,
                },
            );
        });
    }

    let program = Path::new(&job.argv[0]);
    let mut log = capture::open_log(Path::new(&log));
    // Said on stderr and in the log, so the log of a run that never started says why.
    let cannot_run = |why: &dyn std::fmt::Display, log: Option<&mut File>| {
        let line = format!("taskslash: cannot run {}: {why}\n", program.display());
        eprint!("{line}");
        if let Some(log) = log {
            let _ = log.write_all(line.as_bytes());
        }
        (CANNOT_RUN, Report::default())
    };
    let (code, report) = if launch == Launch::Scope && !launch::can_start(program) {
        cannot_run(&"no such executable", log.as_mut())
    } else {
        let mut command = launch::command(launch, &unit, job.class, job.argv);
        command.current_dir(job.cwd);
        let mut copy = log.as_ref().and_then(|log| log.try_clone().ok());
        match exec(command, log, &stop) {
            Ok((status, report)) => (exit_code(status), report),
            Err(e) => cannot_run(&e, copy.as_mut()),
        }
    };

    // The child has run; a daemon that went away meanwhile costs a warning, not its exit code.
    let exited = ClientMessage::Exited {
        job: id,
        code,
        cached: report.cached,
        moon_hash: report.moon_hash.clone(),
    };
    let reply = match send(&mut daemon, &exited) {
        Ok(()) => listening
            .join()
            .unwrap_or_else(|_| Err("its reader panicked".to_owned())),
        Err(e) => Err(e.to_string()),
    };
    phases::mark(phases, "report");
    match reply {
        Ok(DaemonMessage::Recorded { .. }) => {}
        Ok(_) => eprintln!("taskslash: warning: the daemon did not record job {id}"),
        Err(e) => eprintln!("taskslash: warning: could not report job {id} to the daemon: {e}"),
    }
    Ok((code, report))
}

/// Reads job `id`'s connection while its command runs: a `cancel` stops the command through
/// `stop` (and says so on stderr); the thread returns the first other message, the daemon's answer
/// to `exited`, or why there is none.
fn listen(
    mut reader: BufReader<UnixStream>,
    id: u64,
    stop: Arc<Stop>,
) -> thread::JoinHandle<Result<DaemonMessage, String>> {
    thread::spawn(move || {
        capture::hold_signals();
        loop {
            match receive(&mut reader) {
                Ok(Some(DaemonMessage::Cancel { grace_s, .. })) => {
                    eprintln!("taskslash: job {id} cancelled");
                    stop.stop(Duration::from_secs(grace_s));
                }
                Ok(Some(DaemonMessage::Error { message })) => {
                    return Err(format!("the daemon refused: {message}"));
                }
                Ok(Some(reply)) => return Ok(reply),
                Ok(None) => return Err("the daemon closed the connection".to_owned()),
                Err(e) => return Err(e.to_string()),
            }
        }
    })
}

/// What `taskslash cancel` got.
#[derive(Debug, Clone, PartialEq)]
pub enum Cancelled {
    /// The job was waiting or running and is cancelled now, or `already` was.
    Now { was: Was, already: bool },
    /// The job is neither waiting nor running; what became of it, when the daemon remembers.
    NotHeld(Option<JobEnded>),
}

/// Cancels job `job`: off the queue when it waits; when it runs, its client sends its command
/// SIGTERM, then SIGKILL after `grace_s` (0: SIGKILL straight away).
pub fn cancel(socket: &Path, job: u64, grace_s: u64) -> Result<Cancelled, Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon
        .ask(&ClientMessage::Cancel { job, grace_s })
        .map_err(older_daemon)?
    {
        DaemonMessage::Cancelled { was, already, .. } => Ok(Cancelled::Now { was, already }),
        DaemonMessage::NotCancelled { ended, .. } => Ok(Cancelled::NotHeld(ended)),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// The estimate of `key` and its recorded runs, newest first: at most `limit`, and with `within`
/// only those submitted from that directory (a full path, or a directory's name). Both are applied
/// here too, so a daemon older than these fields still answers right.
pub fn history(
    socket: &Path,
    key: &str,
    limit: usize,
    within: Option<&str>,
) -> Result<(Estimate, Vec<Run>), Error> {
    // A shell completes a directory with a trailing `/`; `/` alone stays.
    let within = within.map(|w| match w.trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
    });
    match ask_or_read(
        socket,
        &ClientMessage::History {
            key: key.to_owned(),
            limit: Some(limit),
            within: within.map(str::to_owned),
        },
    )? {
        DaemonMessage::Runs { estimate, mut runs } => {
            if let Some(within) = within {
                runs.retain(|run| submitted_from(&run.submitter, within));
            }
            runs.truncate(limit);
            Ok((estimate, runs))
        }
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// Every key with recorded runs, the most recently run first.
pub fn keys(socket: &Path) -> Result<Vec<KeySummary>, Error> {
    match ask_or_read(socket, &ClientMessage::Keys).map_err(older_daemon)? {
        DaemonMessage::Keys { keys } => Ok(keys),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// What a daemon that does not know a message says (`bad request: unknown variant ...`), worded
/// as what to do about it; any other error as it is.
fn older_daemon(e: Error) -> Error {
    if e.to_string().contains("unknown variant") {
        Error::new(
            "the daemon is older than this client; reinstall it (bun scripts/install-service.ts)",
        )
    } else {
        e
    }
}

/// The newest `limit` recorded runs of any key, newest first.
pub fn recent(socket: &Path, limit: usize) -> Result<Vec<Run>, Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon
        .ask(&ClientMessage::Recent { limit })
        .map_err(older_daemon)?
    {
        DaemonMessage::Recent { runs } => Ok(runs),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// What the machine and the running jobs are doing, from the daemon's latest readings.
pub fn status(socket: &Path) -> Result<Status, Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon.ask(&ClientMessage::Status)? {
        DaemonMessage::Status(status) => Ok(*status),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// Where a job is, as `why` finds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Whereabouts {
    /// Waiting, with its reason, the seconds it has waited and its place in line.
    Waiting(Reason, u64, usize),
    Running,
    /// Ended since the daemon started.
    Ended(JobEnded),
}

/// Where job `job` is. An `Err` when the daemon does not know it.
pub fn why(socket: &Path, job: u64) -> Result<Whereabouts, Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon.ask(&ClientMessage::Why { job })? {
        DaemonMessage::Why {
            state: JobState::Waiting,
            reason: Some(reason),
            waited_s,
            position,
            ..
        } => Ok(Whereabouts::Waiting(
            reason,
            waited_s.unwrap_or(0),
            position.unwrap_or(1),
        )),
        DaemonMessage::Why {
            state: JobState::Running,
            ..
        } => Ok(Whereabouts::Running),
        DaemonMessage::Why {
            ended: Some(ended), ..
        } => Ok(Whereabouts::Ended(ended)),
        DaemonMessage::Why { .. } => Err(Error::new(format!(
            "job {job} not found: the daemon forgets ended jobs when it restarts and keeps only \
             the newest 1000 ('taskslash history' lists the keys, and each key's runs)"
        ))),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// Subscribes to the daemon's job events and writes each event line to `out` as it arrives,
/// unchanged, until the daemon hangs up. `subscribed` is not written; `listening` runs once the
/// daemon has answered it, before the first event.
pub fn events(socket: &Path, out: &mut impl Write, listening: impl FnOnce()) -> Result<(), Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon.ask(&ClientMessage::Subscribe)? {
        DaemonMessage::Subscribed { .. } => listening(),
        other => return Err(Error::new(format!("unexpected reply {other:?}"))),
    }
    let mut line = String::new();
    loop {
        line.clear();
        if daemon
            .reader
            .read_line(&mut line)
            .map_err(|e| daemon.lost(e))?
            == 0
        {
            return Ok(());
        }
        // After `subscribed` the daemon sends only event lines, copied as they are so an event
        // type this client does not know yet still gets through.
        out.write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| Error::new(format!("cannot write the events: {e}")))?;
    }
}

/// Subscribes to the daemon's job events. Returns once the daemon has answered `subscribed`, so
/// every event from then on reaches [`Subscription::each`].
pub fn subscribe(socket: &Path) -> Result<Subscription, Error> {
    let mut daemon = Connection::open(socket)?;
    match daemon.ask(&ClientMessage::Subscribe)? {
        DaemonMessage::Subscribed { .. } => Ok(Subscription {
            reader: daemon.reader,
            socket: socket.to_owned(),
        }),
        other => Err(Error::new(format!("unexpected reply {other:?}"))),
    }
}

/// A subscription to the daemon's job events, from [`subscribe`].
pub struct Subscription {
    reader: BufReader<UnixStream>,
    socket: PathBuf,
}

impl Subscription {
    /// Hands each event, parsed, to `on_event`, until the daemon hangs up or `on_event` returns
    /// false (both `Ok`). A line that is not an event this client knows (a newer event type) is
    /// skipped.
    pub fn each(mut self, mut on_event: impl FnMut(JobEvent) -> bool) -> Result<(), Error> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self.reader.read_line(&mut line).map_err(|e| {
                Error::new(format!("lost the daemon at {}: {e}", self.socket.display()))
            })?;
            if read == 0 {
                return Ok(());
            }
            if let Ok(DaemonMessage::Event { event, .. }) = serde_json::from_str(&line)
                && !on_event(event)
            {
                return Ok(());
            }
        }
    }
}

/// Who submits from `cwd`: the nearest ancestor of it, itself first, with a `.git` entry (a
/// directory in a clone, a file in a worktree), else `cwd`. `taskslash run` sends it, and
/// `taskslash logs --failed` searches its runs.
pub fn submitter(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
        .unwrap_or(cwd)
}

/// A shell's exit code for `status`: the code, or 128+N when signal N killed it.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::{older_daemon, submitter, subscribe};
    use crate::Error;
    use crate::protocol::JobEvent;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::Path;

    #[test]
    fn the_submitter_is_the_nearest_directory_with_a_git_entry() {
        let root = std::env::temp_dir().join(format!("tks-{}", std::process::id()));
        let deep = root.join("wt/a/b");
        fs::create_dir_all(&deep).unwrap();
        // No .git under `root`: the cwd itself, unless a directory above `root` has one.
        let outside = submitter(&deep);
        assert!(
            outside == deep || !outside.starts_with(&root),
            "{outside:?}"
        );
        fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(submitter(&deep), root, "a clone's .git directory");
        fs::write(root.join("wt/.git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(submitter(&deep), root.join("wt"), "a worktree's .git file");
        assert_eq!(
            submitter(&root.join("wt")),
            root.join("wt"),
            "the cwd itself counts"
        );
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(submitter(Path::new("/")), Path::new("/"));
    }

    #[test]
    fn subscribe_hands_over_each_event_parsed_and_skips_an_unknown_line() {
        let dir = std::env::temp_dir().join(format!("tksub-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let daemon = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "{\"type\":\"subscribe\"}\n");
            let mut out = stream;
            out.write_all(
                b"{\"type\":\"subscribed\",\"protocol\":1}\n\
                  {\"type\":\"event\",\"protocol\":1,\"seq\":1,\"event\":{\"type\":\"dropped\",\"count\":3}}\n\
                  {\"type\":\"event\",\"protocol\":1,\"seq\":2,\"event\":{\"type\":\"later\",\"job\":1}}\n\
                  {\"type\":\"event\",\"protocol\":1,\"seq\":3,\"event\":{\"type\":\"dropped\",\"count\":4}}\n",
            )
            .unwrap();
        });
        let mut got = Vec::new();
        subscribe(&socket)
            .unwrap()
            .each(|event| {
                got.push(event);
                true
            })
            .unwrap();
        daemon.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            got,
            [
                JobEvent::Dropped { count: 3 },
                JobEvent::Dropped { count: 4 }
            ]
        );
    }

    #[test]
    fn a_daemon_that_does_not_know_the_message_reads_as_reinstall_it() {
        let refused = Error::new(
            "the daemon refused: bad request: unknown variant `keys`, expected one of `submit`, `history`",
        );
        assert_eq!(
            older_daemon(refused).to_string(),
            "the daemon is older than this client; reinstall it (bun scripts/install-service.ts)"
        );
        let other = Error::new("the daemon refused: database: disk I/O error");
        assert_eq!(
            older_daemon(other).to_string(),
            "the daemon refused: database: disk I/O error"
        );
    }
}
