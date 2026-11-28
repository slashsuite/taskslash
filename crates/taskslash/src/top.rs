//! `taskslash top`: one screen with the machine, the jobs and the selected job's output.
//!
//! [`App`] is the whole state and takes what the daemon and the log files say ([`App::status`],
//! [`App::event`], [`App::log_reset`], [`App::log_append`]); [`render`] draws it. [`run`] is the
//! thin shell: it polls `status` once a second and follows the events on threads of their own,
//! reads the selected job's log every 250 ms, and passes keys on. The finished list starts with the
//! newest recorded runs, so it is not empty before a job ends.

use crate::text::duration;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use std::collections::{HashMap, VecDeque};
use taskslash_core::admission::ordinal;
use taskslash_core::protocol::{Class, JobEvent, JobStatus, Run, Status, WaitingStatus, worktree};

mod shell;
pub use shell::run;

#[cfg(test)]
mod tests;

/// How many finished jobs the list keeps, and how many recorded runs it starts with.
const RECENT: usize = 50;
/// How many lines of a log the pane keeps.
const KEEP_LINES: usize = 2000;
/// The most bytes of an unfinished last line the pane keeps.
const PARTIAL: usize = 16 * 1024;

/// What `top` knows about a job before it finishes, for the row of a finished one.
#[derive(Debug, Clone, PartialEq)]
struct Known {
    key: String,
    class: Class,
    submitter: String,
}

/// What a row is known by: a job by the daemon's number, a run read from the database by its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    Job(u64),
    Run(i64),
}

/// A job that finished since `top` opened (known by its job number), or a run recorded before
/// (known by its run id).
#[derive(Debug, Clone, PartialEq)]
pub struct Recent {
    pub id: Id,
    /// The recorded run; `None` when its row was not written or the job never started.
    pub run: Option<i64>,
    pub key: String,
    /// `None` only when `top` never saw the job queued or in a status poll.
    pub class: Option<Class>,
    pub submitter: Option<String>,
    pub exit_code: Option<i32>,
    pub wall_ms: u64,
    pub cached: Option<bool>,
    /// `None` when the job never started, or the run was recorded before logs were kept.
    pub log: Option<String>,
}

impl Recent {
    /// A run from the database, for the list `top` starts with.
    pub fn recorded(run: Run) -> Self {
        Self {
            id: Id::Run(run.id),
            run: Some(run.id),
            key: run.key,
            class: Some(run.class),
            submitter: Some(run.submitter),
            exit_code: run.exit_code,
            wall_ms: u64::try_from(run.wall_ms).unwrap_or(0),
            cached: run.cached,
            log: run.log_path,
        }
    }

    /// Whether the job left the queue before it started (so it has no exit code and no log).
    fn never_started(&self) -> bool {
        matches!(self.id, Id::Job(_)) && self.log.is_none()
    }
}

