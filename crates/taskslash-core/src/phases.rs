//! Where a `taskslash run` spends its time, for the start benchmark (`floor-bench`). With
//! `TASKSLASH_PHASES` set and not empty, `run` writes one line on stderr when the job ends:
//!
//! ```text
//! taskslash: phases connect=0.081 admit=0.512 launch=0.233 command=24.704 drain=0.012 report=1.903 ms
//! ```
//!
//! Each figure is the milliseconds since the mark before it: `connect` opens the daemon's socket,
//! `admit` is from `submit` to the daemon's `start`, `launch` from `start` to the command
//! spawned (the log opened, the executable checked), `command` from the spawn to the command's
//! exit (with `systemd-run` setting up its scope on that path), `drain` from the exit to the
//! last of its output copied, and `report` from `exited` sent to `recorded` read. A phase the run
//! never reached is left out.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::Instant;

/// The marks of one run, in the order they were made.
pub(crate) struct Phases {
    last: RefCell<Instant>,
    marks: RefCell<Vec<(&'static str, f64)>>,
}

impl Phases {
    /// A clock started now when `TASKSLASH_PHASES` is set and not empty, else `None`.
    pub(crate) fn from_env() -> Option<Self> {
        std::env::var_os("TASKSLASH_PHASES")
            .filter(|v| !v.is_empty())
            .map(|_| Self::new())
    }

    pub(crate) fn new() -> Self {
        Self {
            last: RefCell::new(Instant::now()),
            marks: RefCell::new(Vec::new()),
        }
    }

    /// Ends phase `name` now: it took the time since the mark before it.
    pub(crate) fn mark(&self, name: &'static str) {
        let now = Instant::now();
        let took = now.duration_since(self.last.replace(now));
        self.marks
            .borrow_mut()
            .push((name, took.as_secs_f64() * 1000.0));
    }

    /// The stderr line, without its newline.
    pub(crate) fn line(&self) -> String {
        let mut line = String::from("taskslash: phases");
        for (name, ms) in self.marks.borrow().iter() {
            let _ = write!(line, " {name}={ms:.3}");
        }
        line.push_str(" ms");
        line
    }
}

/// Marks `name` on `phases`, when there is a clock.
pub(crate) fn mark(phases: Option<&Phases>, name: &'static str) {
    if let Some(phases) = phases {
        phases.mark(name);
    }
}

#[cfg(test)]
mod tests {
    use super::Phases;

    #[test]
    fn the_line_names_each_phase_in_order_in_milliseconds() {
        let phases = Phases::new();
        phases.mark("connect");
        phases.mark("admit");
        let line = phases.line();
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words[..2], ["taskslash:", "phases"]);
        assert!(words[2].starts_with("connect="), "{line}");
        assert!(words[3].starts_with("admit="), "{line}");
        assert_eq!(words[4], "ms");
        let ms: f64 = words[2].trim_start_matches("connect=").parse().unwrap();
        assert!((0.0..1000.0).contains(&ms), "{line}");
    }
}
