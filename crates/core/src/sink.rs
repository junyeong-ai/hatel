//! The ledger: one embedded SQLite database (WAL) under the state directory. A record references
//! its Kind, contract and project by id, and the `envelopes` view joins them back into the record
//! every reader sees. The delivery outbox and the contract catalog live beside the records. Writers
//! return errors; only the lifecycle-hook boundary converts them to notices so telemetry cannot
//! block a tool call.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use crate::model::RecordMetadata;
use crate::{Config, Envelope, ProjectRef};

pub const DATABASE_VERSION: i64 = 1;

/// The schema a fresh store is created with, and an empty read is answered from. A Kind, a contract
/// and a project repeat across most records, so each is stored once and referenced by id.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY, key TEXT NOT NULL, label TEXT NOT NULL, UNIQUE (key, label)
);
CREATE TABLE IF NOT EXISTS records (
    id INTEGER PRIMARY KEY,
    kind_id INTEGER NOT NULL REFERENCES kinds (id),
    occurred_seconds INTEGER NOT NULL, occurred_nanos INTEGER NOT NULL, ts TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    contract_id INTEGER REFERENCES contracts (id), kind_version INTEGER,
    record_id TEXT UNIQUE, observed_at TEXT, project_id INTEGER REFERENCES projects (id),
    source TEXT, run_id TEXT, correlation_id TEXT, projection INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_records_kind_time ON records (kind_id, occurred_seconds, occurred_nanos);
CREATE VIEW IF NOT EXISTS envelopes AS SELECT
    r.id, k.name AS kind, r.ts, r.occurred_seconds, r.occurred_nanos, r.schema_version, r.payload,
    r.projection, r.record_id, r.observed_at, p.key AS project_key, p.label AS project_label,
    r.source, r.run_id, r.correlation_id, r.kind_version, c.schema_id
FROM records r JOIN kinds k ON k.id = r.kind_id
LEFT JOIN contracts c ON c.id = r.contract_id LEFT JOIN projects p ON p.id = r.project_id;";

/// The `envelopes` columns [`envelope`] reads, in its order, from the view aliased `e`.
pub const ENVELOPE_COLUMNS: &str = "e.ts, e.kind, e.schema_version, e.payload, e.projection, \
    e.record_id, e.observed_at, e.project_key, e.project_label, e.source, e.run_id, \
    e.correlation_id, e.kind_version, e.schema_id";

/// The record a row of [`ENVELOPE_COLUMNS`] holds.
pub fn envelope(row: &rusqlite::Row<'_>) -> rusqlite::Result<Envelope> {
    let payload: String = row.get(3)?;
    Ok(Envelope {
        ts: row.get(0)?,
        kind: row.get(1)?,
        schema_version: row.get(2)?,
        payload: serde_json::from_str(&payload).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?,
        metadata: RecordMetadata {
            projection: row.get(4)?,
            record_id: row.get(5)?,
            observed_at: row.get(6)?,
            project: row
                .get::<_, Option<String>>(7)?
                .zip(row.get(8)?)
                .map(|(key, label)| ProjectRef { key, label }),
            source: row.get(9)?,
            run_id: row.get(10)?,
            correlation_id: row.get(11)?,
            kind_version: row.get(12)?,
            schema_id: row.get(13)?,
        },
    })
}

fn io(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(error.to_string())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteReceipt {
    pub record_id: Option<String>,
    pub delivery: crate::outbox::Enqueued,
}

pub fn build_sink(cfg: &Config, registry: &crate::Registry) -> SqliteSink {
    SqliteSink::with_exports(
        cfg.database(),
        cfg.export
            .as_ref()
            .map(|e| e.targets.clone())
            .unwrap_or_default(),
        registry,
    )
}

/// Whether a store this build can write: one [`read`] accepts, since the first write creates the
/// schema in a store that has none.
pub fn write_readiness(path: &Path) -> crate::Result<()> {
    read(path).map(drop)
}

/// The ledger opened read-only at this build's schema. A store without a records table — absent,
/// or created and not yet initialized by its first writer — reads as the empty ledger, as a fresh
/// file would; one holding records under another schema is refused with the error a write gets.
pub(crate) fn read(path: &Path) -> crate::Result<Connection> {
    if path.try_exists().map_err(io)? {
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
        let version = check_version(&conn)?;
        if has_records(&conn)? {
            return if version == DATABASE_VERSION {
                Ok(conn)
            } else {
                Err(unsupported(version))
            };
        }
    }
    empty()
}

fn unsupported(version: i64) -> crate::Error {
    io(format!(
        "SQLite schema {version} is not one this build created (it reads {DATABASE_VERSION}); move telemetry.db aside to start a new store"
    ))
}

fn has_records(conn: &Connection) -> crate::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='records')",
        [],
        |r| r.get(0),
    )
    .map_err(io)
}

