//! Pure parsers for the machine's `/proc` and `/sys` files: a file's text in, a struct out. The
//! sampler reads the files and keeps the previous readings; nothing here touches the file system.
//! Also the one formatter the sampler needs, `rfc3339`.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// One `/proc/pressure/*` file: the `some` and `full` lines' `avg10` and `avg60`, in percent. A
/// missing line (no `full` line on an older kernel, or none at all) reads as 0.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
}

pub fn pressure(text: &str) -> Pressure {
    let mut out = Pressure::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let kind = words.next();
        let (mut avg10, mut avg60) = (0.0, 0.0);
        for word in words {
            if let Some(v) = word.strip_prefix("avg10=") {
                avg10 = v.parse().unwrap_or(0.0);
            } else if let Some(v) = word.strip_prefix("avg60=") {
                avg60 = v.parse().unwrap_or(0.0);
            }
        }
        match kind {
            Some("some") => (out.some_avg10, out.some_avg60) = (avg10, avg60),
            Some("full") => (out.full_avg10, out.full_avg60) = (avg10, avg60),
            _ => {}
        }
    }
    out
}

/// The CPU numbers of a list like `0-3,6-9` (`Cpus_allowed_list`, `/sys/devices/system/cpu/online`).
pub fn cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((low, high)) => {
                if let (Ok(low), Ok(high)) = (low.trim().parse::<usize>(), high.trim().parse()) {
                    cpus.extend(low..=high);
                }
            }
            None => cpus.extend(part.trim().parse::<usize>().ok()),
        }
    }
    cpus
}

/// The CPUs this process may run on, from the `Cpus_allowed_list` line of `/proc/self/status`.
pub fn allowed_cpus(status: &str) -> Vec<usize> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(cpu_list)
        .unwrap_or_default()
}

/// One `cpuN` line of `/proc/stat`, in jiffies: `idle` is idle plus iowait, `total` all fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub cpu: usize,
    pub idle: u64,
    pub total: u64,
}

/// Every per CPU line of `/proc/stat` (not the `cpu` total line).
pub fn cpu_times(stat: &str) -> Vec<CpuTimes> {
    stat.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let cpu = words.next()?.strip_prefix("cpu")?.parse().ok()?;
            // user nice system idle iowait irq softirq steal guest guest_nice. Guest time is
            // already counted in user and nice, so only the first eight are summed.
            let fields: Vec<u64> = words.take(8).filter_map(|w| w.parse().ok()).collect();
            let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
            Some(CpuTimes {
                cpu,
                idle,
                total: fields.iter().sum(),
            })
        })
        .collect()
}

/// The busy share of the CPUs in `allowed` between two readings: the non idle jiffies over all
/// jiffies, summed over those CPUs. 0 when no time passed (or the CPUs are not in both readings).
pub fn busy_share(before: &[CpuTimes], after: &[CpuTimes], allowed: &[usize]) -> f64 {
    let (mut busy, mut total) = (0u64, 0u64);
    for now in after.iter().filter(|t| allowed.contains(&t.cpu)) {
        let Some(then) = before.iter().find(|t| t.cpu == now.cpu) else {
            continue;
        };
        let dt = now.total.saturating_sub(then.total);
        let di = now.idle.saturating_sub(then.idle);
        busy += dt.saturating_sub(di);
        total += dt;
    }
    if total == 0 {
        0.0
    } else {
        (busy as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// `/proc/meminfo`'s totals, in MiB.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub total_mib: u64,
    pub available_mib: u64,
    pub swap_total_mib: u64,
    pub swap_free_mib: u64,
}

pub fn meminfo(text: &str) -> Memory {
    let kib = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Memory {
        total_mib: kib("MemTotal") / 1024,
        available_mib: kib("MemAvailable") / 1024,
        swap_total_mib: kib("SwapTotal") / 1024,
        swap_free_mib: kib("SwapFree") / 1024,
    }
}

/// Whether a `/sys/block` entry is a physical disk: not a loop, RAM, zram or device mapper device.
/// Partitions are not in `/sys/block` at all.
pub fn is_physical_disk(name: &str) -> bool {
    !["loop", "ram", "zram", "dm-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// One `/proc/diskstats` line's counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCounters {
    pub name: String,
    /// Field 10 of the line (field 7 of the kernel's `iostats.rst`), in 512 byte sectors.
    pub sectors_written: u64,
    /// Field 13 of the line (field 10 of `iostats.rst`): milliseconds the device was busy.
    pub io_ticks_ms: u64,
}

/// The lines of `/proc/diskstats` whose device is in `disks`, in `disks`' order.
pub fn diskstats(text: &str, disks: &[String]) -> Vec<DiskCounters> {
    let all: Vec<DiskCounters> = text
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            Some(DiskCounters {
                name: (*words.get(2)?).to_owned(),
                sectors_written: words.get(9)?.parse().ok()?,
                io_ticks_ms: words.get(12)?.parse().ok()?,
            })
        })
        .collect();
    disks
        .iter()
        .filter_map(|disk| all.iter().find(|c| &c.name == disk).cloned())
        .collect()
}

/// The three load averages of `/proc/loadavg`.
pub fn loadavg(text: &str) -> [f64; 3] {
    let mut out = [0.0; 3];
    for (slot, word) in out.iter_mut().zip(text.split_whitespace()) {
        *slot = word.parse().unwrap_or(0.0);
    }
    out
}

/// `at` as RFC 3339 in UTC with milliseconds, `2026-11-16T02:17:44.464Z`.
pub fn rfc3339(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// Seconds since the epoch of `at`, RFC 3339 in UTC as the daemon writes it.
pub fn epoch_secs(at: &str) -> Option<i64> {
    let at = at.strip_suffix('Z').unwrap_or(at);
    let (date, time) = at.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut time = time.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm), as `rfc3339` inverts.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// `at`, RFC 3339 in UTC (`2026-11-17T18:22:05.123Z`), as the local date (`2026-11-17`) and time
/// of day (`20:22`). `None` when it does not parse.
pub fn local_time(at: &str) -> Option<(String, String)> {
    let secs = libc::time_t::try_from(epoch_secs(at)?).ok()?;
    // SAFETY: `tzset` reads TZ and the zone files; `localtime_r` only writes `tm`, which outlives it.
    let tm = unsafe {
        tzset();
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some((
        format!(
            "{:04}-{:02}-{:02}",
            i64::from(tm.tm_year) + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        ),
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
    ))
}

unsafe extern "C" {
    fn tzset();
}
