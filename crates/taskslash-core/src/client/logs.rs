//! `taskslash logs`: prints a job's or a run's log, and follows a running job's to its end. A
//! header on stderr says first whose log it is, so the log on stdout stays as the command wrote it.

use super::{Connection, ask_or_read};
use crate::Error;
use crate::procfs::local_time;
use crate::protocol::{ClientMessage, DaemonMessage, JobState, LogFound, LogSelect, worktree};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// How often `-f` looks for more of the log and whether its job has ended.
const FOLLOW_EVERY: Duration = Duration::from_millis(200);

/// Writes the log `select` picks to `out`, unchanged, after a header on `err` naming the run or
/// job, its key, worktree, exit and time. With `follow`, a running job's log is followed until the
/// job ends. An `Err` (exit 1) when nothing matches, the job has not started or never ran, the run
/// predates logs, or retention deleted its log.
pub fn logs(
    socket: &Path,
    select: &LogSelect,
    follow: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<(), Error> {
    let found = match ask_or_read(
        socket,
        &ClientMessage::Log {
            select: select.clone(),
        },
    )? {
        DaemonMessage::Log { found } => found,
        other => return Err(Error::new(format!("unexpected reply {other:?}"))),
    };
    let Some(found) = found else {
        return Err(Error::new(nothing(select)));
    };
    if let LogSelect::Failed {
        within: Some(_), ..
    } = select
        && found.submitter.is_none()
    {
        let _ = writeln!(
            err,
            "(this daemon is older: searched the whole machine; reinstall it)"
        );
    }
    if found.never_ran {
        return Err(Error::new(format!(
            "job {} never ran (it ended before it started), so it has no log",
            found.job.unwrap_or_default()
        )));
    }
    let written = match &found {
        LogFound {
            job: Some(job),
            log_path: Some(path),
            ..
        } => {
            let _ = writeln!(err, "{}", header(&found, local_time));
            running(socket, *job, Path::new(path), follow, out)
        }
        LogFound {
            job: Some(job),
            log_path: None,
            ..
        } => return Err(Error::new(format!("job {job} has not started"))),
        LogFound {
            run,
            log_path: None,
            ..
        } => {
            return Err(Error::new(format!(
                "run {} has no log",
                run.unwrap_or_default()
            )));
        }
        LogFound {
            run,
            log_path: Some(path),
            ..
        } => {
            let run = run.unwrap_or_default();
            let mut file = File::open(path).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Error::new(format!("the log of run {run} has expired")),
                _ => Error::new(format!("cannot read {path}: {e}")),
            })?;
            let _ = writeln!(err, "{}", header(&found, local_time));
            io::copy(&mut file, out).and_then(|_| out.flush())
        }
    };
    match written {
        // A reader that stopped early (`| head`) is not a failure.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(Error::new(format!("cannot write the log: {e}"))),
        Ok(()) => Ok(()),
    }
}

/// What to say when the daemon found nothing for `select`, and what to try next.
fn nothing(select: &LogSelect) -> String {
    match select {
        LogSelect::Job { job } => format!(
            "no job {job} (the daemon restarted or forgot it); find the run with taskslash \
             history <key>, then taskslash logs --run <id>"
        ),
        LogSelect::Run { run } => format!("no run {run}"),
        LogSelect::Last { key } => format!("no run of {key}"),
        LogSelect::Failed { key, within } => {
            let of = key.as_ref().map(|k| format!(" of {k}")).unwrap_or_default();
            match within {
                None => format!("no failed run{of}"),
                Some(within) => {
                    let again = key.as_ref().map(|k| format!(" {k}")).unwrap_or_default();
                    format!(
                        "no failed run{of} in {}; taskslash logs --failed{again} --all searches \
                         the whole machine",
                        worktree(within)
                    )
                }
            }
        }
    }
}

/// The line `logs` prints on stderr before a log: `run 3140 <key> in <worktree>, exit 2,
/// 2026-11-17 20:22` for a recorded run, `job 12 <key> in <worktree>, running since 20:22` for a
/// running job. What an older daemon left out is left out. `local` turns an RFC 3339 UTC time into
/// the local date and time of day.
fn header(found: &LogFound, local: impl Fn(&str) -> Option<(String, String)>) -> String {
    let mut line = match (found.job, found.run) {
        (Some(job), _) => format!("job {job}"),
        (None, run) => format!("run {}", run.unwrap_or_default()),
    };
    if let Some(key) = &found.key {
        line.push_str(&format!(" {key}"));
    }
    if let Some(submitter) = &found.submitter {
        line.push_str(&format!(" in {}", worktree(submitter)));
    }
    let when = found.started_at.as_deref().and_then(local);
    if found.job.is_some() {
        if found.key.is_some() {
            line.push_str(", running");
            if let Some((_, time)) = when {
                line.push_str(&format!(" since {time}"));
            }
        }
    } else if found.submitter.is_some() {
        match found.exit_code {
            Some(code) => line.push_str(&format!(", exit {code}")),
            None => line.push_str(", no exit code"),
        }
        if let Some((date, time)) = when {
            line.push_str(&format!(", {date} {time}"));
        }
    }
    line
}

