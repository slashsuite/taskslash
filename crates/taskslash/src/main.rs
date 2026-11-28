//! The `taskslash` command line: argv only, over `taskslash-core`.
//!
//! Exit codes: 0 ok, 1 a failure that is not usage, 2 a usage error. `taskslash run` exits with
//! its command's code instead. `taskslash history`, `taskslash status` and `taskslash events` exit 1
//! when no daemon is running, as `run` does; `events` exits 0 when the daemon hangs up. `taskslash
//! top` exits 1 when no daemon is running and 2 when stdout is not a terminal.
//! `taskslash moon` exits 1 when any target did not pass. `taskslash cancel` exits 1 when the job is
//! neither waiting nor running. `taskslash logs` exits 1 when the log it
//! names is not there. `taskslash <name>`, for a name that is not a
//! built-in, runs the plugin `taskslash-<name>` from `PATH` in place of taskslash, so the exit code
//! is the plugin's; exit 2 when there is none.

use clap::error::ErrorKind;
use clap::{ArgGroup, CommandFactory, Parser, Subcommand};
use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;
use taskslash_core::Whereabouts;
use taskslash_core::admission::{Waiting, waiting_line};
use taskslash_core::plugin;
use taskslash_core::protocol::{Class, Kind, LogSelect};
use taskslash_core::{Error, HISTORY_LIMIT};
use text::{cancel_text, estimate_text, keys_text, status_text, table, waiting_text};

mod text;
mod top;

