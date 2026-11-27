//! The record of runs: one SQLite table, written only by the daemon.

use crate::Error;
use crate::estimate::RunCost;
use crate::measure::Cost;
use crate::protocol::{Class, KeySummary, Kind, Launch, Run};
use rusqlite::types::Type;
use rusqlite::{Connection, Row, named_params, params};
use std::env;
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many runs `history` returns when not told.
pub const HISTORY_LIMIT: usize = 20;

/// `$TASKSLASH_DB` when set, else `$XDG_STATE_HOME/taskslash/taskslash.db`, else
/// `~/.local/state/taskslash/taskslash.db`.
pub fn db_path() -> Result<PathBuf, Error> {
    let set = |name| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(path) = set("TASKSLASH_DB") {
        return Ok(path);
    }
    let state = set("XDG_STATE_HOME")
        .or_else(|| set("HOME").map(|home| home.join(".local").join("state")))
        .ok_or_else(|| Error::new("set TASKSLASH_DB, XDG_STATE_HOME or HOME"))?;
    Ok(state.join("taskslash").join("taskslash.db"))
}

/// A run's key: `--name` when given, else the first two words of its argv.
pub fn key(name: Option<&str>, argv: &[String]) -> String {
    match name {
        Some(name) => name.to_owned(),
        None => argv
            .iter()
            .take(2)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The mean of the machine's `some avg10` pressures over a run; `None` when no sample fell inside.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RunPressure {
    pub cpu: Option<f64>,
    pub mem: Option<f64>,
    pub io: Option<f64>,
}

/// A finished job, as the daemon knows it.
pub struct Finished<'a> {
    pub key: &'a str,
    pub kind: Kind,
    pub argv: &'a [String],
    pub cwd: &'a str,
    pub class: Class,
    pub submitter: &'a str,
    pub launch: Launch,
    pub started_at: SystemTime,
    pub wall_ms: u64,
    pub cost: Cost,
    pub exit_code: Option<i32>,
    pub pressure: RunPressure,
    pub cached: Option<bool>,
    pub moon_hash: Option<String>,
    pub log_path: Option<&'a str>,
}

/// The columns `from_row` reads, in its order.
const RUN_COLUMNS: &str = "id, key, kind, argv, cwd, launch, started_at, wall_ms, cpu_usec,
    peak_cpu_5s, peak_mem_bytes, written_bytes, exit_code, machine_cpu_some, machine_mem_some,
    machine_io_some, class, submitter, cached, moon_hash, log_path";

