//! SQLite sink (embedded, WAL) — indexed storage for higher volume or richer
//! queries, indexed by Kind and normalized occurrence time.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::Sink;
use crate::Envelope;

pub const DATABASE_VERSION: i64 = 1;

fn io(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(error.to_string())
}

pub fn database_version(path: &Path) -> crate::Result<Option<i64>> {
    if !path.try_exists().map_err(io)? {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .map(Some)
        .map_err(io)
}

pub fn write_readiness(path: &Path) -> crate::Result<()> {
    if !path.try_exists().map_err(io)? {
        return Ok(());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    let version = check_version(&conn)?;
    let records: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='records')",
            [],
            |r| r.get(0),
        )
        .map_err(io)?;
    if records && version != DATABASE_VERSION {
        return Err(io(
            "SQLite schema requires maintenance; stop writers and run migrate --quiescent",
        ));
    }
    Ok(())
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

enum Initialization {
    FreshStore,
    QuiescentUpgrade,
}

fn initialize(conn: &mut Connection, mode: Initialization) -> crate::Result<usize> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(io)?;
    let version = check_version(&tx)?;
    let exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='records')",
            [],
            |r| r.get(0),
        )
        .map_err(io)?;
    if matches!(mode, Initialization::FreshStore) && exists {
        if version == DATABASE_VERSION {
            tx.commit().map_err(io)?;
            return Ok(0);
        }
        return Err(io(
            "SQLite schema requires maintenance; stop writers and run migrate --quiescent",
        ));
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS records (
        ts TEXT NOT NULL, kind TEXT NOT NULL, schema_version INTEGER NOT NULL, payload TEXT NOT NULL
    );",
    )
    .map_err(io)?;
    let columns = {
        let mut stmt = tx.prepare("PRAGMA table_info(records)").map_err(io)?;
        stmt.query_map([], |r| r.get::<_, String>(1))
            .map_err(io)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(io)?
    };
    for (name, sql) in [
        (
            "metadata",
            "ALTER TABLE records ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';",
        ),
        (
            "occurred_seconds",
            "ALTER TABLE records ADD COLUMN occurred_seconds INTEGER;",
        ),
        (
            "occurred_nanos",
            "ALTER TABLE records ADD COLUMN occurred_nanos INTEGER;",
        ),
    ] {
        if !columns.iter().any(|c| c == name) {
            tx.execute_batch(sql).map_err(io)?;
        }
    }
    let mut normalized = 0;
    let mut after = i64::MIN;
    loop {
        let rows = {
            let mut stmt=tx.prepare("SELECT rowid,ts FROM records WHERE rowid>=?1 AND (occurred_seconds IS NULL OR occurred_nanos IS NULL) ORDER BY rowid LIMIT 1000").map_err(io)?;
            stmt.query_map([after], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(io)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(io)?
        };
        if rows.is_empty() {
            break;
        }
        let mut update = tx
            .prepare("UPDATE records SET occurred_seconds=?1,occurred_nanos=?2 WHERE rowid=?3")
            .map_err(io)?;
        for (id, ts) in &rows {
            let ts = ts.parse::<jiff::Timestamp>().map_err(|_| {
                io(format!(
                    "invalid timestamp in SQLite row {id}; maintenance rolled back"
                ))
            })?;
            let (seconds, nanos) = crate::model::timestamp_parts(ts);
            update
                .execute(rusqlite::params![seconds, nanos, id])
                .map_err(io)?;
            normalized += 1;
        }
        match rows.last().expect("nonempty batch").0.checked_add(1) {
            Some(next) => after = next,
            None => break,
        }
    }
    tx.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_records_id ON records(json_extract(metadata,'$.record_id')) WHERE json_extract(metadata,'$.record_id') IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_records_kind_time ON records(kind,occurred_seconds,occurred_nanos);
        CREATE INDEX IF NOT EXISTS idx_records_time ON records(occurred_seconds,occurred_nanos);
        DROP INDEX IF EXISTS idx_records_kind_ts;
        DROP INDEX IF EXISTS idx_records_kind_occurred;").map_err(io)?;
    tx.execute_batch(crate::outbox::SCHEMA).map_err(io)?;
    tx.execute_batch(crate::catalog::SCHEMA).map_err(io)?;
    tx.pragma_update(None, "user_version", DATABASE_VERSION)
        .map_err(io)?;
    tx.commit().map_err(io)?;
    Ok(normalized)
}

