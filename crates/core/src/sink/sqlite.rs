//! SQLite sink (embedded, WAL) — indexed storage for higher volume or richer
//! queries, indexed by `(kind, ts)` so windowed reads stay cheap as the table grows.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::Sink;
use crate::Envelope;

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

/// The newest `ts` of `kind`. `MAX` compares the RFC 3339 text, which can misorder records within
/// one second (a fraction sorts before `Z`); no caller needs that precision.
pub fn last_written(path: &Path, kind: &str) -> Option<i64> {
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    crate::query::functions(&conn).ok()?;
    let ts: Option<String> = conn
        .query_row(
            "SELECT ts FROM records WHERE kind = ?1 ORDER BY hatel_epoch(ts) DESC, hatel_nanos(ts) DESC LIMIT 1",
            [kind],
            |row| row.get(0),
        )
        .ok()?;
    crate::ts_epoch(&ts?)
}

/// Read records for `kind` back as envelopes — the read half of the storage
/// abstraction, so a report consumes the SQLite sink exactly as it does JSONL. The
/// time window is pushed into SQL (`ts >= since`) so the indexed backend isn't forced
/// to scan its whole history. A missing DB reads empty (no telemetry yet); a DB that
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
    if let Err(e) = crate::query::functions(&conn) {
        eprintln!("hatel: sqlite read: {e}");
        return Vec::new();
    }
    let has_metadata = conn
        .prepare("PRAGMA table_info(records)")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .is_ok_and(|cols| cols.iter().any(|c| c == "metadata"));
    let metadata = if has_metadata { "metadata" } else { "'{}'" };
    let mut stmt = match conn.prepare(&format!("SELECT ts, schema_version, payload, {metadata} FROM records WHERE kind = ?1 AND (?2 IS NULL OR hatel_epoch(ts)>=?2)")) {
        Ok(stmt) => stmt,
        Err(e) => { eprintln!("hatel: sqlite query: {e}"); return Vec::new(); }
    };
    let rows = stmt.query_map(rusqlite::params![kind, since], |row| {
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
    crate::query::functions(&conn).ok()?;
    conn.query_row("SELECT ts FROM records WHERE kind=?1 AND (?2 IS NULL OR json_extract(payload, '$.project')=?2) ORDER BY hatel_epoch(ts),hatel_nanos(ts) LIMIT 1", rusqlite::params![kind,project], |r| r.get(0)).ok()
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
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hatel: sqlite prune open failed ({e})");
            return 0;
        }
    };
    // The sweep is the patient side: it waits out other writers rather than failing busy.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    if crate::query::functions(&conn).is_err() {
        return 0;
    }
    // `rowid IN (… LIMIT n)` rather than `DELETE … LIMIT`, which needs a non-default
    // SQLite compile flag.
    let mut total = 0;
    loop {
        match conn.execute(
            "DELETE FROM records WHERE rowid IN \
             (SELECT rowid FROM records WHERE hatel_epoch(ts) < ?1 LIMIT ?2)",
            rusqlite::params![cutoff_epoch, PRUNE_BATCH],
        ) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => {
                eprintln!("hatel: sqlite prune failed ({e})");
                break;
            }
        }
    }
    total
}

pub struct SqliteSink {
    conn: Result<Connection, String>,
}

impl SqliteSink {
    pub fn open(path: PathBuf) -> Self {
        Self {
            conn: Self::init(&path).map_err(|e| e.to_string()),
        }
    }

    pub(crate) fn init(path: &PathBuf) -> rusqlite::Result<Connection> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(100))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS records (
            ts TEXT NOT NULL, kind TEXT NOT NULL, schema_version INTEGER NOT NULL, payload TEXT NOT NULL
        ); CREATE INDEX IF NOT EXISTS idx_records_kind_ts ON records(kind, ts);")?;
        let columns = {
            let mut stmt = tx.prepare("PRAGMA table_info(records)")?;
            stmt.query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !columns.iter().any(|c| c == "metadata") {
            tx.execute_batch(
                "ALTER TABLE records ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';",
            )?;
        }
        if !columns.iter().any(|c| c == "occurred_seconds") {
            tx.execute_batch("ALTER TABLE records ADD COLUMN occurred_seconds INTEGER;")?;
        }
        tx.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_records_id ON records(json_extract(metadata, '$.record_id')) WHERE json_extract(metadata, '$.record_id') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_records_kind_occurred ON records(kind, occurred_seconds);")?;
        tx.commit()?;
        crate::query::functions(&conn)?;
        Ok(conn)
    }
}

impl Sink for SqliteSink {
    fn write_record(&mut self, env: &Envelope) -> crate::Result<super::WriteReceipt> {
        let conn = self
            .conn
            .as_ref()
            .map_err(|e| crate::Error::Io(e.clone()))?;
        let payload =
            serde_json::to_string(&env.payload).map_err(|e| crate::Error::Io(e.to_string()))?;
        conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload, metadata, occurred_seconds) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![env.ts, env.kind, env.schema_version, payload, serde_json::to_string(&env.metadata).map_err(|e| crate::Error::Io(e.to_string()))?, crate::ts_epoch(&env.ts)],
        ).map_err(|e| crate::Error::Io(e.to_string()))?;
        Ok(super::WriteReceipt {
            record_id: env.metadata.record_id.clone(),
        })
    }
}
