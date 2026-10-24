//! Durable delivery state committed with local records. No network work runs in writers.

use crate::{Envelope, ExportTarget};
use rusqlite::{Connection, Transaction};
use serde::Serialize;

pub const MAX_PENDING_RECORDS: i64 = 100_000;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS delivery_queues (
    destination TEXT PRIMARY KEY, pending_records INTEGER NOT NULL DEFAULT 0,
    pending_bytes INTEGER NOT NULL DEFAULT 0
); CREATE TABLE IF NOT EXISTS deliveries (
    record_id TEXT NOT NULL, destination TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','delivered','failed','expired','cancelled','capacity')),
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
    size_bytes INTEGER NOT NULL, last_error TEXT,
    PRIMARY KEY(record_id,destination)
); CREATE INDEX IF NOT EXISTS idx_deliveries_due ON deliveries(destination,status,next_attempt_at);";

#[derive(Debug, Clone, Default, Serialize)]
pub struct Enqueued {
    pub queued: usize,
    pub not_queued: usize,
}

pub(crate) fn enqueue(
    tx: &Transaction<'_>,
    env: &Envelope,
    targets: &[ExportTarget],
    now: i64,
) -> rusqlite::Result<Enqueued> {
    let mut result = Enqueued::default();
    let Some(id) = &env.metadata.record_id else {
        return Ok(result);
    };
    let size = env.to_json_line().len() as i64;
    for target in targets.iter().filter(|t| t.allows_record(env)) {
        tx.execute(
            "INSERT OR IGNORE INTO delivery_queues(destination) VALUES (?1)",
            [&target.endpoint],
        )?;
        let queued = tx.execute("UPDATE delivery_queues SET pending_records=pending_records+1,pending_bytes=pending_bytes+?1 WHERE destination=?2 AND pending_records<?3 AND pending_bytes+?1<=?4", rusqlite::params![size,target.endpoint,MAX_PENDING_RECORDS,target.queue_bytes as i64])?;
        let full = queued == 0;
        let status = if full {
            result.not_queued += 1;
            "capacity"
        } else {
            result.queued += 1;
            "pending"
        };
        tx.execute("INSERT INTO deliveries(record_id,destination,status,created_at,updated_at,next_attempt_at,size_bytes,last_error) VALUES (?1,?2,?3,?4,?4,?4,?5,?6)", rusqlite::params![id,target.endpoint,status,now,size,full.then_some("pending queue capacity exceeded")])?;
    }
    Ok(result)
}

#[derive(Debug)]
pub struct Pending {
    pub record: Envelope,
    pub attempts: u32,
    pub created_at: i64,
}

pub fn destinations(conn: &Connection) -> crate::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT destination FROM deliveries WHERE status='pending' ORDER BY destination").map_err(io)?;
    stmt.query_map([], |r| r.get(0))
        .map_err(io)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(io)
}

pub fn due(conn: &Connection, destination: &str, now: i64) -> crate::Result<Vec<Pending>> {
    let mut stmt = conn.prepare("SELECT r.ts,r.kind,r.schema_version,r.payload,r.metadata,d.attempts,d.created_at FROM deliveries d JOIN records r ON json_extract(r.metadata,'$.record_id')=d.record_id WHERE d.destination=?1 AND d.status='pending' AND d.next_attempt_at<=?2 ORDER BY d.created_at,d.record_id LIMIT 16").map_err(io)?;
    let rows = stmt
        .query_map(rusqlite::params![destination, now], |r| {
            let payload: String = r.get(3)?;
            let metadata: String = r.get(4)?;
            let parse_error = |e| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            };
            Ok(Pending {
                record: Envelope {
                    ts: r.get(0)?,
                    kind: r.get(1)?,
                    schema_version: r.get(2)?,
                    payload: serde_json::from_str(&payload).map_err(parse_error)?,
                    metadata: serde_json::from_str(&metadata).map_err(parse_error)?,
                },
                attempts: r.get(5)?,
                created_at: r.get(6)?,
            })
        })
        .map_err(io)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(io)
}

pub enum Outcome<'a> {
    Delivered,
    Retry {
        next_attempt_at: i64,
        error: &'a str,
    },
    Failed(&'a str),
    FailedBeforeSend(&'a str),
    Expired,
    Cancelled(&'a str),
}

pub fn finish(
    conn: &Connection,
    destination: &str,
    id: &str,
    now: i64,
    outcome: Outcome<'_>,
) -> crate::Result<()> {
    let (status, next, attempted, error) = match outcome {
        Outcome::Delivered => ("delivered", now, 1, None),
        Outcome::Retry {
            next_attempt_at,
            error,
        } => ("pending", next_attempt_at, 1, Some(error)),
        Outcome::Failed(error) => ("failed", now, 1, Some(error)),
        Outcome::FailedBeforeSend(error) => ("failed", now, 0, Some(error)),
        Outcome::Expired => ("expired", now, 0, Some("maximum queue age exceeded")),
        Outcome::Cancelled(error) => ("cancelled", now, 0, Some(error)),
    };
    use rusqlite::OptionalExtension;
    let tx = conn.unchecked_transaction().map_err(io)?;
    let size: Option<i64> = tx.query_row("UPDATE deliveries SET status=?1,updated_at=?2,next_attempt_at=?3,attempts=attempts+?4,last_error=?5 WHERE record_id=?6 AND destination=?7 AND status='pending' RETURNING size_bytes", rusqlite::params![status,now,next,attempted,error,id,destination], |r| r.get(0)).optional().map_err(io)?;
    if status != "pending"
        && let Some(size) = size
    {
        tx.execute("UPDATE delivery_queues SET pending_records=pending_records-1,pending_bytes=pending_bytes-?1 WHERE destination=?2", rusqlite::params![size,destination]).map_err(io)?;
    }
    tx.commit().map_err(io)?;
    Ok(())
}

pub fn cancel_destination(conn: &Connection, destination: &str, now: i64) -> crate::Result<()> {
    let tx = conn.unchecked_transaction().map_err(io)?;
    tx.execute("UPDATE deliveries SET status='cancelled',updated_at=?1,last_error='destination removed or custom delivery disabled' WHERE destination=?2 AND status='pending'", rusqlite::params![now,destination]).map_err(io)?;
    tx.execute(
        "UPDATE delivery_queues SET pending_records=0,pending_bytes=0 WHERE destination=?1",
        [destination],
    )
    .map_err(io)?;
    tx.commit().map_err(io)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct DeliveryState {
    pub destination: String,
    pub status: String,
    pub error: Option<String>,
    pub records: i64,
    pub bytes: i64,
    pub oldest_created_at: i64,
}

pub fn states(path: &std::path::Path) -> crate::Result<Vec<DeliveryState>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(io)?;
    let mut stmt = conn.prepare("SELECT destination,status,COUNT(*),SUM(size_bytes),MIN(created_at),last_error FROM deliveries GROUP BY destination,status,last_error ORDER BY destination,status,last_error").map_err(io)?;
    stmt.query_map([], |r| {
        Ok(DeliveryState {
            destination: r.get(0)?,
            status: r.get(1)?,
            error: r.get(5)?,
            records: r.get(2)?,
            bytes: r.get(3)?,
            oldest_created_at: r.get(4)?,
        })
    })
    .map_err(io)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(io)
}

fn io(e: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(e.to_string())
}
