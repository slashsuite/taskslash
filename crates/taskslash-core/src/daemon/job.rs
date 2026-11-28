//! One job's connection: queued, started, measured and recorded.

use super::Daemon;
use super::ended::Ended;
use super::orphan::{self, Orphan, Started};
use crate::admission::{Reason, ordinal};
use crate::estimate::{Source, estimate};
use crate::logs;
use crate::measure::{self, Cost, Sampler};
use crate::order::WAIT_CAP;
use crate::procfs::rfc3339;
use crate::protocol::{
    Class, ClientMessage, DaemonMessage, JobEvent, Kind, Launch, Was, receive, send,
};
use crate::scheduler::{Job, lock};
use crate::store::{self, Finished};
use std::io::BufReader;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How often a waiting connection checks that its client is still there.
const WAIT_POLL: Duration = Duration::from_millis(100);
/// How often a waiting client is told again why.
const TELL_EVERY: Duration = Duration::from_secs(15);

/// A `submit`: the job's name, kind, command and directory, and what the caller said it costs
/// (`run --cpu`, `--mem`), each of which replaces the estimate's figure; its class and submitter.
pub(super) struct Submitted<'a> {
    pub name: Option<&'a str>,
    pub kind: Kind,
    pub argv: &'a [String],
    pub cwd: &'a str,
    pub cpu: Option<f64>,
    pub mem_mib: Option<u64>,
    pub class: Class,
    pub submitter: &'a str,
}

/// A held job's connection as `cancel` reaches it: a writer of its own, whether the client has been
/// sent `start`, and the cancel, once there is one (where the job was, and the grace period).
pub(super) struct Link {
    writer: UnixStream,
    started: bool,
    pub(super) cancel: Option<(Was, u64)>,
}

impl Link {
    /// Tells the client to stop its command, once it has been sent `start`; until then `start`'s
    /// sender does it right after (`cancel` is set by then).
    pub(super) fn tell_cancel(&mut self, job: u64, grace_s: u64) {
        if self.started {
            let _ = send(&mut self.writer, &DaemonMessage::Cancel { job, grace_s });
        }
    }
}

/// Takes job `id`'s link out of the registry on every way out of `serve_job`.
struct Linked<'a> {
    daemon: &'a Daemon,
    id: u64,
}

impl Drop for Linked<'_> {
    fn drop(&mut self) {
        lock(&self.daemon.links).remove(&self.id);
    }
}

