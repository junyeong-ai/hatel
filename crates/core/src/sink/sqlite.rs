//! SQLite sink (embedded, WAL) — indexed storage for higher volume or richer
//! queries, indexed by `(kind, ts)` so windowed reads stay cheap as the table grows.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::Sink;
use crate::Envelope;

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
    // The SQL pre-filter must be a safe SUPERSET (the caller applies the exact
    // `ts >= cutoff` window in code). Stored timestamps carry fractional seconds
    // (`...:20.5Z`) while a whole-second cutoff renders without a fraction (`...:20Z`),
    // and the comparison is lexical — `'.'` < `'Z'`, so a fraction-less cutoff would
    // sort AFTER a same-second record and wrongly drop it. Subtracting one second
    // moves the cutoff strictly below every in-window record. An empty cutoff (`since`
    // = None) sorts before any timestamp, so `ts >= ""` matches all.
    let cutoff = since
        .and_then(|s| jiff::Timestamp::from_second(s.saturating_sub(1)).ok())
        .map(|t| t.to_string())
        .unwrap_or_default();
    let mut stmt = match conn
        .prepare("SELECT ts, schema_version, payload FROM records WHERE kind = ?1 AND ts >= ?2")
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hatel: sqlite query prepare failed ({e})");
            return Vec::new();
        }
    };
    let rows = stmt.query_map(rusqlite::params![kind, cutoff], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
        ))
    });
    let mut out = Vec::new();
    match rows {
        Ok(rows) => {
            for (ts, schema_version, payload) in rows.flatten() {
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
                    });
                }
            }
        }
        Err(e) => eprintln!("hatel: sqlite query failed ({e})"),
    }
    out
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
    let Ok(cutoff) = jiff::Timestamp::from_second(cutoff_epoch.saturating_sub(1)) else {
        return 0; // a cutoff outside representable time prunes nothing
    };
    // `rowid IN (… LIMIT n)` rather than `DELETE … LIMIT`, which needs a non-default
    // SQLite compile flag.
    let mut total = 0;
    loop {
        match conn.execute(
            "DELETE FROM records WHERE rowid IN \
             (SELECT rowid FROM records WHERE ts < ?1 LIMIT ?2)",
            rusqlite::params![cutoff.to_string(), PRUNE_BATCH],
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
    conn: Option<Connection>,
}

impl SqliteSink {
    pub fn open(path: PathBuf) -> Self {
        let conn = match Self::init(&path) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("hatel: sqlite open failed: {e}");
                None
            }
        };
        Self { conn }
    }

    fn init(path: &PathBuf) -> rusqlite::Result<Connection> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        // Brief writer contention (another hook, the receiver's flush, a prune batch) should be
        // absorbed, not turned into a dropped record — but only briefly: the hook sits on the
        // interaction path, so it must never stall behind a slow writer for long.
        let _ = conn.busy_timeout(std::time::Duration::from_millis(100));
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS records (
                 ts TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 schema_version INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_records_kind_ts ON records(kind, ts);",
        )?;
        Ok(conn)
    }
}

impl Sink for SqliteSink {
    fn write_record(&mut self, env: &Envelope) {
        let Some(conn) = &self.conn else {
            return;
        };
        let payload = serde_json::to_string(&env.payload).unwrap_or_default();
        if let Err(e) = conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![env.ts, env.kind, env.schema_version, payload],
        ) {
            eprintln!("hatel: sqlite insert failed kind={}: {e}", env.kind);
        }
    }
}