fn connect(path: &Path) -> crate::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
    }
    let conn = Connection::open(path).map_err(io)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(io)?;
    check_version(&conn)?;
    crate::query::functions(&conn).map_err(io)?;
    Ok(conn)
}

/// How long a writer waits for another one's lock. A slow disk stretches every commit by its fsync
/// and parallel tool calls queue their hooks behind one another, so a writer can wait through
/// several commits. A shorter wait would lose its record to save time nothing is spending: the hook
/// is wired asynchronously, so no tool call waits on it.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A fresh file starts in rollback mode, and switching it to WAL needs the database to itself, so
/// SQLite answers a concurrent switch with BUSY at once instead of waiting on the busy timeout.
/// Once one switch lands the mode is stored in the file, so retrying lets every first writer
/// through.
fn enable_wal(conn: &Connection) -> crate::Result<()> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())) {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            result => return result.map_err(io),
        }
    }
}

pub(crate) fn check_version(conn: &Connection) -> crate::Result<i64> {
    let version = conn
        .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
        .map_err(io)?;
    if version > DATABASE_VERSION {
        return Err(io(format!(
            "SQLite schema {version} is newer than supported {DATABASE_VERSION}"
        )));
    }
    Ok(version)
}

/// Create the schema in a store that has none. A store holding records under another schema is
/// refused rather than altered: a hook writing on every lifecycle event must never convert data.
fn initialize(conn: &mut Connection) -> crate::Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(io)?;
    let version = check_version(&tx)?;
    if has_records(&tx)? {
        if version != DATABASE_VERSION {
            return Err(unsupported(version));
        }
    } else {
        tx.execute_batch(crate::catalog::SCHEMA).map_err(io)?;
        tx.execute_batch(SCHEMA).map_err(io)?;
        tx.execute_batch(crate::outbox::SCHEMA).map_err(io)?;
        tx.pragma_update(None, "user_version", DATABASE_VERSION)
            .map_err(io)?;
    }
    tx.commit().map_err(io)
}

/// An empty in-memory ledger at the current schema, which answers a read of a store that has
/// never been written exactly as an empty file would.
pub(crate) fn empty() -> crate::Result<Connection> {
    let mut conn = Connection::open_in_memory().map_err(io)?;
    initialize(&mut conn)?;
    Ok(conn)
}

pub(crate) fn record_by_id(conn: &Connection, id: &str) -> crate::Result<Envelope> {
    conn.query_row(
        &format!("SELECT {ENVELOPE_COLUMNS} FROM envelopes e WHERE e.record_id = ?1"),
        [id],
        envelope,
    )
    .map_err(io)
}

/// Every Kind the ledger holds records for, sorted — an exact statement about storage, which is
/// what lets a diagnostic compare what was collected against what the current registry can read.
///
/// A store that has never been written yields an empty list; one that cannot be read yields an
/// error, because "nothing was collected" and "nothing could be looked at" are the two answers a
/// diagnostic must never confuse.
pub fn stored_kinds(cfg: &Config) -> std::io::Result<Vec<String>> {
    let conn = read(&cfg.database()).map_err(std::io::Error::other)?;
    let io = |e: rusqlite::Error| std::io::Error::other(e.to_string());
    let mut stmt = conn
        .prepare(
            "SELECT name FROM kinds k WHERE EXISTS (SELECT 1 FROM records WHERE kind_id = k.id) \
             ORDER BY name",
        )
        .map_err(io)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(io)?;
    rows.collect::<rusqlite::Result<Vec<String>>>().map_err(io)
}