/// Writes what job `job`'s log at `path` holds; with `follow`, then what is added until the job
/// ends. A log its client has not created yet reads as empty.
fn running(
    socket: &Path,
    job: u64,
    path: &Path,
    follow: bool,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut file = File::open(path).ok();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Asked before reading, so whatever the job wrote before it ended is read after.
        let ended = !follow || !holds(socket, job);
        if file.is_none() {
            file = File::open(path).ok();
        }
        if let Some(file) = file.as_mut() {
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
            out.flush()?;
        }
        if ended {
            return Ok(());
        }
        thread::sleep(FOLLOW_EVERY);
    }
}

/// Whether the daemon still holds job `job`. A daemon that cannot be asked holds nothing.
fn holds(socket: &Path, job: u64) -> bool {
    let Ok(mut daemon) = Connection::open(socket) else {
        return false;
    };
    !matches!(
        daemon.ask(&ClientMessage::Why { job }),
        Ok(DaemonMessage::Why {
            state: JobState::NotFound,
            ..
        }) | Err(_)
    )
}

#[cfg(test)]
mod tests {
    use super::{LogFound, LogSelect, header, local_time, logs};
    use crate::procfs::{epoch_secs, rfc3339};
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, UNIX_EPOCH};

    /// A fixed "local" time, so the header does not depend on the machine's zone.
    fn fixed(_: &str) -> Option<(String, String)> {
        Some(("2026-11-17".to_owned(), "20:22".to_owned()))
    }

    #[test]
    fn a_runs_header_names_it_its_key_worktree_exit_and_time() {
        let run = LogFound {
            run: Some(3140),
            log_path: Some("/l".into()),
            key: Some("taskslash/test".into()),
            submitter: Some("/home/j/dev/taskslash-wt".into()),
            exit_code: Some(2),
            started_at: Some("2026-11-17T18:22:00.000Z".into()),
            ..LogFound::default()
        };
        assert_eq!(
            header(&run, fixed),
            "run 3140 taskslash/test in taskslash-wt, exit 2, 2026-11-17 20:22"
        );
        let gone = LogFound {
            exit_code: None,
            ..run
        };
        assert!(header(&gone, fixed).contains(", no exit code, "));
    }

    #[test]
    fn a_running_jobs_header_says_since_when() {
        let job = LogFound {
            job: Some(12),
            log_path: Some("/l".into()),
            key: Some("k".into()),
            submitter: Some("/w/wt".into()),
            started_at: Some("2026-11-17T18:22:00.000Z".into()),
            ..LogFound::default()
        };
        assert_eq!(header(&job, fixed), "job 12 k in wt, running since 20:22");
    }

    #[test]
    fn an_older_daemons_reply_gives_only_the_number() {
        let job = LogFound {
            job: Some(12),
            log_path: Some("/l".into()),
            ..LogFound::default()
        };
        assert_eq!(header(&job, fixed), "job 12");
    }

    #[test]
    fn the_daemons_times_parse_back_to_the_second() {
        for secs in [0, 951_782_400, 1_763_403_725, 4_102_444_800] {
            let at = UNIX_EPOCH + Duration::from_millis(secs * 1000 + 464);
            assert_eq!(epoch_secs(&rfc3339(at)), Some(secs as i64), "{secs}");
        }
        assert_eq!(epoch_secs("yesterday"), None);
        assert_eq!(epoch_secs("2026-13-01T00:00:00Z"), None);
        let (date, time) = local_time("2026-11-17T18:22:00.000Z").unwrap();
        assert_eq!((date.len(), time.len()), (10, 5), "{date} {time}");
    }

    #[test]
    fn an_older_daemon_searched_the_whole_machine_and_the_client_says_so() {
        let dir = std::env::temp_dir().join(format!("tklog-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("d.sock");
        let log = dir.join("run.log");
        fs::write(&log, "boom\n").unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let reply = format!(
            "{{\"type\":\"log\",\"found\":{{\"job\":null,\"run\":3140,\"log_path\":{:?}}}}}\n",
            log.to_str().unwrap()
        );
        let daemon = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let mut out = stream;
            out.write_all(reply.as_bytes()).unwrap();
            line
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let select = LogSelect::Failed {
            key: None,
            within: Some("/w/mine".into()),
        };
        logs(&socket, &select, false, &mut out, &mut err).unwrap();
        let asked = daemon.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(asked.contains("\"within\":\"/w/mine\""), "{asked}");
        assert_eq!(out, b"boom\n");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "(this daemon is older: searched the whole machine; reinstall it)\nrun 3140\n"
        );
    }
}