/// One row of the job list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Row<'a> {
    Running(&'a JobStatus),
    Waiting(&'a WaitingStatus),
    Recent(&'a Recent),
}

impl Row<'_> {
    pub fn id(&self) -> Id {
        match self {
            Row::Running(j) => Id::Job(j.job),
            Row::Waiting(w) => Id::Job(w.job),
            Row::Recent(r) => r.id,
        }
    }
}

/// What the output pane shows for the selected job.
#[derive(Debug, Clone, PartialEq)]
pub enum Pane<'a> {
    Nothing,
    /// A log file to follow.
    Log(&'a str),
    Waiting(&'a str),
    NeverStarted,
    /// A run recorded before logs were kept.
    NoLog,
}

/// The selected job's log as read so far: complete lines, cleaned, and the raw bytes after the
/// last newline (cleaned when drawn, so an escape sequence split across two reads is whole).
#[derive(Debug, Default, Clone, PartialEq)]
struct Output {
    path: Option<String>,
    lines: VecDeque<String>,
    partial: Vec<u8>,
    error: Option<String>,
}

/// Everything `top` shows.
#[derive(Debug, Default)]
pub struct App {
    status: Option<Status>,
    /// The status poll's last failure, cleared by the next answer.
    status_error: Option<String>,
    known: HashMap<u64, Known>,
    /// Newest first.
    recent: VecDeque<Recent>,
    selected: Option<Id>,
    /// The selected row's index, to fall back on when its job leaves the list.
    selected_index: usize,
    notice: Option<String>,
    output: Output,
    /// Whether the key help is shown over the screen.
    help: bool,
    /// The job a first `c` asked to cancel; a second `c` on it cancels it.
    confirm_cancel: Option<u64>,
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    /// A status poll's answer.
    pub fn status(&mut self, status: Status) {
        for j in &status.jobs {
            self.remember(j.job, &j.key, j.class, &j.submitter);
        }
        for w in &status.waiting {
            self.remember(w.job, &w.key, w.class, &w.submitter);
        }
        self.status = Some(status);
        self.status_error = None;
        self.keep_selection();
    }

    /// The runs that ended last, the last first, to start the finished list with; called once,
    /// before any event, with at most `RECENT` runs (the caller asks for that many).
    pub fn seed(&mut self, runs: Vec<Run>) {
        self.recent.extend(runs.into_iter().map(Recent::recorded));
        self.keep_selection();
    }

    /// A status poll that failed; the last answer stays on screen.
    pub fn status_failed(&mut self, message: String) {
        self.status_error = Some(message);
    }

    /// A line for the bottom of the screen, such as the event stream ending.
    pub fn notice(&mut self, message: String) {
        self.notice = Some(message);
    }

    pub fn event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Queued {
                job,
                key,
                class,
                submitter,
                ..
            } => self.remember(job, &key, class, &submitter),
            JobEvent::Started { .. } => {}
            JobEvent::Finished {
                job,
                key,
                exit_code,
                wall_ms,
                cached,
                run,
                log,
                ..
            } => {
                let known = self.known.remove(&job);
                // The run may be listed already, read from the database as top opened.
                if run.is_some() {
                    self.recent.retain(|r| r.run != run);
                }
                self.recent.push_front(Recent {
                    id: Id::Job(job),
                    run,
                    key,
                    class: known.as_ref().map(|k| k.class),
                    submitter: known.map(|k| k.submitter),
                    exit_code,
                    wall_ms,
                    cached,
                    log,
                });
                self.recent.truncate(RECENT);
                self.keep_selection();
            }
            JobEvent::Dropped { count } => {
                self.notice = Some(format!(
                    "missed {count} job events (top read them too slowly); the recent list may lack them"
                ));
            }
        }
    }

    fn remember(&mut self, job: u64, key: &str, class: Class, submitter: &str) {
        self.known.insert(
            job,
            Known {
                key: key.to_owned(),
                class,
                submitter: submitter.to_owned(),
            },
        );
    }

    /// Running jobs, then waiting ones in queue order, then finished ones, newest first. A job
    /// that has finished is listed once, as finished, even while the last poll still has it.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let finished = |job: u64| self.recent.iter().any(|r| r.id == Id::Job(job));
        let mut rows = Vec::new();
        if let Some(status) = &self.status {
            rows.extend(
                status
                    .jobs
                    .iter()
                    .filter(|j| !finished(j.job))
                    .map(Row::Running),
            );
            let mut waiting: Vec<&WaitingStatus> =
                status.waiting.iter().filter(|w| !finished(w.job)).collect();
            waiting.sort_by_key(|w| w.position);
            rows.extend(waiting.into_iter().map(Row::Waiting));
        }
        rows.extend(self.recent.iter().map(Row::Recent));
        rows
    }

    /// The selected row's job number.
    #[cfg(test)]
    pub fn selected(&self) -> Option<u64> {
        match self.selected? {
            Id::Job(job) => Some(job),
            Id::Run(_) => None,
        }
    }

    fn selected_row(&self) -> Option<Row<'_>> {
        let id = self.selected?;
        self.rows().into_iter().find(|r| r.id() == id)
    }

    /// The selected job, when it runs or waits and so can be cancelled.
    pub fn cancellable(&self) -> Option<u64> {
        match self.selected_row()? {
            Row::Running(j) => Some(j.job),
            Row::Waiting(w) => Some(w.job),
            Row::Recent(_) => None,
        }
    }

    /// The `c` key. The first press on a running or waiting job asks for a second; the second
    /// gives the job to cancel. Any other key in between ([`App::other_key`]) keeps the job.
    pub fn cancel_key(&mut self) -> Option<u64> {
        let Some(job) = self.cancellable() else {
            self.confirm_cancel = None;
            self.notice = Some("only a running or waiting job can be cancelled".to_owned());
            return None;
        };
        if self.confirm_cancel.take() == Some(job) {
            self.notice = Some(format!("cancelling job {job}"));
            return Some(job);
        }
        self.confirm_cancel = Some(job);
        self.notice = Some(format!(
            "press c again to cancel job {job}, any other key keeps it"
        ));
        None
    }

    /// A key other than `c`: a cancel asked for and not confirmed is dropped, with its notice.
    pub fn other_key(&mut self) {
        if self.confirm_cancel.take().is_some() {
            self.notice = None;
        }
    }

    /// The `?` key: shows the key help, or hides it.
    pub fn toggle_help(&mut self) {
        self.help = !self.help;
    }

    pub fn help_shown(&self) -> bool {
        self.help
    }

    /// Keeps the selection on the same row; when that row left the list, on the row now at its
    /// place.
    fn keep_selection(&mut self) {
        let jobs: Vec<Id> = self.rows().iter().map(Row::id).collect();
        match self
            .selected
            .and_then(|s| jobs.iter().position(|&j| j == s))
        {
            Some(index) => self.selected_index = index,
            None => {
                self.selected_index = self.selected_index.min(jobs.len().saturating_sub(1));
                self.selected = jobs.get(self.selected_index).copied();
            }
        }
    }

    /// Moves the selection `by` rows, staying inside the list.
    pub fn move_selection(&mut self, by: isize) {
        let jobs: Vec<Id> = self.rows().iter().map(Row::id).collect();
        if jobs.is_empty() {
            return;
        }
        let index = self
            .selected_index
            .saturating_add_signed(by)
            .min(jobs.len() - 1);
        self.selected_index = index;
        self.selected = Some(jobs[index]);
    }

    /// What the output pane shows.
    pub fn pane(&self) -> Pane<'_> {
        match self.selected_row() {
            None => Pane::Nothing,
            Some(Row::Running(j)) => Pane::Log(&j.log_path),
            Some(Row::Waiting(w)) => Pane::Waiting(&w.reason),
            Some(Row::Recent(r)) => match &r.log {
                Some(log) => Pane::Log(log),
                None if r.never_started() => Pane::NeverStarted,
                None => Pane::NoLog,
            },
        }
    }

    /// The log the pane follows, when it shows one.
    pub fn wanted_log(&self) -> Option<&str> {
        match self.pane() {
            Pane::Log(path) => Some(path),
            _ => None,
        }
    }

    /// The tail of the log at `path`, read anew (a new selection, or the file was replaced).
    pub fn log_reset(&mut self, path: &str, bytes: &[u8]) {
        self.output = Output {
            path: Some(path.to_owned()),
            ..Output::default()
        };
        self.log_append(bytes);
    }

    /// Bytes appended to the log since the last read.
    pub fn log_append(&mut self, bytes: &[u8]) {
        let out = &mut self.output;
        out.error = None;
        out.partial.extend_from_slice(bytes);
        if let Some(end) = out.partial.iter().rposition(|&b| b == b'\n') {
            let rest = out.partial.split_off(end + 1);
            let done = std::mem::replace(&mut out.partial, rest);
            for line in done[..end].split(|&b| b == b'\n') {
                out.lines.push_back(clean(line));
            }
            while out.lines.len() > KEEP_LINES {
                out.lines.pop_front();
            }
        }
        // A progress line rewritten with carriage returns and no newline: only what follows the
        // last carriage return is shown (a final one may still be half of a CRLF), and at most
        // PARTIAL bytes of it are kept.
        let len = out.partial.len();
        if let Some(cr) = out.partial[..len.saturating_sub(1)]
            .iter()
            .rposition(|&b| b == b'\r')
        {
            out.partial.drain(..=cr);
        }
        if out.partial.len() > PARTIAL {
            let cut = out.partial.len() - PARTIAL;
            out.partial.drain(..cut);
        }
    }

    /// The log at `path` cannot be read.
    pub fn log_error(&mut self, path: &str, message: String) {
        self.output = Output {
            path: Some(path.to_owned()),
            error: Some(message),
            ..Output::default()
        };
    }

    /// The last `n` log lines read so far, cleaned, the unfinished last line included.
    pub fn log_lines(&self, n: usize) -> Vec<String> {
        let partial = clean(&self.output.partial);
        let partial = (!partial.is_empty()).then_some(partial);
        let whole = n.saturating_sub(usize::from(partial.is_some()));
        let from = self.output.lines.len().saturating_sub(whole);
        let mut lines: Vec<String> = self.output.lines.range(from..).cloned().collect();
        lines.extend(partial);
        lines
    }

    /// The event stream was lost and is followed again: the daemon went away, and one that
    /// started again numbers its jobs from 1. So a finished job is kept by its run id, one with no
    /// run is dropped, what was remembered for the old numbers is dropped, and so is the notice
    /// that said the stream was lost.
    pub fn resubscribed(&mut self) {
        self.recent.retain_mut(|r| match r.run {
            Some(run) => {
                r.id = Id::Run(run);
                true
            }
            None => false,
        });
        self.known.clear();
        self.notice = None;
        self.keep_selection();
    }
}

