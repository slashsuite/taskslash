//! The event loop around [`App`]: the terminal, the threads that poll and subscribe, the keys and
//! the log reads.

use super::{App, RECENT, render};
use crate::text::cancel_text;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Stdout};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};
use taskslash_core::protocol::{JobEvent, Status};
use taskslash_core::{Error, Subscription};

/// How much of a log is read when it is selected, and the most read in one go after that.
const TAIL: u64 = 64 * 1024;
const LOG_EVERY: Duration = Duration::from_millis(250);
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// Seconds between SIGTERM and SIGKILL when `c` cancels a running job, as `taskslash cancel`.
const GRACE_S: u64 = 10;

enum Message {
    Status(Box<Result<Status, Error>>),
    Event(JobEvent),
    Notice(String),
    /// The event stream was lost and is followed again.
    Resubscribed,
}

/// Runs `taskslash top` against the daemon at `socket` until a quit key. An `Err` before the
/// screen is entered: no daemon answers (exit 1), or stdout is not a terminal (a usage error).
pub fn run(socket: &Path) -> Result<i32, Error> {
    taskslash_core::status(socket)?;
    if !io::stdout().is_terminal() {
        return Err(Error::usage("top needs a terminal, and stdout is not one"));
    }
    // Subscribed before the status that fills the list, so each job in it or queued after it is
    // known when its `finished` comes.
    let subscription = taskslash_core::subscribe(socket)?;
    let mut app = App::new();
    app.status(taskslash_core::status(socket)?);
    match taskslash_core::recent(socket, RECENT) {
        Ok(runs) => app.seed(runs),
        Err(e) => app.notice(format!("recorded runs: {e}")),
    }

    let (tx, rx) = mpsc::channel();
    spawn_events(socket.to_owned(), subscription, tx.clone());
    spawn_status(socket.to_owned(), tx.clone());

    let mut terminal =
        enter().map_err(|e| Error::new(format!("cannot set up the terminal: {e}")))?;
    let _restore = Restore;
    let mut tail: Option<LogTail> = None;
    let mut log_read = Instant::now();
    loop {
        terminal
            .draw(|frame| render(&app, frame))
            .map_err(|e| Error::new(format!("cannot draw: {e}")))?;
        let input = |e: io::Error| Error::new(format!("cannot read the keyboard: {e}"));
        if event::poll(Duration::from_millis(50)).map_err(input)?
            && let Event::Key(key) = event::read().map_err(input)?
            && key.kind == KeyEventKind::Press
        {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            if !matches!(key.code, KeyCode::Char('c')) || control {
                app.other_key();
            }
            match key.code {
                KeyCode::Char('c') if control => return Ok(0),
                KeyCode::Esc if app.help_shown() => app.toggle_help(),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(0),
                KeyCode::Char('?') => app.toggle_help(),
                KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                KeyCode::Char('c') => {
                    if let Some(job) = app.cancel_key() {
                        spawn_cancel(socket.to_owned(), job, tx.clone());
                    }
                }
                KeyCode::Enter => {
                    if let Some(path) = app.wanted_log().map(str::to_owned)
                        && let Some(problem) = page(&mut terminal, &path)?
                    {
                        app.notice(problem);
                    }
                }
                _ => {}
            }
        }
        while let Ok(message) = rx.try_recv() {
            match message {
                Message::Status(result) => match *result {
                    Ok(status) => app.status(status),
                    Err(e) => app.status_failed(e.to_string()),
                },
                Message::Event(event) => app.event(event),
                Message::Notice(text) => app.notice(text),
                Message::Resubscribed => app.resubscribed(),
            }
        }
        let wanted = app.wanted_log().map(str::to_owned);
        let changed = wanted.as_deref() != tail.as_ref().map(|t| t.path.as_str());
        if changed {
            tail = wanted.map(LogTail::new);
        }
        if changed || log_read.elapsed() >= LOG_EVERY {
            log_read = Instant::now();
            if let Some(t) = &mut tail {
                match t.read() {
                    Ok((true, bytes)) => app.log_reset(&t.path, &bytes),
                    Ok((false, bytes)) if !bytes.is_empty() => app.log_append(&bytes),
                    Ok(_) => {}
                    Err(e) => app.log_error(&t.path, e),
                }
            }
        }
    }
}

/// Follows the events on a thread of its own. When the stream ends it says so and subscribes
/// again every second; once that works, the app is told.
fn spawn_events(socket: PathBuf, first: Subscription, tx: Sender<Message>) {
    thread::spawn(move || {
        let mut subscription = Ok(first);
        loop {
            let why = match subscription
                .and_then(|s| s.each(|event| tx.send(Message::Event(event)).is_ok()))
            {
                Ok(()) => "the daemon closed the event stream".to_owned(),
                Err(e) => e.to_string(),
            };
            let text = format!("events: {why}; trying again");
            if tx.send(Message::Notice(text)).is_err() {
                return;
            }
            thread::sleep(STATUS_EVERY);
            subscription = taskslash_core::subscribe(&socket);
            if subscription.is_ok() && tx.send(Message::Resubscribed).is_err() {
                return;
            }
        }
    });
}

