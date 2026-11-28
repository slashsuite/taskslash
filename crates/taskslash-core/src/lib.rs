//! The TaskSlash engine. The binary in `crates/taskslash` is its command line.
//!
//! The caller runs the command and the daemon admits and records: `taskslash run` asks the daemon
//! over a unix socket for a job, runs the command as its own child (in a transient systemd scope
//! when the machine has a user session), and reports the exit code. The connection stays open for
//! the job's whole life and is its lease. The daemon measures the scope while it runs and writes a
//! row of what the job cost when it ends. A thread samples the whole machine once a second for
//! `taskslash status` and admission.
//!
//! A job that may not start yet waits in the daemon's queue. `admit` runs `order::pass`, which
//! asks `admission::check` for each waiting job in priority order (class, then the submitter's
//! share of the machine, then time waited), passing over a job that does not fit unless it has
//! waited over 5 minutes: a job starts when no pressure gate holds, it is not a second
//! never-seen job, and its estimate (its past runs, else its kind's defaults, else what the caller
//! gave) fits under the budget beside what is committed. A waiting job is told why.

pub mod admission;
mod answers;
mod capture;
mod client;
mod daemon;
pub mod estimate;
mod launch;
mod logs;
mod measure;
mod moon;
mod moon_run;
pub mod order;
mod phases;
pub mod plugin;
pub mod procfs;
pub mod protocol;
pub mod sampler;
mod scheduler;
mod socket;
mod store;

pub use client::{
    Cancelled, Demand, Subscription, Whereabouts, cancel, events, history, keys, logs, recent, run,
    status, submitter, subscribe, why,
};
pub use daemon::serve;
pub use moon::target_key;
pub use moon_run::moon;
pub use socket::socket_path;
pub use store::{HISTORY_LIMIT, db_path};

/// This build: the version and the commit it was built from, `0.1.0 (ccdb25a)`, or the version
/// alone when it was built outside a git checkout.
pub const VERSION: &str = env!("TASKSLASH_VERSION");

use std::fmt;

/// A failure the command line prints as `taskslash: <message>` and exits 1 on, or 2 on a usage
/// error.
#[derive(Debug)]
pub struct Error {
    message: String,
    usage: bool,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            usage: false,
        }
    }

    /// A usage error: the arguments, or where the command was run, are wrong.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            usage: true,
        }
    }

    pub fn is_usage(&self) -> bool {
        self.usage
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