/// A line of a job's output as the screen may show it: escape sequences (CSI, OSC and the two-byte
/// ones) and control characters removed, a tab as spaces, and only what follows the last carriage
/// return (what a terminal would show of a progress line).
pub fn clean(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let raw = match raw.iter().rposition(|&b| b == b'\r') {
        Some(cr) => &raw[cr + 1..],
        None => raw,
    };
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates up to a final byte in @ to ~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: up to BEL or ESC \.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // A charset designation takes one more character.
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                }
                _ => {}
            },
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// How a finished job ended. A job that never started has no exit code (the daemon sends null).
fn exit_text(r: &Recent) -> String {
    match r.exit_code {
        _ if r.never_started() => "never started".to_owned(),
        None => "no exit code".to_owned(),
        Some(code) => format!("exit {code}"),
    }
}

/// A row's number: the job's, or `r` and the id of a run read from the database.
fn id_text(id: Id) -> String {
    match id {
        Id::Job(job) => job.to_string(),
        Id::Run(run) => format!("r{run}"),
    }
}

/// The two lines of the machine strip.
pub fn strip(status: &Status) -> [String; 2] {
    let m = &status.machine;
    let gib = |mib: u64| mib as f64 / 1024.0;
    let used = m.memory.total_mib.saturating_sub(m.memory.available_mib);
    let available = if m.memory.total_mib == 0 {
        0.0
    } else {
        m.memory.available_mib as f64 / m.memory.total_mib as f64 * 100.0
    };
    [
        format!(
            "cpu {:.0}% busy on {} threads, pressure {:.2}   load {:.2} {:.2} {:.2}",
            m.cpu.busy * 100.0,
            m.cpu.threads,
            m.cpu.pressure.some_avg10,
            m.load[0],
            m.load[1],
            m.load[2]
        ),
        format!(
            "mem {:.1} of {:.1} GiB used, {available:.0}% available, pressure {:.2}   io some {:.2} full {:.2}",
            gib(used),
            gib(m.memory.total_mib),
            m.memory.pressure.some_avg10,
            m.io.pressure.some_avg10,
            m.io.pressure.full_avg10
        ),
    ]
}