/// Polls `status` once a second on a thread of its own.
fn spawn_status(socket: PathBuf, tx: Sender<Message>) {
    thread::spawn(move || {
        loop {
            thread::sleep(STATUS_EVERY);
            if tx
                .send(Message::Status(Box::new(taskslash_core::status(&socket))))
                .is_err()
            {
                return;
            }
        }
    });
}

/// Cancels job `job` on a thread of its own, so a slow daemon does not stall the screen, and says
/// how it went as a notice.
fn spawn_cancel(socket: PathBuf, job: u64, tx: Sender<Message>) {
    thread::spawn(move || {
        let text = match taskslash_core::cancel(&socket, job, GRACE_S) {
            Ok(cancelled) => match cancel_text(&cancelled, job, GRACE_S) {
                Ok(text) | Err(text) => text,
            },
            Err(e) => format!("cannot cancel job {job}: {e}"),
        };
        let line: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let _ = tx.send(Message::Notice(line.join("; ")));
    });
}

/// Shows the log at `path` in `$PAGER` (`less` when unset), off the alternate screen, and comes
/// back to it. `Some` says why the pager did not work; an `Err` when the screen cannot be set
/// up again.
fn page(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    path: &str,
) -> Result<Option<String>, Error> {
    let pager = std::env::var("PAGER")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "less".to_owned());
    restore();
    // Out of raw mode, Ctrl-C in the pager (stopping `less`'s F) signals the whole foreground
    // group, top too: ignore SIGINT and SIGQUIT here while the pager runs, as git does, and give
    // the pager the default back.
    const KEPT: [libc::c_int; 2] = [libc::SIGINT, libc::SIGQUIT];
    // SAFETY: signal() only swaps the disposition, and is async-signal-safe in the child.
    let previous = KEPT.map(|s| unsafe { libc::signal(s, libc::SIG_IGN) });
    // Through the shell, as git runs it, so a PAGER with flags (`less -R`) works.
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{pager} \"$1\""))
        .arg("sh")
        .arg(path);
    // SAFETY: the closure calls only signal(), which is async-signal-safe between fork and exec.
    unsafe {
        command.pre_exec(|| {
            for s in KEPT {
                libc::signal(s, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let ran = command.status();
    for (s, handler) in KEPT.into_iter().zip(previous) {
        // SAFETY: as above; puts back what was there before.
        unsafe { libc::signal(s, handler) };
    }
    enable_raw_mode()
        .and_then(|()| execute!(io::stdout(), EnterAlternateScreen))
        .and_then(|()| terminal.clear())
        .map_err(|e| Error::new(format!("cannot set up the terminal again: {e}")))?;
    Ok(match ran {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("{pager} ended with {status}")),
        Err(e) => Some(format!("cannot run {pager}: {e}")),
    })
}

/// Enters raw mode and the alternate screen, with a panic hook that leaves them first, so a
/// panic's message lands on the normal screen.
fn enter() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    enable_raw_mode()?;
    // From here on, a failure must leave raw mode again.
    let restore_on_error = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    std::mem::forget(restore_on_error);
    Ok(terminal)
}

/// Leaves raw mode and the alternate screen and shows the cursor. Safe to run twice.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
}

/// Restores the terminal when dropped: on a quit, an error, or a panic's unwind.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore();
    }
}

/// A log file read from where the last read stopped.
pub(super) struct LogTail {
    pub path: String,
    file: Option<File>,
    offset: u64,
    /// Whether the tail has been read once.
    read: bool,
}

impl LogTail {
    pub fn new(path: String) -> Self {
        Self {
            path,
            file: None,
            offset: 0,
            read: false,
        }
    }

    /// What is new in the file: `(true, its tail)` on the first read, when the file shrank, or
    /// when more than [`TAIL`] was appended since the last read; else `(false, the appended
    /// bytes)`. A tail that starts inside a line starts at the next one.
    pub fn read(&mut self) -> Result<(bool, Vec<u8>), String> {
        let cannot = |e: io::Error| format!("cannot read {}: {e}", self.path);
        if self.file.is_none() {
            self.file = Some(File::open(&self.path).map_err(cannot)?);
        }
        let file = self.file.as_mut().expect("opened above");
        let len = file.metadata().map_err(cannot)?.len();
        let fresh = !self.read || len < self.offset || len - self.offset > TAIL;
        if fresh {
            self.offset = len.saturating_sub(TAIL);
            self.read = true;
        }
        file.seek(SeekFrom::Start(self.offset)).map_err(cannot)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(len - self.offset)
            .read_to_end(&mut bytes)
            .map_err(cannot)?;
        let from = self.offset;
        self.offset += bytes.len() as u64;
        if fresh && from > 0 {
            let start = bytes.iter().position(|&b| b == b'\n').map_or(0, |n| n + 1);
            bytes.drain(..start);
        }
        Ok((fresh, bytes))
    }
}
