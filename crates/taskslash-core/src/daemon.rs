//! `taskslash daemon`: hands out jobs, measures them, and records a row for each when it ends. One
//! thread per connection; a connection is one job, one query or one subscriber to the job events.
//! One more thread samples the machine once a second, and `status` answers from its latest snapshot.

use crate::Error;
use crate::admission::Limits;
use crate::answers::{self, recorded};
use crate::launch;
use crate::logs::{self, Retention};
use crate::procfs;
use crate::protocol::{
    ClientMessage, DaemonInfo, DaemonMessage, JobEnded, JobState, Launch, LogFound, LogSelect,
    Status, Was, receive, send,
};
use crate::sampler::{FileSampler, MachineSampler, MachineSnapshot, ProcSampler};
use crate::scheduler::{Scheduler, lock};
use crate::store::Store;
use std::collections::HashMap;
use std::fs::{self, DirBuilder};
use std::io::BufReader;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the machine is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How often expired logs are deleted, besides at startup.
const SWEEP_EVERY: Duration = Duration::from_secs(3600);

mod ended;
mod events;
mod job;

use ended::{ENDED_JOBS, Ended, EndedJobs};

/// What every connection shares. Lock order: `scheduler` before `store`, never the other way.
struct Daemon {
    launch: Launch,
    /// The directory every job's log goes in, and how long they are kept.
    logs: PathBuf,
    retention: Retention,
    store: Mutex<Store>,
    next_job: AtomicU64,
    machine: Mutex<MachineSnapshot>,
    scheduler: Mutex<Scheduler>,
    /// The subscribers to the job events.
    events: events::Bus,
    /// The newest jobs that ended since it started, and their runs. Locked on its own.
    ended: Mutex<EndedJobs>,
    /// Each held job's connection, so `cancel` can reach its client. Locked after `scheduler`.
    links: Mutex<HashMap<u64, Arc<Mutex<job::Link>>>>,
    /// When it started serving, for `status`.
    started_at: SystemTime,
}

/// Binds the socket at `path`, opens the database at `db`, and serves until the process is killed.
pub fn serve(path: &Path, db: &Path) -> Result<(), Error> {
    let store = Store::open(db)?;
    let limits = Limits::from_env()?;
    let retention = Retention::from_env()?;
    let logs = logs::logs_dir(db);
    logs::create_dir(&logs)?;
    let mut machine = sampler()?;
    // Asked before binding, so a daemon that accepts has already logged its launch path.
    let launch = launch::choose();
    let listener = bind(path)?;
    if launch == Launch::Plain {
        eprintln!("taskslash: no systemd user session, jobs run unmeasured");
    }
    eprintln!(
        "taskslash daemon listening on {} (launch: {}, database: {}, logs: {})",
        path.display(),
        launch.as_str(),
        db.display(),
        logs.display()
    );
    // The first snapshot is taken before the first connection, so `status` always has one.
    let first = machine.sample();
    let daemon = Arc::new(Daemon {
        launch,
        logs,
        retention,
        store: Mutex::new(store),
        next_job: AtomicU64::new(1),
        scheduler: Mutex::new(Scheduler::new(limits, first.clone())),
        machine: Mutex::new(first),
        events: events::Bus::default(),
        ended: Mutex::new(EndedJobs::new(ENDED_JOBS)),
        links: Mutex::new(HashMap::new()),
        started_at: SystemTime::now(),
    });
    let sampling = Arc::clone(&daemon);
    thread::spawn(move || sampling.sample_forever(machine));
    // Once before the first connection, then every hour.
    daemon.sweep();
    let sweeping = Arc::clone(&daemon);
    thread::spawn(move || {
        loop {
            thread::sleep(SWEEP_EVERY);
            sweeping.sweep();
        }
    });
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                thread::spawn(move || daemon.serve_connection(stream));
            }
            Err(e) => eprintln!("taskslash: accept failed: {e}"),
        }
    }
    Ok(())
}

