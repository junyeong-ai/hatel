//! Immutable registered definitions kept beside SQLite records, including retired Kinds.

use crate::{Config, KindSpec, SinkKind, registry::KindSpecRaw};
use rusqlite::{Connection, OpenFlags, Transaction};
use serde::Serialize;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS contracts (
    schema_id TEXT PRIMARY KEY, kind TEXT NOT NULL, definition TEXT NOT NULL
); CREATE INDEX IF NOT EXISTS idx_contracts_kind ON contracts(kind);";

fn io(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(error.to_string())
}

pub(crate) fn save(tx: &Transaction<'_>, spec: &KindSpec) -> crate::Result<()> {
    let definition = serde_json::to_string(&spec.definition()).map_err(io)?;
    tx.execute("INSERT INTO contracts(schema_id,kind,definition) VALUES (?1,?2,?3) ON CONFLICT(schema_id) DO NOTHING",rusqlite::params![spec.schema_id,spec.name,definition]).map_err(io)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct StoredContract {
    pub schema_id: String,
    pub definition: KindSpecRaw,
}

fn connection(cfg: &Config) -> crate::Result<Option<Connection>> {
    if cfg.sink != SinkKind::Sqlite {
        return Ok(None);
    }
    let path = cfg.state_dir.join("telemetry.db");
    if !path.try_exists().map_err(io)? {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(io)?;
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='contracts')",
            [],
            |r| r.get(0),
        )
        .map_err(io)?;
    Ok(exists.then_some(conn))
}

pub fn stored(cfg: &Config) -> crate::Result<Vec<StoredContract>> {
    let Some(conn) = connection(cfg)? else {
        return Ok(Vec::new());
    };
    let mut stmt = conn
        .prepare("SELECT schema_id,kind,definition FROM contracts ORDER BY kind,schema_id")
        .map_err(io)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(io)?;
    rows.map(|row| {
        let (schema_id, kind, json) = row.map_err(io)?;
        let definition: KindSpecRaw = serde_json::from_str(&json).map_err(io)?;
        let spec = KindSpec::from_raw(definition.clone())?;
        if spec.schema_id != schema_id || spec.name != kind {
            return Err(io("stored contract definition does not match its identity"));
        }
        Ok(StoredContract {
            schema_id,
            definition,
        })
    })
    .collect()
}

pub fn select(cfg: &Config, kind: &str, schema_id: &str) -> crate::Result<KindSpec> {
    let stored = stored(cfg)?.into_iter().find(|entry| entry.schema_id==schema_id && entry.definition.name==kind)
        .ok_or_else(||io(format!("no stored definition for {kind} contract {schema_id}; load its exact schema explicitly")))?;
    KindSpec::from_raw(stored.definition)
}