/// Admission control for builds and tests on this machine.
#[derive(Parser)]
#[command(name = "taskslash", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon that admits and records jobs. Runs until killed.
    Daemon,
    /// Run a command as a job of the daemon.
    ///
    /// Waits while the daemon will not start it yet: until no pressure gate holds, no other
    /// never-seen job runs, and its estimate fits beside what is committed. Waiting jobs are
    /// considered by class (raised one class per 15 minutes waited), then the submitting
    /// worktree's share of the machine, then time waited. A later job that fits may start
    /// before an earlier one, unless the earlier one has waited over 5 minutes. A waiting run prints
    /// `taskslash: job <n> waiting <n>s (<place> in line): <reason>` on stderr when queued and every
    /// 15 s; the first line of a key that never ran adds the CPU its kind is assumed to take.
    ///
    /// Exits with the command's exit code (128+N when signal N killed it, 127 when it cannot be
    /// started). Exits 1 when no daemon is running, which a command's own exit 1 can also mean.
    Run {
        /// A name for the job: its key, which its estimate is learned under. Give one for anything
        /// run more than once. Without it the key is the command's first two words, or for
        /// `sh -c '<script>'` (or bash, dash, zsh, ksh) the script's. In either, a path word
        /// (`/`, `./`, `../`, `~/`) is cut to its last part, and a key over 60 characters keeps
        /// its start and end around a short hash of it.
        #[arg(long)]
        name: Option<String>,
        /// The job's cost class, whose defaults it gets until it has history: test, typecheck,
        /// build, browser or command.
        #[arg(long, default_value_t = Kind::Command)]
        kind: Kind,
        /// The job's priority class, which orders the queue and sets its CPU weight: urgent,
        /// flake, interactive, normal or background. Default: `TASKSLASH_CLASS`, else normal.
        #[arg(long)]
        class: Option<Class>,
        /// The CPU it takes, in threads, in place of the learned estimate.
        #[arg(long, value_parser = positive)]
        cpu: Option<f64>,
        /// The memory it takes, in MiB, in place of the learned estimate. With `--cpu`, the job is
        /// never held as never seen.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        mem: Option<u64>,
        /// The command and its arguments.
        #[arg(required = true, trailing_var_arg = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Run moon targets, each as its own job, in dependency order.
    ///
    /// Replaces `moon run --concurrency N <targets>`: the daemon decides how many run at once.
    /// Each target and every target it depends on (requested or not) runs exactly once, after the
    /// ones it depends on have passed, as `moon run <target> --upstream none`, named
    /// `<repo>/<target>`. A target whose dependency failed is skipped. Ends with one line per
    /// target: pass, fail or skipped, its wall time, `cached` when moon replayed it from cache, and
    /// its key in brackets; a failed target is followed by `  log: taskslash logs --last <key>`.
    /// Exits 0 when every target passed, else 1; 2 when there is no moon workspace at or above the
    /// current directory or a target is wrong.
    Moon {
        /// `:<task>` (that task in every project that has it) or `<project>:<task>`.
        #[arg(required = true, value_name = "TARGET")]
        targets: Vec<String>,
        /// Pass `--force` to every `moon run`, so none is a cache hit.
        #[arg(long)]
        force: bool,
        /// The jobs' priority class, as for `run`. Default: `TASKSLASH_CLASS`, else normal.
        #[arg(long)]
        class: Option<Class>,
    },
    /// Show the cost estimate of a key and its recorded runs, newest first; with no key, list the
    /// keys.
    ///
    /// With no key, one line per key that has runs, the most recently run first: when it last ran,
    /// how many runs, the last exit code, the worktree it last ran in, and the key. With a key, the
    /// estimate, then its newest runs (20 unless `--limit`), each with the worktree it ran in.
    /// A key with no runs exits 1, naming up to five known keys like it. Inside a moon workspace, a
    /// key with no runs that looks like `<project>:<task>` is tried as `<repo>/<project>:<task>`, and
    /// when that has runs, `key: <repo>/<project>:<task>` is printed first on stderr.
    ///
    /// Each figure of the estimate comes from the key's history, or from its kind's default when
    /// no run feeds it (the kind is the newest run's). CPU and duration come from calm runs only:
    /// a calm run is one that ran while the machine was not under pressure (measured, with the
    /// machine's CPU `some avg10` at most 40 during the run). A cost the daemon could not measure
    /// (no systemd user session) shows as `-`, and as null with `--json`.
    History {
        /// The key: a --name, the first two words of a command ("bun test"), or for taskslash
        /// moon, <repo>/<project>:<task> ("shop/web:test").
        key: Option<String>,
        /// Print `{"estimate": ..., "runs": [...]}`, or with no key an array of the keys.
        #[arg(long)]
        json: bool,
        /// Show at most this many runs.
        #[arg(long, value_name = "N", default_value_t = HISTORY_LIMIT as u64,
              value_parser = clap::value_parser!(u64).range(1..))]
        limit: u64,
        /// Only the runs submitted from this worktree: its directory name, or its full path.
        #[arg(long = "where", value_name = "DIR")]
        within: Option<String>,
    },
    /// Show what the machine and the running jobs are doing, from the daemon's latest readings.
    ///
    /// The machine is sampled once a second (free space on the watched mounts every 30 s). A job
    /// with no live reading yet, or one run unmeasured, shows `-`, and null with `--json`.
    Status {
        /// Print the answer as one line of JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a job's or a run's log: everything its command wrote, stdout and stderr together.
    ///
    /// `taskslash run` and `taskslash moon` copy each job's output into a log of its own beside the
    /// database (`TASKSLASH_LOGS` overrides the directory). The log goes to stdout unchanged, after
    /// one line on stderr naming the run or job, its key, worktree, exit code and start time.
    /// Exits 1 when there is no such job or run, when the run was recorded before logs were kept,
    /// or when its log has expired (the daemon deletes logs of jobs that ended over
    /// `TASKSLASH_LOG_DAYS` ago, 7 by default, and the oldest ones past `TASKSLASH_LOG_MIB`, 1024).
    #[command(group(ArgGroup::new("pick").required(true)))]
    Logs {
        /// A job's number, as `status` shows it: running, or ended since the daemon started.
        #[arg(group = "pick")]
        job: Option<u64>,
        /// A finished run, by the id `history` shows.
        #[arg(long, value_name = "ID", group = "pick")]
        run: Option<i64>,
        /// The newest run of a key, running or not.
        #[arg(long, value_name = "KEY", group = "pick")]
        last: Option<String>,
        /// The newest run (of a key, when given) that exited with a code other than 0, submitted
        /// from this worktree (the nearest directory above here with a `.git`).
        #[arg(long, value_name = "KEY", num_args = 0..=1, group = "pick")]
        failed: Option<Option<String>>,
        /// With `--failed`: search the runs of the whole machine, not only this worktree's.
        #[arg(long)]
        all: bool,
        /// Follow a running job's log until the job ends.
        #[arg(short, long)]
        follow: bool,
    },
    /// Say why a job waits, `running`, or what became of it; with no job, why each job waits.
    ///
    /// With no job, one line per waiting job (`job <n> <worktree> <key>: <reason>`), or `nothing is
    /// waiting`. A job that ended since the daemon started is named with its run and exit code.
    /// Exits 1 for a job the daemon does not know (it forgets ended jobs when it restarts and keeps
    /// the newest 1000; `taskslash history` lists the keys, and each key's runs).
    Why {
        /// The job's number, as `status` and a waiting `run` show it.
        job: Option<u64>,
        /// Print the reason object (`{"kind":"running"}` for a running job, `{"kind":"ended",...}`
        /// for an ended one); with no job, an array of `{"job","key","submitter","reason"}`.
        #[arg(long)]
        json: bool,
    },
    /// Cancel a waiting or running job, from any terminal.
    ///
    /// A waiting job leaves the queue: its `taskslash run` prints `taskslash: job <n> cancelled
    /// while waiting` and exits 1 without running its command. A running job is stopped by its own
    /// `taskslash run`, which sends the command's process group SIGTERM, then SIGKILL after
    /// `--grace` seconds if it has not ended; the run is recorded with the code it ends with (143
    /// after SIGTERM, 137 after SIGKILL). Exits 0 once the job is cancelled, or when it already
    /// was; 1 when it is neither waiting nor running.
    ///
    /// Only the user who runs the daemon can cancel: its socket sits in a directory only that user
    /// can open (mode 0700, in `$XDG_RUNTIME_DIR`), and that is the only check. A
    /// `TASKSLASH_SOCKET` elsewhere is as private as its directory.
    Cancel {
        /// The job's number, as `status`, `why` and a waiting `run` show it.
        job: u64,
        /// Seconds between SIGTERM and SIGKILL; 0 sends SIGKILL straight away.
        #[arg(long, value_name = "SECONDS", default_value_t = 10)]
        grace: u64,
    },
    /// Follow the daemon's job events: one line of JSON on stdout per event, as it happens.
    ///
    /// Each line is `{"type":"event","protocol":1,"seq":<n>,"event":{...}}`, `seq` counting up per
    /// daemon from 1, and `event.type` one of these (with its fields):
    ///
    /// - queued: job, key, kind, class, submitter, cwd, argv, at
    ///
    /// - started: job, unit, launch, log (the job's log file), at
    ///
    /// - finished: job, key, exit_code, wall_ms, cached, moon_hash, run (the history id), log, at
    ///
    /// - dropped: count
    ///
    /// Every queued job gets one finished: exit_code is null when its client went away, run when
    /// its row was not written, and a job that never started has exit_code, run and log null and
    /// wall_ms 0. Times are RFC 3339, UTC. Events carry no output: read the log. Only events from
    /// after the subscription are sent. A slow reader never slows a job: the daemon queues at most
    /// 1024 events for it, drops the rest, and sends one dropped (with how many, and the seq of the
    /// last one missed) before the next event it does take.
    ///
    /// A listener plugin runs this, or opens `TASKSLASH_SOCKET` and sends `{"type":"subscribe"}`
    /// itself (the reply is `{"type":"subscribed","protocol":1}`, then the event lines). Exits 0
    /// when the daemon hangs up, 1 when no daemon is running.
    Events,
    /// List the plugins on PATH: every executable named `taskslash-<name>`.
    ///
    /// `taskslash <name> [args...]` runs the first `taskslash-<name>` on PATH in place of taskslash,
    /// when `<name>` is not a built-in command. One line per name with the path that runs; later
    /// ones of the same name are listed as shadowed, and a name a built-in has is marked as never
    /// run.
    Plugins {
        /// Print one array of `{"name", "path", "shadowed", "builtin"}`.
        #[arg(long)]
        json: bool,
    },
    /// Show the machine, the jobs and the selected job's output on one screen, live.
    ///
    /// A strip of the machine's CPU, memory, pressure and load on top; the running jobs, the
    /// waiting ones in queue order and the jobs that finished since top opened (newest first) on
    /// the left; the selected job's log, its last lines, on the right, followed as it grows (a
    /// waiting job shows why it waits). Up/down or j/k select; q, Esc or Ctrl-C quit.
    ///
    /// Exits 1 when no daemon is running, 2 when stdout is not a terminal.
    Top,
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// A finite number above 0.
fn positive(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| format!("{text} is not a number above 0"))
}