/// One job list row: number, state, class, worktree and key (last, as the one most often cut).
pub fn row_text(row: Row<'_>) -> String {
    let job = id_text(row.id());
    let (state, key, submitter, class) = match row {
        Row::Running(j) => (
            "running".to_owned(),
            j.key.as_str(),
            j.submitter.as_str(),
            j.class.to_string(),
        ),
        Row::Waiting(w) => (
            format!("waiting {} {}", ordinal(w.position), duration(w.waited_s)),
            w.key.as_str(),
            w.submitter.as_str(),
            w.class.to_string(),
        ),
        Row::Recent(r) => {
            let mut state = exit_text(r);
            if !r.never_started() {
                state = format!("{state} {}", duration(r.wall_ms as f64 / 1000.0));
            }
            if r.cached == Some(true) {
                state.push_str(" cached");
            }
            (
                state,
                r.key.as_str(),
                r.submitter.as_deref().unwrap_or("-"),
                r.class.map_or("-".to_owned(), |c| c.to_string()),
            )
        }
    };
    format!(
        "{job:>5} {state:<18} {class:<11} {}  {key}",
        worktree(submitter)
    )
}

/// Draws `app` on the whole frame: the strip on top, the list on the left, the output on the
/// right, and one line of keys and notices at the bottom.
pub fn render(app: &App, frame: &mut Frame) {
    let [top, middle, bottom] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_strip(app, frame, top);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(middle);
    render_list(app, frame, left);
    render_pane(app, frame, right);

    let mut foot = "q quit   j/k select   enter page the log   c cancel   ? keys".to_owned();
    if let Some(e) = &app.status_error {
        foot.push_str(&format!("   status: {e}"));
    } else if let Some(notice) = &app.notice {
        foot.push_str(&format!("   {notice}"));
    }
    frame.render_widget(
        Paragraph::new(foot).style(Style::new().add_modifier(Modifier::DIM)),
        bottom,
    );
    if app.help {
        render_help(frame, middle);
    }
}

