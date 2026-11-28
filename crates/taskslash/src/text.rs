//! The plain text forms of `status` and `history`.

/// How wide the `where` column of the run table is.
const WHERE_WIDTH: usize = 12;

/// `name` in at most `width` characters: a longer one cut from the left to `..` and its last
/// `width - 2`.
fn cut_left(name: &str, width: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= width {
        return name.to_owned();
    }
    let tail: String = chars[chars.len() - (width - 2)..].iter().collect();
    format!("..{tail}")
}

/// One header line and one line per key: when it last ran, how many runs, the last exit code (`-`
/// when its client went away), the worktree it last ran in, and the key, whole, last.
pub fn keys_text(keys: &[KeySummary]) -> String {
    let mut out = format!(
        "{:<24}  {:>6}  {:>4}  {:<12}  key\n",
        "last_run", "runs", "exit", "where"
    );
    for k in keys {
        out.push_str(&format!(
            "{:<24}  {:>6}  {:>4}  {:<12}  {}\n",
            k.last_started_at,
            k.runs,
            k.last_exit_code.map_or("-".to_owned(), |c| c.to_string()),
            cut_left(worktree(&k.last_submitter), WHERE_WIDTH),
            k.key
        ));
    }
    out
}

use std::time::{SystemTime, UNIX_EPOCH};
use taskslash_core::Cancelled;
use taskslash_core::admission::ordinal;
use taskslash_core::estimate::{Estimate, Source};
use taskslash_core::procfs::{epoch_secs, local_time};
use taskslash_core::protocol::{DaemonInfo, KeySummary, Run, Status, WaitingStatus, Was, worktree};

