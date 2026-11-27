//! `floor-bench [--runs N] [--taskslash <path>]`: times N starts of each row, spawn to exit, and
//! prints min, median, p95, p99 and max in milliseconds, one line per row, then the `taskslash`
//! row's phases, then one JSON line with every row and phase, the versions, the repo commit and the
//! machine's state at the start (1-minute load, CPU pressure, and whether that reads as busy).
//!
//! Rows: `bare` runs `true` directly, `floor` runs it through `taskslash-floor` (the sibling binary
//! in this target directory), `taskslash` through `taskslash run`. The command's output goes to
//! null, so `taskslash run` takes its pipe path, as `taskslash-floor` always does. When
//! `taskslash status --json` does not exit 0 the `taskslash` row is skipped, not failed.
//!
//! The `taskslash` row runs with `TASKSLASH_PHASES=1`, so each run says on stderr how long each
//! phase it can see took (`taskslash-core`'s `phases` module); `other` is the rest of the start to
//! exit time, the client process starting and exiting.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

const DEFAULT_RUNS: usize = 100;
/// The phases `taskslash run` reports, in order.
const PHASES: [&str; 6] = ["connect", "admit", "launch", "command", "drain", "report"];
/// A machine is busy, and its numbers read high, past either: the 1-minute load average, or the
/// CPU pressure `some avg10` in percent.
const BUSY_LOAD: f64 = 2.0;
const BUSY_CPU_PRESSURE: f64 = 5.0;
const USAGE: &str = "usage: floor-bench [--runs N] [--taskslash <path>]";

/// The value at percentile `p` (0 to 100) of `sorted` (ascending), by nearest rank: the smallest
/// sample with at least `p`% of the samples at or below it. `sorted` is never empty: `--runs` is
/// at least 1 and a failed run ends the benchmark.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    // The rank is ceil(p * n / 100), at least 1, multiplied before the divide so a whole-number
    // rank (95 of 100) stays whole.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "sample counts are far below 2^52, so the casts are exact"
    )]
    let rank = (p * n as f64 / 100.0).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