/// The real sampler, or the fake one `TASKSLASH_FAKE_MACHINE` names (for tests).
fn sampler() -> Result<Box<dyn MachineSampler>, Error> {
    match std::env::var_os("TASKSLASH_FAKE_MACHINE").filter(|v| !v.is_empty()) {
        Some(file) => {
            let file = PathBuf::from(file);
            eprintln!("taskslash: fake machine from {}", file.display());
            Ok(Box::new(FileSampler::new(file).map_err(Error::new)?))
        }
        None => Ok(Box::new(ProcSampler::new())),
    }
}

/// Binds `path`, creating its parent with mode 0700 (the socket's permissions are the
/// authentication). A socket file nobody answers on is stale and is replaced; anything else at
/// `path` is left alone.
fn bind(path: &Path) -> Result<UnixListener, Error> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| Error::new(format!("cannot create {}: {e}", parent.display())))?;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(Error::new(format!(
                "cannot bind {}: not a socket",
                path.display()
            )));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(Error::new(format!(
                "a daemon is already running on {}",
                path.display()
            )));
        }
        fs::remove_file(path)
            .map_err(|e| Error::new(format!("cannot remove stale {}: {e}", path.display())))?;
    }
    UnixListener::bind(path).map_err(|e| Error::new(format!("cannot bind {}: {e}", path.display())))
}

impl Daemon {
    fn serve_connection(&self, stream: UnixStream) {
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(read_half);
        let mut writer = stream;
        match receive(&mut reader) {
            // A connection that closes without asking is a probe (the stale check, a readiness wait).
            Ok(None) => {}
            Ok(Some(ClientMessage::Submit {
                name,
                kind,
                argv,
                cwd,
                cpu,
                mem_mib,
                class,
                submitter,
            })) if !argv.is_empty() => {
                let submitted = job::Submitted {
                    name: name.as_deref(),
                    kind,
                    argv: &argv,
                    cwd: &cwd,
                    cpu,
                    mem_mib,
                    class,
                    submitter: &submitter,
                };
                self.serve_job(&mut reader, &mut writer, submitted);
            }
            Ok(Some(ClientMessage::Submit { .. })) => {
                refuse(&mut writer, "submit needs a non-empty argv");
            }
            Ok(Some(
                message @ (ClientMessage::History { .. }
                | ClientMessage::Keys
                | ClientMessage::Recent { .. }),
            )) => {
                let reply = match answers::from_store(&self.store(), &message) {
                    Some(Ok(reply)) => reply,
                    Some(Err(e)) => DaemonMessage::Error {
                        message: e.to_string(),
                    },
                    None => unreachable!("the store answers history, keys and recent"),
                };
                let _ = send(&mut writer, &reply);
            }
            Ok(Some(ClientMessage::Status)) => {
                let _ = send(&mut writer, &DaemonMessage::Status(Box::new(self.status())));
            }
            Ok(Some(ClientMessage::Why { job })) => {
                let _ = send(&mut writer, &self.why(job));
            }
            Ok(Some(ClientMessage::Log { select })) => {
                let reply = match self.log(select) {
                    Ok(found) => DaemonMessage::Log { found },
                    Err(e) => DaemonMessage::Error {
                        message: e.to_string(),
                    },
                };
                let _ = send(&mut writer, &reply);
            }
            Ok(Some(ClientMessage::Subscribe)) => self.serve_subscriber(&mut writer),
            Ok(Some(ClientMessage::Cancel { job, grace_s })) => {
                let _ = send(&mut writer, &self.cancel(job, grace_s));
            }
            Ok(Some(ClientMessage::Exited { .. })) => {
                refuse(
                    &mut writer,
                    "expected submit, history, keys, status, why, log, cancel or subscribe, got exited",
                );
            }
            Err(e) => refuse(&mut writer, &format!("bad request: {e}")),
        }
    }

