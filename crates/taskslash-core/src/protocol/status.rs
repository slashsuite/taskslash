//! The answer to `status`: the machine, the running jobs and the waiting ones.

use super::{Class, DaemonMessage, Kind};
use crate::admission::Reason;
use crate::estimate::Estimate;
use crate::sampler::MachineSnapshot;
use serde::{Deserialize, Serialize};

/// What the machine and the running jobs are doing, as `taskslash status --json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon that answered. `None` from a daemon older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonInfo>,
    pub machine: MachineSnapshot,
    /// The running jobs, oldest first.
    pub jobs: Vec<JobStatus>,
    /// The waiting jobs, in the order they will be considered.
    pub waiting: Vec<WaitingStatus>,
}

/// The daemon's build and since when it has run, so a person can tell whether it is the one just
/// installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// `0.1.0 (ccdb25a)`, as `taskslash --version` prints it after the name.
    pub version: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub pid: u32,
}

/// A running job and the last live reading of it. The reading is `None` before its first one, and
/// always on the plain launch path (nothing measures it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub job: u64,
    pub key: String,
    pub kind: Kind,
    pub class: Class,
    pub submitter: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub cpu_threads: Option<f64>,
    pub mem_mib: Option<u64>,
    pub estimate: Estimate,
    /// The file its output is copied into.
    pub log_path: String,
}

/// A queued job: how long it has waited, its 1-based place in the queue, and why: `reason` as the
/// sentence `why` prints, `why` as the object `why --json` prints (`None` before its first pass).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingStatus {
    pub job: u64,
    pub key: String,
    pub kind: Kind,
    pub class: Class,
    /// The class it is ordered as, when waiting has raised it above `class`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_class: Option<Class>,
    pub submitter: String,
    pub waited_s: f64,
    pub position: usize,
    pub reason: String,
    pub why: Option<Reason>,
    pub estimate: Estimate,
}

/// `status` as the one line of JSON `taskslash status --json` prints, with its newline.
pub fn status_json(status: Status) -> String {
    let mut text =
        serde_json::to_string(&DaemonMessage::Status(Box::new(status))).unwrap_or_default();
    text.push('\n');
    text
}