/// What each key does, as `?` shows it.
pub const KEYS: [(&str, &str); 5] = [
    ("up/down, j/k", "select a row"),
    ("enter", "open the selected log in $PAGER (less when unset)"),
    ("c, then c", "cancel the selected running or waiting job"),
    ("?", "show or hide these keys"),
    ("q, Esc, Ctrl-C", "quit"),
];

fn render_help(frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, what)| Line::from(format!("{key:<16} {what}")))
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys (? hides) ")),
        area,
    );
}

fn render_strip(app: &App, frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = match &app.status {
        Some(status) => strip(status).into_iter().map(Line::from).collect(),
        None => vec![Line::from("waiting for the daemon's first status")],
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let rows = app.rows();
    let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(row_text(*r))).collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(Block::bordered().title(" jobs "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(
        app.selected
            .and_then(|s| rows.iter().position(|r| r.id() == s)),
    );
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        let inner = Block::bordered().inner(area);
        frame.render_widget(
            Paragraph::new("no jobs running, waiting or recorded"),
            inner,
        );
    }
}

fn render_pane(app: &App, frame: &mut Frame, area: Rect) {
    let row = app.selected_row();
    let title = match row {
        None => " output ".to_owned(),
        Some(Row::Running(j)) => format!(" job {} {}: running ", j.job, j.key),
        Some(Row::Waiting(w)) => format!(" job {} {}: waiting ", w.job, w.key),
        Some(Row::Recent(r)) => match r.id {
            Id::Job(job) => format!(" job {job} {}: {} ", r.key, exit_text(r)),
            Id::Run(run) => format!(" run {run} {}: {} ", r.key, exit_text(r)),
        },
    };
    let block = Block::bordered().title(title);
    let height = block.inner(area).height as usize;
    let lines: Vec<Line> = match app.pane() {
        Pane::Nothing => vec![Line::from("no job selected")],
        Pane::Waiting(reason) => vec![Line::from(format!("waiting: {reason}"))],
        Pane::NeverStarted => vec![Line::from(
            "never started: it left the queue before it was admitted, so it has no exit code",
        )],
        Pane::NoLog => vec![Line::from(
            "no log: this run was recorded before taskslash kept logs",
        )],
        Pane::Log(path) => match (&app.output.path, &app.output.error) {
            (Some(p), Some(e)) if p == path => vec![Line::from(e.clone())],
            (Some(p), None) if p == path => {
                app.log_lines(height).into_iter().map(Line::from).collect()
            }
            _ => vec![Line::from(format!("reading {path}"))],
        },
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