    /// The job or run `select` picks, and its log.
    fn log(&self, select: LogSelect) -> Result<Option<LogFound>, Error> {
        let held = |job: u64| {
            self.scheduler().held(job).map(|(held, started)| LogFound {
                job: Some(job),
                log_path: started.map(|_| held.log.to_string_lossy().into_owned()),
                key: Some(held.key.clone()),
                submitter: Some(held.submitter.clone()),
                started_at: started.map(procfs::rfc3339),
                ..LogFound::default()
            })
        };
        Ok(match select {
            // A job is remembered as ended before it leaves the scheduler, so asking `held` first
            // and `ended` second never misses one that ends in between.
            LogSelect::Job { job } => match held(job) {
                Some(found) => Some(found),
                None => match self.ended_job(job) {
                    Some(Ended::Run(run)) => recorded(self.store().run(run)?),
                    Some(Ended::Recording) => {
                        return Err(Error::new(format!(
                            "job {job} is ending; ask again in a moment"
                        )));
                    }
                    Some(Ended::Unrecorded) => {
                        return Err(Error::new(format!(
                            "job {job} ran, but its run could not be recorded, so its log \
                             cannot be found by number"
                        )));
                    }
                    Some(Ended::NoRun) => Some(LogFound {
                        job: Some(job),
                        never_ran: true,
                        ..LogFound::default()
                    }),
                    None => None,
                },
            },
            LogSelect::Run { run } => recorded(self.store().run(run)?),
            LogSelect::Last { key } => {
                let key = answers::lookup_key(&self.store(), &key)?;
                let running = {
                    let scheduler = self.scheduler();
                    scheduler.last_running(&key)
                };
                let running = running.and_then(|(job, at)| held(job).map(|found| (found, at)));
                let run = self.store().last(&key)?;
                match (running, run) {
                    (Some((_, at)), Some(run)) if run.started_at > procfs::rfc3339(at) => {
                        recorded(Some(run))
                    }
                    (Some((found, _)), _) => Some(found),
                    (None, run) => recorded(run),
                }
            }
            LogSelect::Failed { key, within } => {
                let store = self.store();
                let key = key
                    .map(|key| answers::lookup_key(&store, &key))
                    .transpose()?;
                recorded(store.last_failed(key.as_deref(), within.as_deref())?)
            }
        })
    }

    /// What became of ended job `job`, when it is remembered.
    fn ended_job(&self, job: u64) -> Option<Ended> {
        lock(&self.ended).get(job)
    }

    /// Remembers that job `job` ended as `ended`, for `logs <n>`.
    fn job_ended(&self, job: u64, ended: Ended) {
        lock(&self.ended).insert(job, ended);
    }

    /// Deletes the expired logs. The scheduler stays locked throughout, so no job starts or ends
    /// between reading which run and deleting.
    fn sweep(&self) {
        let scheduler = self.scheduler();
        let running = scheduler.running_logs();
        let ended: HashMap<PathBuf, SystemTime> = match self.store().log_ends() {
            Ok(ends) => ends
                .into_iter()
                .map(|(path, ms)| {
                    let ms = u64::try_from(ms).unwrap_or(0);
                    (PathBuf::from(path), UNIX_EPOCH + Duration::from_millis(ms))
                })
                .collect(),
            Err(e) => {
                eprintln!("taskslash: logs not swept: {e}");
                return;
            }
        };
        let gone = logs::sweep(
            &self.logs,
            &running,
            &ended,
            &self.retention,
            SystemTime::now(),
        );
        drop(scheduler);
        if !gone.is_empty() {
            eprintln!("taskslash: deleted {} expired log(s)", gone.len());
        }
    }

    /// Where job `id` is and, when it waits, why; when it ended since the daemon started, what
    /// became of it.
    fn why(&self, job: u64) -> DaemonMessage {
        let (state, waiting) = self.scheduler().whereabouts(job);
        let (reason, waited_s, position) = match waiting {
            Some((reason, waited_s, position)) => (Some(reason), Some(waited_s), Some(position)),
            None => (None, None, None),
        };
        // Asked after the scheduler, as `log` does: a job is remembered as ended before it leaves.
        let ended = match (state, self.ended_job(job)) {
            (JobState::NotFound, Some(ended)) => match self.job_ended_as(ended) {
                Ok(ended) => Some(ended),
                Err(e) => {
                    return DaemonMessage::Error {
                        message: e.to_string(),
                    };
                }
            },
            _ => None,
        };
        DaemonMessage::Why {
            job,
            state,
            reason,
            waited_s,
            position,
            ended,
        }
    }