/// How long, read at a glance: `45s`, `1m05s` past a minute, `2h03m` past an hour.
pub fn duration(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// How long a job that started `started_at` (RFC 3339) has run at `now`; `-` when it cannot be read.
fn running_for(started_at: &str, now: SystemTime) -> String {
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    epoch_secs(started_at)
        .and_then(|at| u64::try_from(at).ok())
        .map_or("-".to_owned(), |at| duration(now.saturating_sub(at) as f64))
}

/// One line per waiting job, as `taskslash why` with no job prints it, or `nothing is waiting`.
pub fn waiting_text(waiting: &[WaitingStatus]) -> String {
    if waiting.is_empty() {
        return "nothing is waiting\n".to_owned();
    }
    let mut out = String::new();
    for w in waiting {
        let reason = if w.reason.is_empty() {
            "not considered yet"
        } else {
            &w.reason
        };
        out.push_str(&format!(
            "job {} {} {}: {reason}\n",
            w.job,
            worktree(&w.submitter),
            w.key
        ));
    }
    out
}

/// The daemon's build, pid and how long it has run at `now` (since when, local time), and when its
/// build is not this one's, that this one differs: after an install, the daemon still running the
/// old build shows here.
fn daemon_line(daemon: Option<&DaemonInfo>, now: SystemTime) -> String {
    let Some(d) = daemon else {
        return format!(
            "daemon    older than this taskslash ({}), which names its build\n",
            taskslash_core::VERSION
        );
    };
    let mut line = format!(
        "daemon    {}, pid {}, up {}",
        d.version,
        d.pid,
        running_for(&d.started_at, now)
    );
    if let Some((date, time)) = local_time(&d.started_at) {
        line.push_str(&format!(" (since {date} {time})"));
    }
    if d.version != taskslash_core::VERSION {
        line.push_str(&format!(
            "; this taskslash is {}, a different build",
            taskslash_core::VERSION
        ));
    }
    line.push('\n');
    line
}

/// A few lines: the daemon, CPU, the three `some avg10` pressures, memory, each disk, each mount, each job
/// (how long it has run at `now`, and its worktree), or that no job runs or waits.
pub fn status_text(status: &Status, now: SystemTime) -> String {
    let m = &status.machine;
    let pct = |share: f64| format!("{:.0}%", share * 100.0);
    let mut out = daemon_line(status.daemon.as_ref(), now);
    out.push_str(&format!(
        "cpu       {} busy on {} threads ({} online), load {:.2} {:.2} {:.2}\n",
        pct(m.cpu.busy),
        m.cpu.threads,
        m.cpu.threads_online,
        m.load[0],
        m.load[1],
        m.load[2]
    ));
    out.push_str(&format!(
        "pressure  cpu {:.2}  memory {:.2}  io {:.2}  (some avg10, %)\n",
        m.cpu.pressure.some_avg10, m.memory.pressure.some_avg10, m.io.pressure.some_avg10
    ));
    out.push_str(&format!(
        "memory    {} MiB available of {} MiB, swap {} of {} MiB free\n",
        m.memory.available_mib, m.memory.total_mib, m.memory.swap_free_mib, m.memory.swap_total_mib
    ));
    for disk in &m.io.disks {
        out.push_str(&format!(
            "disk      {} {} busy, {:.1} MiB/s written\n",
            disk.name,
            pct(disk.busy),
            disk.write_mib_s
        ));
    }
    for mount in &m.mounts {
        out.push_str(&format!(
            "mount     {} {} MiB free of {} MiB ({} used)\n",
            mount.path,
            mount.free_mib,
            mount.total_mib,
            pct(mount.used)
        ));
    }
    for job in &status.jobs {
        let cpu = job
            .cpu_threads
            .map_or("-".to_owned(), |t| format!("{t:.2}"));
        let mem = job.mem_mib.map_or("-".to_owned(), |m| m.to_string());
        out.push_str(&format!(
            "job {}  {}  {}  {} ({}) {cpu} threads {mem} MiB\n",
            job.job,
            running_for(&job.started_at, now),
            worktree(&job.submitter),
            job.key,
            job.class
        ));
    }
    for w in &status.waiting {
        let class = match w.effective_class {
            Some(raised) => format!("{}, as {raised}", w.class),
            None => w.class.to_string(),
        };
        out.push_str(&format!(
            "waiting {}  {}  {} ({}, {class}) {:.0}s ({} in line): {}\n",
            w.job,
            worktree(&w.submitter),
            w.key,
            w.kind,
            w.waited_s,
            ordinal(w.position),
            w.reason
        ));
    }
    if status.jobs.is_empty() && status.waiting.is_empty() {
        out.push_str("no jobs running or waiting\n");
    }
    out
}

/// Two lines: each figure with where it came from, then how many runs fed it.
pub fn estimate_text(e: &Estimate) -> String {
    let from = |source: Source| match source {
        Source::History => "history",
        Source::Default => "default",
        Source::Given => "given",
    };
    let peak = e
        .cpu_peak_5s
        .map_or(String::new(), |p| format!(", peak {p:.2} over 5s"));
    let duration = e.duration_s.map_or("-".to_owned(), |d| format!("{d:.1}s"));
    format!(
        "estimate  cpu {:.2} threads{peak} ({}), memory {} MiB ({}), write {:.2} MiB/s ({}), \
         duration {duration} ({})\nsamples   {} measured, {} calm\n",
        e.cpu_threads,
        from(e.source.cpu),
        e.mem_mib,
        from(e.source.mem),
        e.write_mib_s,
        from(e.source.io),
        from(e.source.duration),
        e.samples.measured,
        e.samples.calm,
    )
}

/// One header line and one line per run, `-` for a cost that was not measured, and the worktree
/// it ran in (cut to 12 characters), so no line is over 120 columns.
pub fn table(runs: &[Run]) -> String {
    const MIB: f64 = (1u64 << 20) as f64;
    let mut out = format!(
        "{:>6}  {:<24}  {:>8}  {:>8}  {:>11}  {:>12}  {:>11}  {:>4}  {:>6}  {}\n",
        "id",
        "started_at",
        "wall",
        "cpu_s",
        "peak_cpu_5s",
        "peak_mem_mib",
        "written_mib",
        "exit",
        "cached",
        "where"
    );
    for run in runs {
        let or_dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
        let mib = |v: Option<i64>| or_dash(v.map(|b| format!("{:.1}", b as f64 / MIB)));
        out.push_str(&format!(
            "{:>6}  {:<24}  {:>8}  {:>8}  {:>11}  {:>12}  {:>11}  {:>4}  {:>6}  {}\n",
            run.id,
            run.started_at,
            format!("{:.1}s", run.wall_ms as f64 / 1000.0),
            or_dash(run.cpu_usec.map(|u| format!("{:.2}", u as f64 / 1e6))),
            or_dash(run.peak_cpu_5s.map(|p| format!("{p:.2}"))),
            mib(run.peak_mem_bytes),
            mib(run.written_bytes),
            or_dash(run.exit_code.map(|c| c.to_string())),
            or_dash(run.cached.map(|c| if c { "yes" } else { "no" }.to_owned())),
            cut_left(worktree(&run.submitter), WHERE_WIDTH),
        ));
    }
    out
}

/// What `taskslash cancel <job>` prints on stdout (`Ok`), or its error message (`Err`, exit 1).
pub fn cancel_text(cancelled: &Cancelled, job: u64, grace_s: u64) -> Result<String, String> {
    match cancelled {
        Cancelled::Now { was, already: true } => Ok(format!(
            "job {job} is already being cancelled (it was {})\n",
            was.as_str()
        )),
        Cancelled::Now {
            was: Was::Waiting, ..
        } => Ok(format!("job {job} cancelled (was waiting)\n")),
        Cancelled::Now { .. } if grace_s == 0 => {
            Ok(format!("job {job} cancelled (was running; SIGKILL sent)\n"))
        }
        Cancelled::Now { .. } => Ok(format!(
            "job {job} cancelled (was running; SIGTERM sent, SIGKILL in {grace_s}s if it has not ended)\n"
        )),
        Cancelled::NotHeld(ended) => {
            let how = ended
                .as_ref()
                .map(|ended| format!("\n  {}", ended.sentence(job)))
                .unwrap_or_default();
            Err(format!(
                "job {job} is not waiting or running{how}\n  (taskslash status lists jobs)"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use taskslash_core::estimate::estimate;
    use taskslash_core::protocol::{Class, JobEnded, JobStatus, Kind, Launch};
    use taskslash_core::sampler::MachineSnapshot;

    fn machine() -> MachineSnapshot {
        let calm = r#"{"some_avg10":0.0,"some_avg60":0.0,"full_avg10":0.0,"full_avg60":0.0}"#;
        serde_json::from_str(&format!(
            r#"{{"sampled_at":"2026-11-17T00:00:00.000Z",
"cpu":{{"threads":16,"threads_online":16,"busy":0.42,"pressure":{calm}}},
"memory":{{"total_mib":65536,"available_mib":49152,"swap_total_mib":0,"swap_free_mib":0,"pressure":{calm}}},
"io":{{"pressure":{calm},"disks":[]}},
"mounts":[],
"load":[3.5,2.0,1.0]}}"#
        ))
        .unwrap()
    }

    fn waiting(job: u64, key: &str, reason: &str) -> WaitingStatus {
        WaitingStatus {
            job,
            key: key.into(),
            kind: Kind::Build,
            class: Class::Normal,
            effective_class: None,
            submitter: "/home/me/dev/shop.wt-2".into(),
            waited_s: 65.0,
            position: 1,
            reason: reason.into(),
            why: None,
            estimate: estimate(&[], Kind::Build),
        }
    }

    /// 2026-11-17T00:00:00Z.
    fn midnight() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(epoch_secs("2026-11-17T00:00:00.000Z").unwrap() as u64)
    }

    fn run() -> Run {
        Run {
            id: 1,
            key: "t1".into(),
            kind: Kind::Command,
            argv: vec!["sh".into(), "-c".into(), "exit 4".into()],
            cwd: "/work".into(),
            class: Class::Normal,
            submitter: "/work".into(),
            launch: Launch::Plain,
            started_at: "2026-11-16T02:17:44.464Z".into(),
            wall_ms: 1234,
            cpu_usec: None,
            peak_cpu_5s: None,
            peak_mem_bytes: None,
            written_bytes: None,
            exit_code: Some(4),
            machine_cpu_some: None,
            machine_mem_some: None,
            machine_io_some: None,
            cached: None,
            moon_hash: None,
            log_path: None,
        }
    }

    #[test]
    fn a_plain_run_shows_a_dash_for_each_cost() {
        let text = table(&[run()]);
        let line: Vec<&str> = text.lines().nth(1).unwrap().split_whitespace().collect();
        assert_eq!(
            line,
            [
                "1",
                "2026-11-16T02:17:44.464Z",
                "1.2s",
                "-",
                "-",
                "-",
                "-",
                "4",
                "-",
                "work"
            ]
        );
    }

    #[test]
    fn an_estimate_with_no_history_says_default_for_each_figure() {
        let text = estimate_text(&estimate(&[], Kind::Typecheck));
        assert_eq!(
            text,
            "estimate  cpu 2.00 threads (default), memory 3072 MiB (default), \
             write 0.00 MiB/s (default), duration - (default)\n\
             samples   0 measured, 0 calm\n"
        );
    }

    #[test]
    fn a_run_table_line_stays_within_120_columns_for_long_values() {
        let long = Run {
            id: 999_999,
            key: "a-very-long-repository-name/with-a-project:and-a-long-task-name".into(),
            submitter: "/home/someone/dev/a-very-long-worktree-directory-name".into(),
            wall_ms: 9_999_999,
            cpu_usec: Some(99_999_000_000),
            peak_cpu_5s: Some(64.0),
            peak_mem_bytes: Some(99_999 << 20),
            written_bytes: Some(99_999 << 20),
            exit_code: Some(255),
            cached: Some(false),
            ..run()
        };
        let text = table(&[long]);
        for line in text.lines() {
            assert!(
                line.chars().count() <= 120,
                "{} columns: {line}",
                line.len()
            );
        }
        assert!(
            text.lines().nth(1).unwrap().ends_with("  ..ctory-name"),
            "{text}"
        );
    }

    #[test]
    fn a_short_name_is_kept_and_a_long_one_cut_from_the_left() {
        assert_eq!(cut_left("shop", 12), "shop");
        assert_eq!(cut_left("taskslash.tk", 12), "taskslash.tk");
        assert_eq!(cut_left("taskslash.tk-35", 12), "..lash.tk-35");
    }

    #[test]
    fn keys_text_puts_the_key_last_and_whole() {
        let keys = [KeySummary {
            key: "shop/web:test with a long name past every column".into(),
            runs: 12,
            last_started_at: "2026-11-17T09:00:00.000Z".into(),
            last_exit_code: None,
            last_submitter: "/dev/shop".into(),
        }];
        let text = keys_text(&keys);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["last_run", "runs", "exit", "where", "key"]
        );
        assert_eq!(
            lines[1],
            "2026-11-17T09:00:00.000Z      12     -  shop          \
             shop/web:test with a long name past every column"
        );
    }

    #[test]
    fn status_names_the_daemons_build_and_how_long_it_has_run() {
        let daemon = |version: &str| DaemonInfo {
            version: version.into(),
            started_at: "2026-11-16T22:55:00.000Z".into(),
            pid: 4242,
        };
        let same = daemon_line(Some(&daemon(taskslash_core::VERSION)), midnight());
        assert!(
            same.starts_with(&format!(
                "daemon    {}, pid 4242, up 1h05m (since ",
                taskslash_core::VERSION
            )),
            "{same}"
        );
        assert!(!same.contains("different"), "{same}");
        let old = daemon_line(Some(&daemon("0.0.9 (0000000)")), midnight());
        assert!(
            old.starts_with("daemon    0.0.9 (0000000), pid 4242, up 1h05m"),
            "{old}"
        );
        assert!(
            old.ends_with(&format!(
                "; this taskslash is {}, a different build\n",
                taskslash_core::VERSION
            )),
            "{old}"
        );
        let older = daemon_line(None, midnight());
        assert!(
            older.starts_with("daemon    older than this taskslash"),
            "{older}"
        );
    }

    #[test]
    fn status_says_when_nothing_runs_or_waits() {
        let status = Status {
            daemon: None,
            machine: machine(),
            jobs: vec![],
            waiting: vec![],
        };
        let text = status_text(&status, midnight());
        assert!(text.ends_with("\nno jobs running or waiting\n"), "{text}");
    }

    #[test]
    fn a_running_job_shows_how_long_it_has_run_and_its_worktree() {
        let job = JobStatus {
            job: 3,
            key: "shop/web:test".into(),
            kind: Kind::Test,
            class: Class::Normal,
            submitter: "/home/me/dev/shop.wt-1".into(),
            started_at: "2026-11-16T23:59:48.000Z".into(),
            cpu_threads: Some(1.31),
            mem_mib: Some(706),
            estimate: estimate(&[], Kind::Test),
            log_path: "/logs/3.log".into(),
        };
        let status = Status {
            daemon: None,
            machine: machine(),
            jobs: vec![job],
            waiting: vec![waiting(
                4,
                "shop/web:build",
                "cpu pressure 90% over the 40% gate",
            )],
        };
        let text = status_text(&status, midnight());
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.contains(&"job 3  12s  shop.wt-1  shop/web:test (normal) 1.31 threads 706 MiB"),
            "{text}"
        );
        assert!(
            lines.contains(
                &"waiting 4  shop.wt-2  shop/web:build (build, normal) 65s (1st in line): \
                  cpu pressure 90% over the 40% gate"
            ),
            "{text}"
        );
        assert!(!text.contains("no jobs running"), "{text}");
    }

    #[test]
    fn cancel_says_where_the_job_was_and_what_was_sent() {
        let now = |was, already| Cancelled::Now { was, already };
        assert_eq!(
            cancel_text(&now(Was::Running, false), 12, 0).unwrap(),
            "job 12 cancelled (was running; SIGKILL sent)\n"
        );
        assert_eq!(
            cancel_text(&now(Was::Running, true), 12, 10).unwrap(),
            "job 12 is already being cancelled (it was running)\n"
        );
        assert_eq!(
            cancel_text(&Cancelled::NotHeld(Some(JobEnded::NeverRan)), 12, 10).unwrap_err(),
            "job 12 is not waiting or running\n  job 12 ended without running (it left the queue, \
             or its client went before it started)\n  (taskslash status lists jobs)"
        );
    }

    #[test]
    fn durations_read_in_seconds_then_minutes_then_hours() {
        assert_eq!(duration(12.0), "12s");
        assert_eq!(duration(65.0), "1m05s");
        assert_eq!(duration(3720.0), "1h02m");
        assert_eq!(running_for("not a time", midnight()), "-");
    }

    #[test]
    fn why_with_no_job_lists_each_waiting_job_or_says_none_waits() {
        assert_eq!(waiting_text(&[]), "nothing is waiting\n");
        let text = waiting_text(&[
            waiting(
                18,
                "shop/web:test",
                "cpu needs 4.0, free 1.5; held by build (job 3)",
            ),
            waiting(19, "lint", ""),
        ]);
        assert_eq!(
            text,
            "job 18 shop.wt-2 shop/web:test: cpu needs 4.0, free 1.5; held by build (job 3)\n\
             job 19 shop.wt-2 lint: not considered yet\n"
        );
    }
}
