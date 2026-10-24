//! The ledger: one embedded SQLite database (WAL) under the state directory, indexed by Kind and
//! normalized occurrence time, with the delivery outbox and the contract catalog beside the
//! records. Writers return errors; only the lifecycle-hook boundary converts them to notices so
//! telemetry cannot block a tool call.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::{Config, Envelope};

pub const DATABASE_VERSION: i64 = 1;

/// The schema a fresh store is created with, and an empty read is answered from.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS records (
    ts TEXT NOT NULL, kind TEXT NOT NULL, schema_version INTEGER NOT NULL, payload TEXT NOT NULL,
    metadata TEXT NOT NULL, occurred_seconds INTEGER NOT NULL, occurred_nanos INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_records_id ON records(json_extract(metadata,'$.record_id')) WHERE json_extract(metadata,'$.record_id') IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_records_kind_time ON records(kind,occurred_seconds,occurred_nanos);
CREATE INDEX IF NOT EXISTS idx_records_time ON records(occurred_seconds,occurred_nanos);";

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

/// Whether a store this build can write: absent (created on first write) or at its schema.
pub fn write_readiness(path: &Path) -> crate::Result<()> {
    if !path.try_exists().map_err(io)? {
        return Ok(());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    let version = check_version(&conn)?;
    if has_records(&conn)? && version != DATABASE_VERSION {
        return Err(unsupported(version));
    }
    Ok(())
}

fn unsupported(version: i64) -> crate::Error {
    io(format!(
        "SQLite schema {version} is not one this build created (it reads {DATABASE_VERSION})"
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
    conn.busy_timeout(std::time::Duration::from_millis(100))
        .map_err(io)?;
    check_version(&conn)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;").map_err(io)?;
    crate::query::functions(&conn).map_err(io)?;
    Ok(conn)
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
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(io)?;
    let version = check_version(&tx)?;
    if has_records(&tx)? {
        if version != DATABASE_VERSION {
            return Err(unsupported(version));
        }
    } else {
        tx.execute_batch(SCHEMA).map_err(io)?;
        tx.execute_batch(crate::outbox::SCHEMA).map_err(io)?;
        tx.execute_batch(crate::catalog::SCHEMA).map_err(io)?;
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
    let io = |e: rusqlite::Error| crate::Error::Io(e.to_string());
    let (ts, kind, schema_version, payload, metadata): (String, String, u32, String, String) = conn.query_row(
        "SELECT ts,kind,schema_version,payload,metadata FROM records WHERE json_extract(metadata,'$.record_id')=?1",
        [id], |r|Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).map_err(io)?;
    Ok(Envelope {
        ts,
        kind,
        schema_version,
        payload: serde_json::from_str(&payload).map_err(|e| crate::Error::Io(e.to_string()))?,
        metadata: serde_json::from_str(&metadata).map_err(|e| crate::Error::Io(e.to_string()))?,
    })
}

/// Every Kind the ledger holds records for, sorted — an exact statement about storage, which is
/// what lets a diagnostic compare what was collected against what the current registry can read.
///
/// A store that has never been written yields an empty list; one that cannot be read yields an
/// error, because "nothing was collected" and "nothing could be looked at" are the two answers a
/// diagnostic must never confuse.
pub fn stored_kinds(cfg: &Config) -> std::io::Result<Vec<String>> {
    let path = cfg.database();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let io = |e: rusqlite::Error| std::io::Error::other(e.to_string());
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    let mut stmt = conn
        .prepare("SELECT DISTINCT kind FROM records ORDER BY kind")
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
    let path = cfg.database();
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT occurred_seconds FROM records WHERE kind=?1 ORDER BY occurred_seconds DESC LIMIT 1",
        [kind],
        |row| row.get(0),
    )
    .ok()
}

/// Rows deleted per DELETE statement during the retention sweep. Batching keeps each writer-lock
/// window short, so a first sweep over a months-old backlog can't hold the WAL write lock long
/// enough to starve concurrent hook inserts.
const PRUNE_BATCH: i64 = 5_000;

/// Remove records that occurred before the retention horizon, with their delivery references and
/// the queue capacity they held. Destructive, so it belongs to exactly one caller: the receiver
/// (which holds the single-writer state lock) — never the hook, and never a read path. Returns
/// rows deleted; a missing DB is zero, and any error is logged (fail-open) rather than propagated.
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
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("CREATE TEMP TABLE prune_records (id INTEGER PRIMARY KEY);")?;
        let mut total = 0;
        loop {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM prune_records", [])?;
            let count = tx.execute(
                "INSERT INTO prune_records SELECT rowid FROM records WHERE occurred_seconds<?1 LIMIT ?2",
                rusqlite::params![retention.cutoff, PRUNE_BATCH],
            )?;
            if count == 0 {
                tx.commit()?;
                break;
            }
            let pending = {
                let mut stmt = tx.prepare("SELECT d.destination,COUNT(*),SUM(d.size_bytes) FROM deliveries d JOIN records r ON json_extract(r.metadata,'$.record_id')=d.record_id WHERE r.rowid IN (SELECT id FROM prune_records) AND d.status='pending' GROUP BY d.destination")?;
                stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (destination, count, bytes) in pending {
                tx.execute("UPDATE delivery_queues SET pending_records=pending_records-?1,pending_bytes=pending_bytes-?2 WHERE destination=?3",rusqlite::params![count,bytes,destination])?;
            }
            tx.execute("DELETE FROM deliveries WHERE record_id IN (SELECT json_extract(metadata,'$.record_id') FROM records WHERE rowid IN (SELECT id FROM prune_records))",[])?;
            tx.execute(
                "DELETE FROM records WHERE rowid IN (SELECT id FROM prune_records)",
                [],
            )?;
            tx.commit()?;
            total += count;
        }
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

pub struct SqliteSink {
    conn: Result<Connection, String>,
    targets: Vec<crate::ExportTarget>,
    registry: crate::Registry,
}

impl SqliteSink {
    pub fn open(path: PathBuf, registry: &crate::Registry) -> Self {
        Self {
            conn: Self::init(&path).map_err(|e| e.to_string()),
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
            initialize(&mut conn)?;
        }
        Ok(conn)
    }

    pub fn write_record(&mut self, env: &Envelope) -> crate::Result<WriteReceipt> {
        let conn = self
            .conn
            .as_mut()
            .map_err(|e| crate::Error::Io(e.clone()))?;
        let payload =
            serde_json::to_string(&env.payload).map_err(|e| crate::Error::Io(e.to_string()))?;
        let occurred = env
            .ts
            .parse::<jiff::Timestamp>()
            .map_err(|_| crate::Error::InvalidPayload("invalid record timestamp".into()))?;
        let (seconds, nanos) = crate::model::timestamp_parts(occurred);
        let tx = conn
            .transaction()
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        if let Some(spec) = self.registry.kind(&env.kind)
            && env.metadata.schema_id.as_deref() == Some(spec.schema_id.as_str())
        {
            crate::catalog::save(&tx, spec)?;
        }
        let inserted = tx.execute(
            "INSERT INTO records (ts, kind, schema_version, payload, metadata, occurred_seconds,occurred_nanos) VALUES (?1, ?2, ?3, ?4, ?5, ?6,?7) ON CONFLICT DO NOTHING",
            rusqlite::params![env.ts, env.kind, env.schema_version, payload, serde_json::to_string(&env.metadata).map_err(|e| crate::Error::Io(e.to_string()))?, seconds,nanos],
        ).map_err(|e| crate::Error::Io(e.to_string()))?;
        if inserted == 0 {
            let id = env.metadata.record_id.as_deref().ok_or_else(|| {
                crate::Error::InvalidPayload("duplicate record without id".into())
            })?;
            let stored = record_by_id(&tx, id)?;
            if !env.same_record(&stored) {
                return Err(crate::Error::InvalidPayload(
                    "record id already describes different data".into(),
                ));
            }
            tx.commit().map_err(|e| crate::Error::Io(e.to_string()))?;
            return Ok(WriteReceipt {
                record_id: Some(id.into()),
                delivery: Default::default(),
            });
        }
        let delivery = crate::outbox::enqueue(&tx, env, &self.targets, crate::now_epoch())
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        tx.commit().map_err(|e| crate::Error::Io(e.to_string()))?;
        Ok(WriteReceipt {
            record_id: env.metadata.record_id.clone(),
            delivery,
        })
    }
}