    /// Cancels job `job`, under the scheduler lock so it cannot start or end meanwhile. A waiting
    /// job leaves the queue here (its connection's thread tells its client); a running one's client
    /// is sent `cancel` on its connection, at once when it has been sent `start`, else right after.
    fn cancel(&self, job: u64, grace_s: u64) -> DaemonMessage {
        let mut scheduler = self.scheduler();
        let link = lock(&self.links).get(&job).cloned();
        let (state, _) = scheduler.whereabouts(job);
        if let Some(link) = link {
            let mut link = lock(&link);
            if let Some((was, _)) = link.cancel {
                return DaemonMessage::Cancelled {
                    job,
                    was,
                    already: true,
                };
            }
            let was = match state {
                JobState::Waiting => {
                    scheduler.leave(job);
                    self.job_ended(job, Ended::NoRun);
                    Some(Was::Waiting)
                }
                JobState::Running => {
                    link.tell_cancel(job, grace_s);
                    Some(Was::Running)
                }
                // Its connection's thread is ending it.
                JobState::NotFound => None,
            };
            if let Some(was) = was {
                link.cancel = Some((was, grace_s));
                eprintln!("job {job} cancelled (was {})", was.as_str());
                return DaemonMessage::Cancelled {
                    job,
                    was,
                    already: false,
                };
            }
        }
        drop(scheduler);
        let ended = match self.ended_job(job).map(|ended| self.job_ended_as(ended)) {
            Some(Ok(ended)) => Some(ended),
            Some(Err(e)) => {
                return DaemonMessage::Error {
                    message: e.to_string(),
                };
            }
            None => None,
        };
        DaemonMessage::NotCancelled { job, ended }
    }

    /// An ended job as `why` reports it; a run that is not in the store reads as unrecorded.
    fn job_ended_as(&self, ended: Ended) -> Result<JobEnded, Error> {
        Ok(match ended {
            Ended::Run(id) => match self.store().run(id)? {
                Some(run) => JobEnded::Run {
                    run: run.id,
                    key: run.key,
                    exit_code: run.exit_code,
                },
                None => JobEnded::Unrecorded,
            },
            Ended::Recording => JobEnded::Recording,
            Ended::Unrecorded => JobEnded::Unrecorded,
            Ended::NoRun => JobEnded::NeverRan,
        })
    }

    fn admit(&self) {
        self.admit_locked(&mut self.scheduler());
    }

    /// Admits with the scheduler already locked; reads the store for whether a key is seen. A key
    /// whose history cannot be read counts as never seen.
    fn admit_locked(&self, scheduler: &mut Scheduler) {
        scheduler.admit(|key| {
            self.store().seen(key).unwrap_or_else(|e| {
                eprintln!("taskslash: {key} counted as never seen: {e}");
                false
            })
        });
    }

    fn scheduler(&self) -> MutexGuard<'_, Scheduler> {
        lock(&self.scheduler)
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        lock(&self.store)
    }

    /// Samples the machine on every whole `SAMPLE_EVERY` from now, skipping a deadline already
    /// missed, so the period does not drift by the time a sample takes.
    fn sample_forever(&self, mut sampler: Box<dyn MachineSampler>) {
        let mut next = Instant::now();
        loop {
            next += SAMPLE_EVERY;
            let now = Instant::now();
            while next <= now {
                next += SAMPLE_EVERY;
            }
            thread::sleep(next - now);
            let snapshot = sampler.sample();
            {
                let mut scheduler = self.scheduler();
                scheduler.sample(&snapshot);
                self.admit_locked(&mut scheduler);
            }
            *lock(&self.machine) = snapshot;
        }
    }

    fn status(&self) -> Status {
        let machine = lock(&self.machine).clone();
        let (jobs, waiting) = self.scheduler().status();
        Status {
            daemon: Some(DaemonInfo {
                version: crate::VERSION.to_owned(),
                started_at: procfs::rfc3339(self.started_at),
                pid: std::process::id(),
            }),
            machine,
            jobs,
            waiting,
        }
    }
}

fn refuse(writer: &mut UnixStream, message: &str) {
    let _ = send(
        writer,
        &DaemonMessage::Error {
            message: message.to_owned(),
        },
    );
}