/// `--class`, else `TASKSLASH_CLASS` when set and not empty, else normal. A `TASKSLASH_CLASS` that
/// is not a class is a usage error, as a bad `--class` is.
fn class(flag: Option<Class>) -> Class {
    if let Some(class) = flag {
        return class;
    }
    let Some(text) = std::env::var_os("TASKSLASH_CLASS").filter(|v| !v.is_empty()) else {
        return Class::Normal;
    };
    text.to_string_lossy().parse().unwrap_or_else(|e: String| {
        Cli::command()
            .error(ErrorKind::InvalidValue, format!("TASKSLASH_CLASS: {e}"))
            .exit()
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("taskslash: {e}");
            ExitCode::from(if e.is_usage() { 2 } else { 1 })
        }
    }
}

/// The names of the built-in commands, `help` included.
fn builtins() -> Vec<String> {
    let mut command = Cli::command();
    command.build();
    command
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect()
}

/// Runs the plugin for `taskslash <name> [args...]`; returns only when it could not be started.
fn plugin(argv: Vec<OsString>) -> Error {
    let mut argv = argv.into_iter();
    let name = argv.next().unwrap_or_default();
    let args: Vec<OsString> = argv.collect();
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(program) = plugin::find(&name, &path) {
        return plugin::exec(&program, &args);
    }
    let name = name.to_string_lossy();
    let builtins = builtins();
    let tip = plugin::closest(&name, &builtins)
        .map(|c| format!("\n\n  tip: a similar subcommand exists: '{c}'"))
        .unwrap_or_default();
    let message = format!(
        "unrecognized subcommand '{name}'{tip}\n\nNo program named {}{name} is on PATH.",
        plugin::PREFIX
    );
    Cli::command()
        .error(ErrorKind::InvalidSubcommand, message)
        .exit()
}