pub(crate) fn upgrade_database(path: &Path) -> crate::Result<(Connection, usize)> {
    let mut conn = connect(path)?;
    let normalized = initialize(&mut conn, Initialization::QuiescentUpgrade)?;
    Ok((conn, normalized))
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

/// Every Kind the table holds records for.
pub fn stored_kinds(path: &Path) -> std::io::Result<Vec<String>> {
    // No database file means nothing has ever been written through this sink, which is an empty
    // store rather than an unreadable one.
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

/// The newest occurrence of `kind`, ordered by the parsed timestamp.
pub fn last_written(path: &Path, kind: &str) -> Option<i64> {
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    crate::query::prepare_read(&conn).ok()?;
    let ts: Option<String> = conn
        .query_row(
            "SELECT ts FROM records_read WHERE kind = ?1 AND occurred_seconds IS NOT NULL ORDER BY occurred_seconds DESC, occurred_nanos DESC,sequence DESC LIMIT 1",
            [kind],
            |row| row.get(0),
        )
        .ok()?;
    crate::ts_epoch(&ts?)
}

/// Read records for `kind` back as envelopes — the read half of the storage
/// abstraction, so a report consumes the SQLite sink exactly as it does JSONL. The
/// time window is filtered in SQL. A missing DB reads empty (no telemetry yet); a DB that
/// exists but can't be read is logged rather than silently treated as empty.
pub fn read_records(path: &Path, kind: &str, since: Option<i64>) -> Vec<Envelope> {
    if !path.exists() {
        return Vec::new();
    }
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hatel: sqlite read open failed ({e})");
            return Vec::new();
        }
    };
    if let Err(e) = crate::query::prepare_read(&conn) {
        eprintln!("hatel: sqlite read: {e}");
        return Vec::new();
    }
    let mut stmt = match conn.prepare("SELECT ts, schema_version, payload, metadata FROM records_read WHERE kind = ?1 AND occurred_seconds>=?2") {
        Ok(stmt) => stmt,
        Err(e) => { eprintln!("hatel: sqlite query: {e}"); return Vec::new(); }
    };
    let rows = stmt.query_map(rusqlite::params![kind, since.unwrap_or(i64::MIN)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    });
    let mut out = Vec::new();
    match rows {
        Ok(rows) => {
            for (ts, schema_version, payload, metadata) in rows.flatten() {
                // Skip a record stamped newer than this build understands (same guard
                // as the JSONL path) so a future format bump degrades safely.
                if schema_version > crate::SCHEMA_VERSION {
                    continue;
                }
                if let Ok(payload) = serde_json::from_str(&payload) {
                    out.push(Envelope {
                        ts,
                        kind: kind.to_string(),
                        schema_version,
                        payload,
                        metadata: serde_json::from_str(&metadata).unwrap_or_default(),
                    });
                }
            }
        }
        Err(e) => eprintln!("hatel: sqlite query failed ({e})"),
    }
    out
}

/// The oldest record of `kind` — of `project` when named — still stored, as its timestamp: how
/// far back the store reaches.
pub fn oldest_ts(path: &Path, kind: &str, project: Option<&str>) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    crate::query::prepare_read(&conn).ok()?;
    conn.query_row("SELECT ts FROM records_read WHERE kind=?1 AND occurred_seconds IS NOT NULL AND (?2 IS NULL OR json_extract(payload, '$.project')=?2) ORDER BY occurred_seconds,occurred_nanos,sequence LIMIT 1", rusqlite::params![kind,project], |r| r.get(0)).ok()
}

/// Rows deleted per DELETE statement during the retention sweep. Batching keeps each writer-lock
/// window short, so a first sweep over a months-old backlog can't hold the WAL write lock long
/// enough to starve concurrent hook inserts.
const PRUNE_BATCH: i64 = 5_000;

/// Delete rows older than `cutoff_epoch` — the SQLite half of the retention sweep. The SQL
/// cutoff is rendered one second *below* the epoch cutoff: the lexical fraction quirk (see
/// `read_records`) can then only retain a boundary row for one extra second, never delete a
/// row inside the window. Returns rows deleted; a missing DB is zero, and any error is logged
/// (fail-open) rather than propagated.
pub fn prune_records(path: &Path, cutoff_epoch: i64) -> usize {
    if !path.exists() {
        return 0;
    }
    if let Err(error) = write_readiness(path) {
        eprintln!("hatel: sqlite retention skipped: {error}");
        return 0;
    }
    let result = (|| -> rusqlite::Result<usize> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        crate::query::functions(&conn)?;
        conn.execute_batch("CREATE TEMP TABLE prune_records (id INTEGER PRIMARY KEY);")?;
        let has_deliveries: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='deliveries')",
            [],
            |r| r.get(0),
        )?;
        let mut total = 0;
        loop {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM prune_records", [])?;
            let count = tx.execute("INSERT INTO prune_records SELECT rowid FROM records WHERE occurred_seconds<?1 UNION ALL SELECT rowid FROM records WHERE occurred_seconds IS NULL AND hatel_epoch(ts)<?1 LIMIT ?2",rusqlite::params![cutoff_epoch,PRUNE_BATCH])?;
            if count == 0 {
                tx.commit()?;
                break;
            }
            if has_deliveries {
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
            }
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

    pub(crate) fn init(path: &Path) -> crate::Result<Connection> {
        let mut conn = connect(path)?;
        if check_version(&conn)? != DATABASE_VERSION {
            initialize(&mut conn, Initialization::FreshStore)?;
        }
        Ok(conn)
    }
}

impl Sink for SqliteSink {
    fn write_record(&mut self, env: &Envelope) -> crate::Result<super::WriteReceipt> {
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
            return Ok(super::WriteReceipt {
                record_id: Some(id.into()),
                delivery: Default::default(),
            });
        }
        let delivery = crate::outbox::enqueue(&tx, env, &self.targets, crate::now_epoch())
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        tx.commit().map_err(|e| crate::Error::Io(e.to_string()))?;
        Ok(super::WriteReceipt {
            record_id: env.metadata.record_id.clone(),
            delivery,
        })
    }
}
