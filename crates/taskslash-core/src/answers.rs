//! The answers the database alone gives: what ran, and where its log is. The daemon gives them,
//! and so does a client when no daemon answers, from the database opened read-only.

use crate::Error;
use crate::estimate::estimate;
use crate::protocol::{ClientMessage, DaemonMessage, Kind, LogFound, LogSelect, Run};
use crate::store::{self, HISTORY_LIMIT, Store};
use std::path::Path;
use std::sync::Once;

/// The key a typed `key` looks up: as typed when runs were recorded under it (a key from before
/// keys were shaped), else shaped as a run's key is, so a long or path-like one finds the runs it
/// named.
pub(crate) fn lookup_key(store: &Store, key: &str) -> Result<String, Error> {
    let shaped = store::key(Some(key), &[]);
    if shaped != key && store.seen(key)? {
        return Ok(key.to_owned());
    }
    Ok(shaped)
}

/// The key's runs (at most `limit`, of `within` when given), and its estimate under the newest
/// run's kind (`command` with no runs).
pub(crate) fn runs(
    store: &Store,
    key: &str,
    limit: usize,
    within: Option<&str>,
) -> Result<DaemonMessage, Error> {
    let key = &lookup_key(store, key)?;
    let runs = store.history(key, limit, within)?;
    let kind = runs.first().map_or(Kind::Command, |run| run.kind);
    let estimate = estimate(&store.costs(key)?, kind);
    Ok(DaemonMessage::Runs { estimate, runs })
}

/// A recorded run as `log` answers it: the run, its log and whose it is.
pub(crate) fn recorded(run: Option<Run>) -> Option<LogFound> {
    run.map(|run| LogFound {
        job: None,
        run: Some(run.id),
        log_path: run.log_path,
        never_ran: false,
        key: Some(run.key),
        submitter: Some(run.submitter),
        exit_code: run.exit_code,
        started_at: Some(run.started_at),
    })
}

/// What a daemon with no jobs answers `message` from `store`: the history, the keys, the newest
/// runs, and a log by run, by key or the last that failed. `None` for any other message, which needs the daemon (a
/// job's number is the daemon's, and only while it runs).
pub(crate) fn from_store(
    store: &Store,
    message: &ClientMessage,
) -> Option<Result<DaemonMessage, Error>> {
    Some(match message {
        ClientMessage::History { key, limit, within } => runs(
            store,
            key,
            limit.unwrap_or(HISTORY_LIMIT),
            within.as_deref(),
        ),
        ClientMessage::Keys => store.keys().map(|keys| DaemonMessage::Keys { keys }),
        ClientMessage::Recent { limit } => store
            .recent(*limit)
            .map(|runs| DaemonMessage::Recent { runs }),
        ClientMessage::Log { select } => {
            let found = match select {
                LogSelect::Job { .. } => return None,
                LogSelect::Run { run } => store.run(*run).map(recorded),
                LogSelect::Last { key } => lookup_key(store, key)
                    .and_then(|key| store.last(&key))
                    .map(recorded),
                LogSelect::Failed { key, within } => key
                    .as_deref()
                    .map(|key| lookup_key(store, key))
                    .transpose()
                    .and_then(|key| store.last_failed(key.as_deref(), within.as_deref()))
                    .map(recorded),
            };
            found.map(|found| DaemonMessage::Log { found })
        }
        _ => return None,
    })
}

/// Answers `message` from the database at `db`, read-only, for a client that found no daemon at
/// `socket`, and says so on stderr (once a process). `None` when the message needs the daemon or
/// there is no database, so the caller reports that no daemon answered.
pub(crate) fn offline(
    socket: &Path,
    db: &Path,
    message: &ClientMessage,
) -> Option<Result<DaemonMessage, Error>> {
    if !db.is_file() {
        return None;
    }
    let store = match Store::open_read_only(db) {
        Ok(store) => store,
        Err(e) => return Some(Err(e)),
    };
    let answer = from_store(&store, message)?;
    static SAID: Once = Once::new();
    SAID.call_once(|| {
        eprintln!(
            "taskslash: no daemon at {}; read {} directly",
            socket.display(),
            db.display()
        );
    });
    Some(answer)
}