/// A run's submitter is `:within`, or ends in `/<:within>`, or `:within` is null:
/// [`submitted_from`](crate::protocol::submitted_from) in SQL.
const WITHIN: &str = "(:within IS NULL OR submitter = :within
    OR (length(submitter) > length(:within)
        AND substr(submitter, length(submitter) - length(:within)) = '/' || :within))";

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating it and its directory when missing) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| Error::new(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| Error::new(format!("cannot open {}: {e}", path.display())))?;
        Self::with(conn)
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self::with(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn with(conn: Connection) -> Result<Self, Error> {
        // Every run's row is written while its client waits for `recorded`. In the default
        // rollback journal each write is several fsyncs (8 to 11 ms on btrfs); in WAL with
        // `synchronous = NORMAL` only a checkpoint (about every 1000 pages) fsyncs. A power cut
        // can lose the rows written since the last checkpoint, never the database.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
            .map_err(sql)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY,
                key TEXT NOT NULL,
                kind TEXT NOT NULL,
                argv TEXT NOT NULL,
                cwd TEXT NOT NULL,
                class TEXT NOT NULL,
                submitter TEXT NOT NULL,
                launch TEXT NOT NULL,
                started_at TEXT NOT NULL,
                wall_ms INTEGER NOT NULL,
                cpu_usec INTEGER,
                peak_cpu_5s REAL,
                peak_mem_bytes INTEGER,
                written_bytes INTEGER,
                exit_code INTEGER,
                machine_cpu_some REAL,
                machine_mem_some REAL,
                machine_io_some REAL,
                cached INTEGER,
                moon_hash TEXT,
                log_path TEXT
            );
            CREATE INDEX IF NOT EXISTS runs_key ON runs (key, started_at);",
        )
        .map_err(sql)?;
        // A database from before logs were kept gains the column; its rows have no log.
        let has_log_path: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('runs') WHERE name = 'log_path')",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !has_log_path {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN log_path TEXT")
                .map_err(sql)?;
        }
        Ok(Self { conn })
    }

    /// Writes one run and returns its id.
    pub fn record(&self, run: &Finished) -> Result<i64, Error> {
        let argv = serde_json::to_string(run.argv).map_err(|e| Error::new(e.to_string()))?;
        let started_ms = run
            .started_at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let int = |v: Option<u64>| v.map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        self.conn
            .execute(
                "INSERT INTO runs (key, kind, argv, cwd, launch, started_at, wall_ms, cpu_usec,
                    peak_cpu_5s, peak_mem_bytes, written_bytes, exit_code, machine_cpu_some,
                    machine_mem_some, machine_io_some, class, submitter, cached, moon_hash,
                    log_path)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                    strftime('%Y-%m-%dT%H:%M:%fZ', ?6 / 1000.0, 'unixepoch'),
                    ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    run.key,
                    run.kind.as_str(),
                    argv,
                    run.cwd,
                    run.launch.as_str(),
                    started_ms,
                    int(Some(run.wall_ms)),
                    int(run.cost.cpu_usec),
                    run.cost.peak_cpu_5s,
                    int(run.cost.peak_mem_bytes),
                    int(run.cost.written_bytes),
                    run.exit_code,
                    run.pressure.cpu,
                    run.pressure.mem,
                    run.pressure.io,
                    run.class.as_str(),
                    run.submitter,
                    run.cached,
                    run.moon_hash,
                    run.log_path,
                ],
            )
            .map_err(sql)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest runs of `key`, at most `limit`, newest first. With `within`, only the runs whose
    /// submitter is `within` or ends in `/<within>` (its directory name).
    pub fn history(
        &self,
        key: &str,
        limit: usize,
        within: Option<&str>,
    ) -> Result<Vec<Run>, Error> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE key = :key AND {WITHIN}
                 ORDER BY started_at DESC, id DESC LIMIT :limit"
            ))
            .map_err(sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        statement
            .query_map(
                named_params! {":key": key, ":limit": limit, ":within": within},
                from_row,
            )
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Every key with runs, the most recently run first: how many, and the newest run's start,
    /// exit code and submitter. The newest run is the one `history` puts first.
    pub fn keys(&self) -> Result<Vec<KeySummary>, Error> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT key, runs, started_at, exit_code, submitter FROM (
                    SELECT key, started_at, exit_code, submitter,
                        COUNT(*) OVER (PARTITION BY key) AS runs,
                        ROW_NUMBER() OVER (PARTITION BY key ORDER BY started_at DESC, id DESC) AS n
                    FROM runs)
                 WHERE n = 1 ORDER BY started_at DESC, key",
            )
            .map_err(sql)?;
        statement
            .query_map([], |row| {
                Ok(KeySummary {
                    key: row.get(0)?,
                    runs: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                    last_started_at: row.get(2)?,
                    last_exit_code: row.get(3)?,
                    last_submitter: row.get(4)?,
                })
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// The run with id `id`.
    pub fn run(&self, id: i64) -> Result<Option<Run>, Error> {
        self.one(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
            params![id],
        )
    }

    /// The newest run of `key`.
    pub fn last(&self, key: &str) -> Result<Option<Run>, Error> {
        self.one(
            &format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE key = ?1
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            params![key],
        )
    }

    /// The newest run, of `key` when given, whose exit code is set and not 0; with `within`, only
    /// the runs submitted from there, matched as `history` matches it. A run whose client went
    /// away has no exit code and does not count.
    pub fn last_failed(
        &self,
        key: Option<&str>,
        within: Option<&str>,
    ) -> Result<Option<Run>, Error> {
        self.one(
            &format!(
                "SELECT {RUN_COLUMNS} FROM runs
                 WHERE exit_code IS NOT NULL AND exit_code != 0
                    AND (:key IS NULL OR key = :key) AND {WITHIN}
                 ORDER BY started_at DESC, id DESC LIMIT 1"
            ),
            named_params! {":key": key, ":within": within},
        )
    }

    fn one(&self, query: &str, params: impl rusqlite::Params) -> Result<Option<Run>, Error> {
        let mut statement = self.conn.prepare(query).map_err(sql)?;
        let mut rows = statement.query_map(params, from_row).map_err(sql)?;
        rows.next().transpose().map_err(sql)
    }

    /// Every run with a log, and when it ended (its start plus its wall time), in ms since the
    /// epoch.
    pub fn log_ends(&self) -> Result<Vec<(String, i64)>, Error> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT log_path,
                    CAST(ROUND((julianday(started_at) - 2440587.5) * 86400000.0) AS INTEGER)
                        + wall_ms
                 FROM runs WHERE log_path IS NOT NULL",
            )
            .map_err(sql)?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Every measured run of `key`, newest first, for its estimate (which skips the cached ones). Not capped: the newest 10 calm
    /// runs may sit behind any number of starved ones.
    pub fn costs(&self, key: &str) -> Result<Vec<RunCost>, Error> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT wall_ms, cpu_usec, peak_cpu_5s, peak_mem_bytes, written_bytes, exit_code,
                    machine_cpu_some, cached
                 FROM runs WHERE key = ?1 AND cpu_usec IS NOT NULL
                    AND peak_mem_bytes IS NOT NULL AND exit_code IS NOT NULL
                 ORDER BY started_at DESC, id DESC",
            )
            .map_err(sql)?;
        statement
            .query_map(params![key], |row| {
                Ok(RunCost {
                    wall_ms: row.get(0)?,
                    cpu_usec: row.get(1)?,
                    peak_cpu_5s: row.get(2)?,
                    peak_mem_bytes: row.get(3)?,
                    written_bytes: row.get(4)?,
                    exit_code: row.get(5)?,
                    machine_cpu_some: row.get(6)?,
                    cached: row.get::<_, Option<bool>>(7)?.unwrap_or(false),
                })
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }

    /// Whether `key` is seen: at least one of its runs finished (has an exit code), measured or not,
    /// and was not a moon cache hit (which says nothing of what the task costs).
    pub fn seen(&self, key: &str) -> Result<bool, Error> {
        self.conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM runs WHERE key = ?1 AND exit_code IS NOT NULL
                    AND cached IS NOT 1)",
                params![key],
                |row| row.get(0),
            )
            .map_err(sql)
    }
}