/// When the ledger last recorded an occurrence of `kind`, in epoch seconds, or `None` when it holds
/// none or cannot say. How long a kind nothing writes any more is kept follows from it
/// ([`Config::stored_until`]).
pub fn last_written(cfg: &Config, kind: &str) -> Option<i64> {
    let conn = read(&cfg.database()).ok()?;
    conn.query_row(
        "SELECT occurred_seconds FROM records WHERE kind_id = (SELECT id FROM kinds WHERE name = ?1) \
         ORDER BY occurred_seconds DESC LIMIT 1",
        [kind],
        |row| row.get(0),
    )
    .ok()
}

/// Rows deleted per DELETE statement during the retention sweep. Batching keeps each writer-lock
/// window short, so a first sweep over a months-old backlog can't hold the WAL write lock long
/// enough to starve concurrent hook inserts.
const PRUNE_BATCH: i64 = 5_000;

/// Pages one incremental-vacuum step returns to the file system. Each step is its own write
/// transaction, so a writer waits for one step at most rather than for the whole reclaim.
const RECLAIM_PAGES: u32 = 1_024;

/// Remove records that occurred before the retention horizon, with their delivery references and
/// the queue capacity they held, then return the freed pages to the file system. Destructive, so it
/// belongs to exactly one caller: the receiver (which holds the single-writer state lock) — never
/// the hook, and never a read path. Returns rows deleted; a missing DB is zero, and any error is
/// logged (fail-open) rather than propagated.
pub fn prune(cfg: &Config, retention: crate::config::Retention) -> usize {
    let path = cfg.database();
    if !path.exists() {
        return 0;
    }
    if let Err(error) = write_readiness(&path) {
        eprintln!("hatel: sqlite retention skipped: {error}");
        return 0;
    }
    let result = (|| -> rusqlite::Result<usize> {
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("CREATE TEMP TABLE prune_records (id INTEGER PRIMARY KEY);")?;
        let kinds = conn
            .prepare("SELECT id FROM kinds")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut total = 0;
        for kind_id in kinds {
            total += prune_kind(&mut conn, kind_id, retention.cutoff)?;
        }
        reclaim(&conn)?;
        Ok(total)
    })();
    match result {
        Ok(count) => count,
        Err(error) => {
            eprintln!("hatel: sqlite retention failed: {error}");
            0
        }
    }
}

fn prune_kind(conn: &mut Connection, kind_id: i64, cutoff: i64) -> rusqlite::Result<usize> {
    let mut total = 0;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM prune_records", [])?;
        let count = tx.execute(
            "INSERT INTO prune_records SELECT id FROM records \
             WHERE kind_id = ?1 AND occurred_seconds < ?2 LIMIT ?3",
            rusqlite::params![kind_id, cutoff, PRUNE_BATCH],
        )?;
        if count == 0 {
            tx.commit()?;
            return Ok(total);
        }
        let pending = tx
            .prepare(
                "SELECT d.destination, COUNT(*), SUM(d.size_bytes) FROM deliveries d \
                 JOIN records r ON r.record_id = d.record_id \
                 WHERE r.id IN (SELECT id FROM prune_records) AND d.status = 'pending' \
                 GROUP BY d.destination",
            )?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (destination, count, bytes) in pending {
            tx.execute(
                "UPDATE delivery_queues SET pending_records = pending_records - ?1, \
                 pending_bytes = pending_bytes - ?2 WHERE destination = ?3",
                rusqlite::params![count, bytes, destination],
            )?;
        }
        tx.execute(
            "DELETE FROM deliveries WHERE record_id IN \
             (SELECT record_id FROM records WHERE id IN (SELECT id FROM prune_records))",
            [],
        )?;
        tx.execute(
            "DELETE FROM records WHERE id IN (SELECT id FROM prune_records)",
            [],
        )?;
        tx.commit()?;
        total += count;
    }
}

/// Return free pages to the file system in [`RECLAIM_PAGES`] steps until none are left, or until a
/// step frees nothing, as in a file created without incremental auto-vacuum.
fn reclaim(conn: &Connection) -> rusqlite::Result<()> {
    let free = || conn.query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0));
    loop {
        let before = free()?;
        if before == 0 {
            return Ok(());
        }
        let mut step = conn.prepare(&format!("PRAGMA incremental_vacuum({RECLAIM_PAGES})"))?;
        let mut pages = step.raw_query();
        while pages.next()?.is_some() {}
        if free()? == before {
            return Ok(());
        }
    }
}

