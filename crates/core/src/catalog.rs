//! Immutable registered definitions kept beside SQLite records, including retired Kinds, and the
//! resolution of each stored contract against the definition a reader answers under.

use crate::{Config, KindSpec, Registry, registry::KindSpecRaw};
use rusqlite::{Connection, OpenFlags, Transaction};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

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
    let path = cfg.database();
    if !path.try_exists().map_err(io)? {
        return Ok(None);
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(Some)
        .map_err(io)
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
        decode(schema_id, kind, json)
    })
    .collect()
}

pub fn select(cfg: &Config, kind: &str, schema_id: &str) -> crate::Result<KindSpec> {
    use rusqlite::OptionalExtension;
    let missing = || {
        io(format!(
            "no stored definition for {kind} contract {schema_id}; load its exact schema explicitly"
        ))
    };
    let conn = connection(cfg)?.ok_or_else(missing)?;
    let json: String = conn
        .query_row(
            "SELECT definition FROM contracts WHERE schema_id=?1 AND kind=?2",
            [schema_id, kind],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)?
        .ok_or_else(missing)?;
    let stored = decode(schema_id.into(), kind.into(), json)?;
    KindSpec::from_raw(stored.definition)
}

/// How records stamped with one contract identifier relate to the definition a reader answers
/// under. Only `selected` and `compatible` records are read; the others are excluded and counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Selected,
    Compatible,
    Incompatible,
    Undefined,
}

/// The contract identifiers one definition reads, resolved from every definition known for its
/// Kind: itself and those stored beside the records.
#[derive(Debug, Clone)]
pub struct Contracts {
    selected: String,
    readable: BTreeSet<String>,
    known: BTreeSet<String>,
}

impl Contracts {
    fn new(spec: &KindSpec, stored: impl IntoIterator<Item = KindSpec>) -> Self {
        let mut contracts = Self {
            selected: spec.schema_id.clone(),
            readable: BTreeSet::new(),
            known: BTreeSet::new(),
        };
        for writer in std::iter::once(spec.clone()).chain(stored) {
            if spec.reads(&writer) {
                contracts.readable.insert(writer.schema_id.clone());
            }
            contracts.known.insert(writer.schema_id);
        }
        contracts
    }

    /// Resolve `spec` against the definitions stored in an open ledger.
    pub(crate) fn resolve(conn: &Connection, spec: &KindSpec) -> crate::Result<Self> {
        let mut stmt = conn
            .prepare("SELECT schema_id,definition FROM contracts WHERE kind=?1")
            .map_err(io)?;
        let stored = stmt
            .query_map([&spec.name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(io)?
            .map(|row| {
                let (schema_id, json) = row.map_err(io)?;
                KindSpec::from_raw(decode(schema_id, spec.name.clone(), json)?.definition)
            })
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(Self::new(spec, stored))
    }

    /// Resolve every registered Kind against the configured store's definitions.
    pub fn for_registry(cfg: &Config, reg: &Registry) -> crate::Result<BTreeMap<String, Self>> {
        let mut stored: BTreeMap<String, Vec<KindSpec>> = BTreeMap::new();
        for contract in self::stored(cfg)? {
            stored
                .entry(contract.definition.name.clone())
                .or_default()
                .push(KindSpec::from_raw(contract.definition)?);
        }
        Ok(reg
            .kinds()
            .map(|spec| {
                let writers = stored.remove(&spec.name).unwrap_or_default();
                (spec.name.clone(), Self::new(spec, writers))
            })
            .collect())
    }

    pub fn resolution(&self, schema_id: Option<&str>) -> Resolution {
        match schema_id {
            Some(id) if id == self.selected => Resolution::Selected,
            Some(id) if self.readable.contains(id) => Resolution::Compatible,
            Some(id) if self.known.contains(id) => Resolution::Incompatible,
            _ => Resolution::Undefined,
        }
    }

    pub fn reads(&self, schema_id: Option<&str>) -> bool {
        schema_id.is_some_and(|id| self.readable.contains(id))
    }

    pub(crate) fn readable(&self) -> &BTreeSet<String> {
        &self.readable
    }
}

fn decode(schema_id: String, kind: String, json: String) -> crate::Result<StoredContract> {
    let definition: KindSpecRaw = serde_json::from_str(&json).map_err(io)?;
    let spec = KindSpec::from_raw(definition.clone())?;
    if spec.schema_id != schema_id || spec.name != kind {
        return Err(io("stored contract definition does not match its identity"));
    }
    Ok(StoredContract {
        schema_id,
        definition,
    })
}