/// `taskslash history`: the keys when `key` is `None`, else the key's estimate and runs. A key with
/// no runs (in `within`, when given) is an error naming keys like it.
fn history(
    socket: &std::path::Path,
    key: Option<&str>,
    json: bool,
    limit: usize,
    within: Option<&str>,
) -> Result<i32, Error> {
    let text = match key {
        None => {
            let keys = taskslash_core::keys(socket)?;
            if json {
                taskslash_core::protocol::keys_json(&keys)
            } else {
                keys_text(&keys)
            }
        }
        Some(typed) => {
            let (mut estimate, mut runs) = taskslash_core::history(socket, typed, limit, within)?;
            let resolved = runs
                .is_empty()
                .then(|| std::env::current_dir().ok())
                .flatten()
                .and_then(|cwd| taskslash_core::target_key(&cwd, typed));
            if let Some(resolved) = &resolved {
                (estimate, runs) = taskslash_core::history(socket, resolved, limit, within)?;
                if !runs.is_empty() {
                    eprintln!("key: {resolved}");
                }
            }
            if runs.is_empty() {
                return Err(no_runs(socket, typed, resolved.as_deref(), within)?);
            }
            if json {
                taskslash_core::protocol::history_json(&estimate, &runs)
            } else {
                format!("{}{}", estimate_text(&estimate), table(&runs))
            }
        }
    };
    write!(io::stdout(), "{text}")
        .map_err(|e| Error::new(format!("cannot write the runs: {e}")))?;
    Ok(0)
}

/// The error for a key with no runs (nor its moon form `resolved`, when it has one): in `within`
/// when the key has runs elsewhere, else with up to five known keys like it.
fn no_runs(
    socket: &std::path::Path,
    key: &str,
    resolved: Option<&str>,
    within: Option<&str>,
) -> Result<Error, Error> {
    let keys: Vec<String> = taskslash_core::keys(socket)?
        .into_iter()
        .map(|k| k.key)
        .collect();
    if let Some(within) = within
        && let Some(known) = [Some(key), resolved]
            .into_iter()
            .flatten()
            .find(|&candidate| keys.iter().any(|k| k == candidate))
    {
        return Ok(Error::new(format!("no runs of \"{known}\" in {within}")));
    }
    let like = taskslash_core::protocol::suggest(key, &keys);
    let mut message = format!("no runs of \"{key}\"");
    if let Some(resolved) = resolved {
        message.push_str(&format!(" nor of \"{resolved}\""));
    }
    if like.is_empty() {
        message.push_str("\n  'taskslash history' with no key lists the keys");
    } else {
        message.push_str("\n  keys like it:");
        for k in like {
            message.push_str(&format!("\n    {k}"));
        }
    }
    Ok(Error::new(message))
}

/// What `taskslash why <job>` prints for job `job`, found `at`.
fn why_text(at: Whereabouts, job: u64, json: bool) -> String {
    match (at, json) {
        (Whereabouts::Waiting(reason, _, _), true) => {
            taskslash_core::protocol::reason_json(&reason)
        }
        (Whereabouts::Waiting(reason, waited_s, position), false) => {
            let line = waiting_line(&Waiting {
                job: Some(job),
                reason: &reason,
                waited_s,
                position,
                never_run: None,
            });
            format!("{}\n", line.trim_start_matches("taskslash: "))
        }
        (Whereabouts::Running, true) => "{\"kind\":\"running\"}\n".to_owned(),
        (Whereabouts::Running, false) => "running\n".to_owned(),
        (Whereabouts::Ended(ended), true) => taskslash_core::protocol::ended_json(&ended),
        (Whereabouts::Ended(ended), false) => format!("{}\n", ended.sentence(job)),
    }
}