pub struct SqliteSink {
    conn: crate::Result<Connection>,
    targets: Vec<crate::ExportTarget>,
    registry: crate::Registry,
}

impl SqliteSink {
    pub fn open(path: PathBuf, registry: &crate::Registry) -> Self {
        Self {
            conn: Self::init(&path),
            targets: Vec::new(),
            registry: registry.clone(),
        }
    }

    pub fn with_exports(
        path: PathBuf,
        targets: Vec<crate::ExportTarget>,
        registry: &crate::Registry,
    ) -> Self {
        let mut sink = Self::open(path, registry);
        sink.targets = targets;
        sink
    }

    fn init(path: &Path) -> crate::Result<Connection> {
        let mut conn = connect(path)?;
        if check_version(&conn)? != DATABASE_VERSION {
            // The retention sweep returns deleted records' pages through incremental vacuum. SQLite
            // fixes the auto-vacuum mode when it writes the first page, which the WAL switch does.
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
                .map_err(io)?;
            enable_wal(&conn)?;
            initialize(&mut conn)?;
        }
        Ok(conn)
    }

    pub fn write_record(&mut self, env: &Envelope) -> crate::Result<WriteReceipt> {
        let conn = self.conn.as_mut().map_err(|e| e.clone())?;
        let payload = serde_json::to_string(&env.payload).map_err(io)?;
        let occurred = env
            .ts
            .parse::<jiff::Timestamp>()
            .map_err(|_| crate::Error::InvalidPayload("invalid record timestamp".into()))?;
        let (seconds, nanos) = crate::model::timestamp_parts(occurred);
        // Immediate, because the ids looked up first decide what is inserted: a deferred
        // transaction that read before another writer committed could not take the write lock.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io)?;
        let kind_id = crate::catalog::kind_id(&tx, &env.kind).map_err(io)?;
        let contract_id = crate::catalog::contract_id(&tx, &self.registry, env, kind_id)?;
        let project_id = env
            .metadata
            .project
            .as_ref()
            .map(|project| project_id(&tx, project))
            .transpose()
            .map_err(io)?;
        let m = &env.metadata;
        let inserted = tx
            .execute(
                "INSERT INTO records (kind_id, occurred_seconds, occurred_nanos, ts, schema_version, \
                 contract_id, kind_version, record_id, observed_at, project_id, source, run_id, \
                 correlation_id, projection, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
                 ON CONFLICT DO NOTHING",
                rusqlite::params![
                    kind_id,
                    seconds,
                    nanos,
                    env.ts,
                    env.schema_version,
                    contract_id,
                    m.kind_version,
                    m.record_id,
                    m.observed_at,
                    project_id,
                    m.source,
                    m.run_id,
                    m.correlation_id,
                    m.projection,
                    payload,
                ],
            )
            .map_err(io)?;
        if inserted == 0 {
            let id = m.record_id.as_deref().ok_or_else(|| {
                crate::Error::InvalidPayload("duplicate record without id".into())
            })?;
            let stored = record_by_id(&tx, id)?;
            if !env.same_record(&stored) {
                return Err(crate::Error::InvalidPayload(
                    "record id already describes different data".into(),
                ));
            }
            tx.commit().map_err(io)?;
            return Ok(WriteReceipt {
                record_id: Some(id.into()),
                delivery: Default::default(),
            });
        }
        let delivery =
            crate::outbox::enqueue(&tx, env, &self.targets, crate::now_epoch()).map_err(io)?;
        tx.commit().map_err(io)?;
        Ok(WriteReceipt {
            record_id: m.record_id.clone(),
            delivery,
        })
    }
}

fn project_id(conn: &Connection, project: &ProjectRef) -> rusqlite::Result<i64> {
    let key = [&project.key, &project.label];
    let stored = conn
        .query_row(
            "SELECT id FROM projects WHERE key = ?1 AND label = ?2",
            key,
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        Some(id) => Ok(id),
        None => conn.query_row(
            "INSERT INTO projects (key, label) VALUES (?1, ?2) RETURNING id",
            key,
            |r| r.get(0),
        ),
    }
}