struct Stats {
    min: f64,
    median: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl Stats {
    fn of(samples: &mut [f64]) -> Stats {
        samples.sort_by(f64::total_cmp);
        Stats {
            min: percentile(samples, 0.0),
            median: percentile(samples, 50.0),
            p95: percentile(samples, 95.0),
            p99: percentile(samples, 99.0),
            max: percentile(samples, 100.0),
        }
    }
}

/// Each phase's name and stats, in order.
type PhaseStats = Vec<(&'static str, Stats)>;

enum Outcome {
    Timed(Stats),
    Skipped(&'static str),
}

struct Row {
    name: &'static str,
    outcome: Outcome,
    /// The run's phases, the `taskslash` row's only.
    phases: PhaseStats,
}

/// Times `runs` starts of `argv`, spawn to exit, in milliseconds. Any run that does not exit 0 is
/// an error: a timing of a failed start is not a start.
fn time(argv: &[String], runs: usize) -> Result<Stats, String> {
    Ok(time_phases(argv, runs, false)?.0)
}

/// `time`, and with `phases` each phase's samples from the `taskslash: phases` line the run
/// writes on stderr, in `PHASES` order and then `other` (the rest of the run's time). A run with
/// no such line is an error.
fn time_phases(argv: &[String], runs: usize, phases: bool) -> Result<(Stats, PhaseStats), String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut samples = Vec::with_capacity(runs);
    let mut phase_samples: Vec<Vec<f64>> = vec![Vec::with_capacity(runs); PHASES.len() + 1];
    for _ in 0..runs {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if phases {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        if phases {
            command.env("TASKSLASH_PHASES", "1");
        }
        let started = Instant::now();
        let output = command
            .output()
            .map_err(|e| format!("cannot start {program}: {e}"))?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        if !output.status.success() {
            return Err(format!("{} exited with {}", argv.join(" "), output.status));
        }
        samples.push(elapsed);
        if phases {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let took = parse_phases(&stderr)
                .ok_or_else(|| format!("{} wrote no phases line: {stderr}", argv.join(" ")))?;
            let mut seen = 0.0;
            for (i, ms) in took.iter().enumerate() {
                phase_samples[i].push(*ms);
                seen += ms;
            }
            phase_samples[PHASES.len()].push(elapsed - seen);
        }
    }
    let names = PHASES.iter().copied().chain(["other"]);
    let phase_stats = if phases {
        names
            .zip(phase_samples.iter_mut())
            .map(|(name, s)| (name, Stats::of(s)))
            .collect()
    } else {
        Vec::new()
    };
    Ok((Stats::of(&mut samples), phase_stats))
}

/// Each of `PHASES` from the last `taskslash: phases ...` line in `stderr`, in that order; a phase
/// the line leaves out (the run never reached it) is 0. `None` when there is no such line.
fn parse_phases(stderr: &str) -> Option<[f64; PHASES.len()]> {
    let line = stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("taskslash: phases "))?;
    let mut took = [0.0; PHASES.len()];
    for word in line.split_whitespace() {
        if let Some((name, ms)) = word.split_once('=')
            && let Some(i) = PHASES.iter().position(|p| *p == name)
        {
            took[i] = ms.parse().ok()?;
        }
    }
    Some(took)
}

/// The first line `argv` prints on stdout when it exits 0, else `None`.
fn first_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

fn answers(program: &str) -> bool {
    Command::new(program)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// CPU pressure `some avg10` from `/proc/pressure/cpu`, in percent.
fn cpu_pressure() -> Option<f64> {
    fs::read_to_string("/proc/pressure/cpu")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("some "))?
        .split_whitespace()
        .find_map(|w| w.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// Whether a machine at `load` and `pressure` is too busy for comparable numbers. One that
/// cannot be read is not called busy.
fn busy(load: Option<f64>, pressure: Option<f64>) -> bool {
    load.is_some_and(|l| l > BUSY_LOAD) || pressure.is_some_and(|p| p > BUSY_CPU_PRESSURE)
}

fn load_1m() -> Option<f64> {
    fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `{"min_ms":...,"max_ms":...}` for `s`.
fn stats_json(s: &Stats) -> String {
    format!(
        "{{\"min_ms\":{:.3},\"median_ms\":{:.3},\"p95_ms\":{:.3},\"p99_ms\":{:.3},\"max_ms\":{:.3}}}",
        s.min, s.median, s.p95, s.p99, s.max
    )
}

fn json_or_null(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), json_string)
}

struct Options {
    runs: usize,
    taskslash: String,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        runs: DEFAULT_RUNS,
        taskslash: "taskslash".to_string(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                let value = args.next().ok_or("--runs needs a number")?;
                options.runs = value
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or(format!("--runs: not a positive number: {value}"))?;
            }
            "--taskslash" => options.taskslash = args.next().ok_or("--taskslash needs a path")?,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

fn floor_binary() -> Result<PathBuf, String> {
    let me = env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let floor = me.with_file_name("taskslash-floor");
    if floor.is_file() {
        Ok(floor)
    } else {
        Err(format!(
            "no {} (build it first: cargo build --release --bins)",
            floor.display()
        ))
    }
}

fn run(options: &Options) -> Result<(), String> {
    let floor = floor_binary()?;
    // Read before the first start, so the runs' own load is not counted.
    let load = load_1m();
    let pressure = cpu_pressure();
    let busy = busy(load, pressure);
    let strings = |argv: &[&str]| argv.iter().map(ToString::to_string).collect::<Vec<_>>();
    let mut rows = vec![
        Row {
            name: "bare",
            outcome: Outcome::Timed(time(&strings(&["true"]), options.runs)?),
            phases: Vec::new(),
        },
        Row {
            name: "floor",
            outcome: Outcome::Timed(time(
                &strings(&[&floor.to_string_lossy(), "--", "true"]),
                options.runs,
            )?),
            phases: Vec::new(),
        },
    ];
    let taskslash = options.taskslash.as_str();
    let answering = answers(taskslash);
    let (outcome, phases) = if answering {
        let (stats, phases) = time_phases(
            &strings(&[
                taskslash,
                "run",
                "--name",
                "overhead-probe",
                "--cpu",
                "0.1",
                "--mem",
                "10",
                "--",
                "true",
            ]),
            options.runs,
            true,
        )?;
        (Outcome::Timed(stats), phases)
    } else {
        (Outcome::Skipped("daemon not answering"), Vec::new())
    };
    rows.push(Row {
        name: "taskslash",
        outcome,
        phases,
    });
    let state = format!(
        "machine: load {} cpu pressure {}%{}",
        load.map_or_else(|| "?".to_string(), |l| format!("{l:.2}")),
        pressure.map_or_else(|| "?".to_string(), |p| format!("{p:.2}")),
        if busy {
            "  BUSY: the numbers read high, do not compare them"
        } else {
            ""
        }
    );
    println!("{state}");

    let mut json_rows = Vec::new();
    for row in &rows {
        match &row.outcome {
            Outcome::Timed(s) => {
                println!(
                    "{:<10} min {:>8.3}  median {:>8.3}  p95 {:>8.3}  p99 {:>8.3}  max {:>8.3}  ms ({} runs)",
                    row.name, s.min, s.median, s.p95, s.p99, s.max, options.runs
                );
                let mut phases = Vec::new();
                for (name, p) in &row.phases {
                    println!(
                        "  {name:<8} min {:>8.3}  median {:>8.3}  p95 {:>8.3}  p99 {:>8.3}  max {:>8.3}  ms",
                        p.min, p.median, p.p95, p.p99, p.max
                    );
                    phases.push(format!("{}:{}", json_string(name), stats_json(p)));
                }
                let phases = if phases.is_empty() {
                    String::new()
                } else {
                    format!(",\"phases\":{{{}}}", phases.join(","))
                };
                let stats = stats_json(s);
                json_rows.push(format!(
                    "{{\"name\":{},{}{phases}}}",
                    json_string(row.name),
                    &stats[1..stats.len() - 1]
                ));
            }
            Outcome::Skipped(why) => {
                println!("{:<10} skipped: {why}", row.name);
                json_rows.push(format!(
                    "{{\"name\":{},\"skipped\":{}}}",
                    json_string(row.name),
                    json_string(&format!("skipped: {why}"))
                ));
            }
        }
    }
    let taskslash_version = answering
        .then(|| first_line(taskslash, &["--version"]))
        .flatten();
    let commit = first_line("git", &["rev-parse", "--short", "HEAD"])
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let number = |v: Option<f64>| v.map_or_else(|| "null".to_string(), |v| format!("{v:.2}"));
    println!(
        "{{\"runs\":{},\"floor_version\":{},\"taskslash_version\":{},\"commit\":{},\"load_1m\":{},\"cpu_pressure_some_avg10\":{},\"busy\":{busy},\"rows\":[{}]}}",
        options.runs,
        json_string(env!("CARGO_PKG_VERSION")),
        json_or_null(taskslash_version.as_deref()),
        json_string(&commit),
        number(load),
        number(pressure),
        json_rows.join(",")
    );
    Ok(())
}

fn main() -> ExitCode {
    let options = match parse(env::args().skip(1)) {
        Ok(options) => options,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("floor-bench: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("floor-bench: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{busy, parse_phases, percentile};

    #[test]
    fn phases_are_read_from_the_last_phases_line_in_order() {
        let stderr = "job 3 waiting\ntaskslash: phases connect=0.1 admit=0.5 launch=0.2 command=25.0 drain=0.01 report=2.5 ms\n";
        assert_eq!(parse_phases(stderr), Some([0.1, 0.5, 0.2, 25.0, 0.01, 2.5]));
        // A phase the run never reached reads 0; an unknown one is skipped.
        assert_eq!(
            parse_phases("taskslash: phases connect=1 later=9 ms"),
            Some([1.0, 0.0, 0.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(parse_phases("no phases here"), None);
        assert_eq!(parse_phases("taskslash: phases connect=x ms"), None);
    }

    #[test]
    fn a_machine_is_busy_past_the_load_or_the_cpu_pressure_and_not_when_unread() {
        assert!(!busy(Some(0.5), Some(0.0)));
        assert!(busy(Some(2.5), Some(0.0)));
        assert!(busy(Some(0.5), Some(12.0)));
        assert!(!busy(None, None));
    }

    #[test]
    fn single_sample_is_every_percentile() {
        for p in [0.0, 50.0, 95.0, 99.0, 100.0] {
            assert_eq!(percentile(&[7.0], p), 7.0);
        }
    }

    #[test]
    fn odd_count_median_is_the_middle() {
        let s = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&s, 50.0), 3.0);
        assert_eq!(percentile(&s, 95.0), 5.0);
        assert_eq!(percentile(&s, 0.0), 1.0);
    }

    #[test]
    fn even_count_median_is_the_lower_middle() {
        let s = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&s, 50.0), 2.0);
        assert_eq!(percentile(&s, 75.0), 3.0);
        assert_eq!(percentile(&s, 99.0), 4.0);
    }

    #[test]
    fn hundred_samples_rank_exactly() {
        let s: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&s, 95.0), 95.0);
        assert_eq!(percentile(&s, 99.0), 99.0);
        assert_eq!(percentile(&s, 100.0), 100.0);
    }
}