impl Daemon {
    /// A job: queued until `admit` starts it (`waiting` when queued and every 15 s), then
    /// `start`, sampling while it runs, and a row when `exited` arrives (and `recorded`) or when the
    /// connection drops (no exit code, once what is left of the command the client reported with
    /// `running` has been stopped). A connection that drops while waiting leaves the queue.
    pub(super) fn serve_job(
        &self,
        reader: &mut BufReader<UnixStream>,
        writer: &mut UnixStream,
        submitted: Submitted<'_>,
    ) {
        let Submitted {
            name,
            kind,
            argv,
            cwd,
            cpu,
            mem_mib,
            class,
            submitter,
        } = submitted;
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        // The daemon's pid keeps unit names apart across restarts and daemons on other sockets.
        let unit = format!("taskslash-{}-{id}", process::id());
        let log = logs::log_path(&self.logs, &unit);
        let key = store::key(name, argv);
        self.events.publish(JobEvent::Queued {
            job: id,
            key: key.clone(),
            kind,
            class,
            submitter: submitter.to_owned(),
            cwd: cwd.to_owned(),
            argv: argv.to_vec(),
            at: now(),
        });
        let costs = self.store().costs(&key).unwrap_or_else(|e| {
            eprintln!("taskslash: job {id} estimated from defaults: {e}");
            Vec::new()
        });
        let mut estimate = estimate(&costs, kind);
        if let Some(cpu) = cpu {
            estimate.cpu_threads = cpu;
            estimate.source.cpu = Source::Given;
        }
        if let Some(mem_mib) = mem_mib {
            estimate.mem_mib = mem_mib;
            estimate.source.mem = Source::Given;
        }
        let job = Job {
            id,
            key: key.clone(),
            kind,
            estimate,
            vouched: cpu.is_some() && mem_mib.is_some(),
            class,
            submitter: submitter.to_owned(),
            log: log.clone(),
        };
        let Ok(link_writer) = writer.try_clone() else {
            eprintln!("taskslash: job {id} refused: cannot clone its connection");
            self.never_started(id, &key);
            return;
        };
        let link = Arc::new(Mutex::new(Link {
            writer: link_writer,
            started: false,
            cancel: None,
        }));
        lock(&self.links).insert(id, Arc::clone(&link));
        let _linked = Linked { daemon: self, id };
        let (start, started) = mpsc::channel();
        let (waiting, never_seen) = {
            let mut scheduler = self.scheduler();
            scheduler.submit(job, start);
            self.admit_locked(&mut scheduler);
            (scheduler.waiting(id), scheduler.never_seen(id))
        };
        let assumed_threads = never_seen.then(|| kind.default_cpu_threads());
        // Started in the pass above: `admit` already sent its `Admitted`. A waiting job takes it in
        // the loop below, which a pass on another thread can send to at any moment.
        let mut admitted = if waiting.is_none() {
            started.try_recv().ok()
        } else {
            None
        };
        if let Some((reason, _, position)) = waiting {
            eprintln!("job {id} waiting ({}): {reason}", ordinal(position));
            let _ = send(
                writer,
                &DaemonMessage::Waiting {
                    job: Some(id),
                    reason,
                    waited_s: 0,
                    position,
                    never_seen,
                    assumed_threads,
                },
            );
            let mut told = Instant::now();
            loop {
                let got = started.recv_timeout(WAIT_POLL);
                // Cancelled while waiting: `cancel` took it off the queue, so it cannot have been
                // admitted (and `got` is not `Ok`).
                if got.is_err()
                    && let Some((Was::Waiting, grace_s)) = lock(&link).cancel
                {
                    let _ = send(writer, &DaemonMessage::Cancel { job: id, grace_s });
                    self.never_started(id, &key);
                    return;
                }
                match got {
                    Ok(sent) => {
                        admitted = Some(sent);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) if told.elapsed() >= TELL_EVERY => {
                        told = Instant::now();
                        // The never-run hint goes on the first line only, so it is not sent again.
                        let now = self.scheduler().waiting(id);
                        if let Some((reason, waited_s, position)) = now {
                            let _ = send(
                                writer,
                                &DaemonMessage::Waiting {
                                    job: Some(id),
                                    reason,
                                    waited_s,
                                    position,
                                    never_seen: false,
                                    assumed_threads: None,
                                },
                            );
                        }
                    }
                    Err(RecvTimeoutError::Timeout) if hung_up(writer) => {
                        // Not on the queue any more means it started meanwhile: run it as dropped.
                        // Remembered while the scheduler is locked, as `finish` below does.
                        let left = {
                            let mut scheduler = self.scheduler();
                            let left = scheduler.leave(id);
                            if left {
                                self.job_ended(id, Ended::NoRun);
                            }
                            left
                        };
                        if left {
                            eprintln!("job {id} left the queue");
                            self.never_started(id, &key);
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        self.never_started(id, &key);
                        return;
                    }
                }
            }
        }

        let capped = admitted.and_then(|a| a.capped);
        eprintln!("{}", started_line(id, class, argv, capped.as_ref()));
        let sampler = match self.launch {
            Launch::Scope => measure::cgroup_dir(&unit).map(Sampler::start),
            Launch::Plain => None,
        };
        if let Some(sampler) = &sampler {
            self.scheduler().set_live(id, sampler.live());
        }
        let log = log.to_string_lossy().into_owned();
        self.events.publish(JobEvent::Started {
            job: id,
            unit: unit.clone(),
            launch: self.launch,
            log: log.clone(),
            at: now(),
        });
        // What the daemon stops should the client go before it says how the command ended.
        let mut orphan = Orphan {
            scope: match self.launch {
                Launch::Scope => measure::cgroup_dir(&unit),
                Launch::Plain => None,
            },
            process: None,
        };
        let start = DaemonMessage::Start {
            job: id,
            unit,
            launch: self.launch,
            log: log.clone(),
            running: true,
        };
        // Sent under the link's lock, so a `cancel` that came first follows it and one that comes
        // after finds `started` set.
        let sent = {
            let mut link = lock(&link);
            let sent = send(writer, &start);
            link.started = true;
            if let Some((_, grace_s)) = link.cancel {
                link.tell_cancel(id, grace_s);
            }
            sent
        };
        let reported = sent.ok().and_then(|()| {
            loop {
                match receive(reader).ok().flatten() {
                    Some(ClientMessage::Running { job, pid, group }) if job == id => {
                        orphan.process = Started::new(pid, group);
                    }
                    Some(ClientMessage::Exited {
                        job,
                        code,
                        cached,
                        moon_hash,
                    }) if job == id => break Some((code, cached, moon_hash)),
                    _ => break None,
                }
            }
        });
        let (exit_code, cached, moon_hash) = match reported {
            Some((code, cached, moon_hash)) => (Some(code), cached, moon_hash),
            None => {
                // The client went without saying how its command ended. A cancel already under way
                // keeps the grace it gave.
                let grace = lock(&link)
                    .cancel
                    .map_or(orphan::GRACE, |(_, grace_s)| Duration::from_secs(grace_s));
                let stopped = orphan.stop(grace);
                eprintln!("job {id}: its client went away; {}", stopped.as_str());
                (None, None, None)
            }
        };
        // Remembered as ending while the scheduler is still locked, so `logs <n>` always finds it.
        let finished = {
            let mut scheduler = self.scheduler();
            let finished = scheduler.finish(id);
            if finished.is_some() {
                self.job_ended(id, Ended::Recording);
            }
            finished
        };
        let Some(running) = finished else {
            self.never_started(id, &key);
            return;
        };
        let wall_ms = u64::try_from(running.clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let cost = sampler.map(Sampler::finish).unwrap_or_else(Cost::default);
        match exit_code {
            Some(code) => eprintln!("job {id} exited {code}"),
            None => eprintln!("job {id} dropped"),
        }
        let finished = Finished {
            key: &key,
            kind,
            argv,
            cwd,
            class,
            submitter,
            launch: self.launch,
            started_at: running.started_at,
            wall_ms,
            cost,
            exit_code,
            pressure: running.pressure(),
            cached,
            moon_hash: moon_hash.clone(),
            log_path: Some(&log),
        };
        let recorded = self.store().record(&finished);
        self.job_ended(
            id,
            recorded
                .as_ref()
                .map_or(Ended::Unrecorded, |&run| Ended::Run(run)),
        );
        self.events.publish(JobEvent::Finished {
            job: id,
            key: key.clone(),
            exit_code,
            wall_ms,
            cached,
            moon_hash,
            run: recorded.as_ref().ok().copied(),
            log: Some(log.clone()),
            at: now(),
        });
        // After the row, so a job that finished counts as seen when the next one is considered.
        self.admit();
        match recorded {
            // The client may already be gone; the row is written either way.
            Ok(_) if exit_code.is_some() => {
                let _ = send(writer, &DaemonMessage::Recorded { job: id });
            }
            Ok(_) => {}
            Err(e) => eprintln!("taskslash: job {id} not recorded: {e}"),
        }
    }
}

impl Daemon {
    /// The `finished` of a job that leaves before it runs, so every `queued` has one.
    fn never_started(&self, id: u64, key: &str) {
        self.job_ended(id, Ended::NoRun);
        self.events.publish(JobEvent::Finished {
            job: id,
            key: key.to_owned(),
            exit_code: None,
            wall_ms: 0,
            cached: None,
            moon_hash: None,
            run: None,
            log: None,
            at: now(),
        });
    }
}

/// The daemon's line for a job that starts, which says so when the wait cap let it past what
/// still held it.
fn started_line(id: u64, class: Class, argv: &[String], capped: Option<&Reason>) -> String {
    let argv = argv.join(" ");
    match capped {
        None => format!("job {id} started ({class}): {argv}"),
        Some(reason) => format!(
            "job {id} started ({class}) past its {} min wait cap: {reason}: {argv}",
            WAIT_CAP.as_secs() / 60
        ),
    }
}

fn now() -> String {
    rfc3339(SystemTime::now())
}

/// Whether the client on `stream` hung up (or sent something, which a waiting client never does).
fn hung_up(stream: &UnixStream) -> bool {
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: `fd` is one valid `pollfd` for the duration of the call, and the timeout is 0.
    let ready = unsafe { libc::poll(&raw mut fd, 1, 0) };
    // -1 (EINTR and the like) is not a hang up; the next poll asks again.
    ready > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::Gate;

    #[test]
    fn the_start_line_says_when_a_job_started_past_its_wait_cap_and_why() {
        let argv = ["cargo".to_owned(), "test".to_owned()];
        assert_eq!(
            started_line(12, Class::Normal, &argv, None),
            "job 12 started (normal): cargo test"
        );
        let held = Reason::Gate {
            gate: Gate::CpuPressure,
            value: 52.0,
            limit: 40.0,
            mount: None,
        };
        assert_eq!(
            started_line(12, Class::Normal, &argv, Some(&held)),
            "job 12 started (normal) past its 3 min wait cap: \
             cpu pressure 52% over the 40% gate: cargo test"
        );
    }
}