fn from_row(row: &Row) -> rusqlite::Result<Run> {
    // A cell that does not parse fails the query by name rather than reading as another run.
    let bad = |column: usize, e: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(e))
    };
    let kind: String = row.get(2)?;
    let argv: String = row.get(3)?;
    let launch: String = row.get(5)?;
    let class: String = row.get(16)?;
    Ok(Run {
        id: row.get(0)?,
        key: row.get(1)?,
        kind: serde_json::from_value(kind.into()).map_err(|e| bad(2, e))?,
        argv: serde_json::from_str(&argv).map_err(|e| bad(3, e))?,
        cwd: row.get(4)?,
        class: serde_json::from_value(class.into()).map_err(|e| bad(16, e))?,
        submitter: row.get(17)?,
        launch: serde_json::from_value(launch.into()).map_err(|e| bad(5, e))?,
        started_at: row.get(6)?,
        wall_ms: row.get(7)?,
        cpu_usec: row.get(8)?,
        peak_cpu_5s: row.get(9)?,
        peak_mem_bytes: row.get(10)?,
        written_bytes: row.get(11)?,
        exit_code: row.get(12)?,
        machine_cpu_some: row.get(13)?,
        machine_mem_some: row.get(14)?,
        machine_io_some: row.get(15)?,
        cached: row.get(18)?,
        moon_hash: row.get(19)?,
        log_path: row.get(20)?,
    })
}

fn sql(e: rusqlite::Error) -> Error {
    Error::new(format!("database: {e}"))
}

#[cfg(test)]
mod tests;