fn dispatch(command: Commands) -> Result<i32, Error> {
    let socket = taskslash_core::socket_path;
    match command {
        Commands::External(argv) => Err(plugin(argv)),
        Commands::Plugins { json } => {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let plugins = plugin::list(&path, &builtins());
            let text = if json {
                plugin::list_json(&plugins)
            } else {
                plugin::list_text(&plugins)
            };
            write!(io::stdout(), "{text}")
                .map_err(|e| Error::new(format!("cannot write the plugins: {e}")))?;
            Ok(0)
        }
        Commands::Top => top::run(&socket()?),
        Commands::Events => {
            taskslash_core::events(&socket()?, &mut io::stdout().lock()).map(|()| 0)
        }
        Commands::Daemon => {
            taskslash_core::serve(&socket()?, &taskslash_core::db_path()?).map(|()| 0)
        }
        Commands::Run {
            name,
            kind,
            class: flag,
            cpu,
            mem,
            command,
        } => {
            let demand = taskslash_core::Demand { cpu, mem_mib: mem };
            taskslash_core::run(&socket()?, name, kind, class(flag), demand, &command)
        }
        Commands::Moon {
            targets,
            force,
            class: flag,
        } => {
            let cwd = std::env::current_dir()
                .map_err(|e| Error::new(format!("cannot read the current directory: {e}")))?;
            taskslash_core::moon(&socket()?, &cwd, &targets, force, class(flag))
        }
        Commands::Logs {
            job,
            run,
            last,
            failed,
            all,
            follow,
        } => {
            if all && failed.is_none() {
                return Err(Error::usage("--all goes with --failed"));
            }
            let select = match (job, run, last, failed) {
                (Some(job), ..) => LogSelect::Job { job },
                (_, Some(run), ..) => LogSelect::Run { run },
                (_, _, Some(key), _) => LogSelect::Last { key },
                (_, _, _, Some(key)) => {
                    let within = if all {
                        None
                    } else {
                        let cwd = std::env::current_dir().map_err(|e| {
                            Error::new(format!("cannot read the current directory: {e}"))
                        })?;
                        Some(
                            taskslash_core::submitter(&cwd)
                                .to_string_lossy()
                                .into_owned(),
                        )
                    };
                    LogSelect::Failed { key, within }
                }
                (None, None, None, None) => return Err(Error::usage("name a job or a run")),
            };
            taskslash_core::logs(
                &socket()?,
                &select,
                follow,
                &mut io::stdout().lock(),
                &mut io::stderr().lock(),
            )
            .map(|()| 0)
        }
        Commands::Cancel { job, grace } => {
            let cancelled = taskslash_core::cancel(&socket()?, job, grace)?;
            match cancel_text(&cancelled, job, grace) {
                Ok(text) => {
                    write!(io::stdout(), "{text}")
                        .map_err(|e| Error::new(format!("cannot write the answer: {e}")))?;
                    Ok(0)
                }
                Err(message) => Err(Error::new(message)),
            }
        }
        Commands::Why { job, json } => {
            let socket = socket()?;
            let text = match job {
                None => {
                    let status = taskslash_core::status(&socket)?;
                    if json {
                        taskslash_core::protocol::waiting_json(&status.waiting)
                    } else {
                        waiting_text(&status.waiting)
                    }
                }
                Some(job) => why_text(taskslash_core::why(&socket, job)?, job, json),
            };
            write!(io::stdout(), "{text}")
                .map_err(|e| Error::new(format!("cannot write the reason: {e}")))?;
            Ok(0)
        }
        Commands::History {
            key,
            json,
            limit,
            within,
        } => {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            history(&socket()?, key.as_deref(), json, limit, within.as_deref())
        }
        Commands::Status { json } => {
            let status = taskslash_core::status(&socket()?)?;
            let text = if json {
                taskslash_core::protocol::status_json(status)
            } else {
                status_text(&status, std::time::SystemTime::now())
            };
            write!(io::stdout(), "{text}")
                .map_err(|e| Error::new(format!("cannot write the status: {e}")))?;
            Ok(0)
        }
    }
}
