//! Immutable registered definitions kept beside SQLite records, including retired Kinds, and the
//! resolution of each stored contract against the definition a reader answers under.

use crate::{Config, Envelope, KindSpec, Registry, registry::KindSpecRaw};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS kinds (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS contracts (
    id INTEGER PRIMARY KEY, schema_id TEXT NOT NULL UNIQUE,
    kind_id INTEGER NOT NULL REFERENCES kinds (id), definition TEXT NOT NULL
);";

fn io(error: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(error.to_string())
}

/// The id of the Kind named `name`, registered on first use.
pub(crate) fn kind_id(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    let stored = conn
        .query_row("SELECT id FROM kinds WHERE name = ?1", [name], |r| r.get(0))
        .optional()?;
    match stored {
        Some(id) => Ok(id),
        None => conn.query_row(
            "INSERT INTO kinds (name) VALUES (?1) RETURNING id",
            [name],
            |r| r.get(0),
        ),
    }
}

/// The id of the contract `env` is stamped with, stored with its definition on first use. A record
/// is kept only beside the definition of its contract, so one stamped with a contract that is
/// neither stored nor the registry's current definition of its Kind is refused.
pub(crate) fn contract_id(
    conn: &Connection,
    registry: &Registry,
    env: &Envelope,
    kind_id: i64,
) -> crate::Result<Option<i64>> {
    let Some(schema_id) = env.metadata.schema_id.as_deref() else {
        return Ok(None);
    };
    let stored = conn
        .query_row(
            "SELECT id FROM contracts WHERE schema_id = ?1",
            [schema_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)?;
    if stored.is_some() {
        return Ok(stored);
    }
    let spec = registry
        .kind(&env.kind)
        .filter(|spec| spec.schema_id == schema_id)
        .ok_or_else(|| {
            crate::Error::InvalidPayload(format!(
                "{} contract {schema_id} has no definition to store beside the record",
                env.kind
            ))
        })?;
    let definition = serde_json::to_string(&spec.definition()).map_err(io)?;
    conn.query_row(
        "INSERT INTO contracts (schema_id, kind_id, definition) VALUES (?1, ?2, ?3) RETURNING id",
        rusqlite::params![schema_id, kind_id, definition],
        |r| r.get(0),
    )
    .map(Some)
    .map_err(io)
}

#[derive(Debug, Serialize)]
pub struct StoredContract {
    pub schema_id: String,
    pub definition: KindSpecRaw,
}

pub fn stored(cfg: &Config) -> crate::Result<Vec<StoredContract>> {
    let conn = crate::sink::read(&cfg.database())?;
    let mut stmt = conn
        .prepare(
            "SELECT c.schema_id, k.name, c.definition FROM contracts c \
             JOIN kinds k ON k.id = c.kind_id ORDER BY k.name, c.schema_id",
        )
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
    let missing = || {
        io(format!(
            "no stored definition for {kind} contract {schema_id}; load its exact schema explicitly"
        ))
    };
    let conn = crate::sink::read(&cfg.database())?;
    let json: String = conn
        .query_row(
            "SELECT c.definition FROM contracts c JOIN kinds k ON k.id = c.kind_id \
             WHERE c.schema_id = ?1 AND k.name = ?2",
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

/// The contract identifiers one definition reads, and of those the ones whose records it may
/// export, resolved from every definition known for its Kind: itself and those stored beside the
/// records.
#[derive(Debug, Clone)]
pub struct Contracts {
    selected: String,
    readable: BTreeSet<String>,
    exportable: BTreeSet<String>,
    known: BTreeSet<String>,
}

impl Contracts {
    fn new(spec: &KindSpec, stored: impl IntoIterator<Item = KindSpec>) -> Self {
        let mut contracts = Self {
            selected: spec.schema_id.clone(),
            readable: BTreeSet::new(),
            exportable: BTreeSet::new(),
            known: BTreeSet::new(),
        };
        for writer in std::iter::once(spec.clone()).chain(stored) {
            if spec.reads(&writer) {
                contracts.readable.insert(writer.schema_id.clone());
            }
            if spec.exports(&writer) {
                contracts.exportable.insert(writer.schema_id.clone());
            }
            contracts.known.insert(writer.schema_id);
        }
        contracts
    }

    /// Resolve `spec` against the definitions stored in an open ledger.
    pub(crate) fn resolve(conn: &Connection, spec: &KindSpec) -> crate::Result<Self> {
        let mut stmt = conn
            .prepare(
                "SELECT c.schema_id, c.definition FROM contracts c \
                 JOIN kinds k ON k.id = c.kind_id WHERE k.name = ?1",
            )
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

    pub fn exports(&self, schema_id: Option<&str>) -> bool {
        schema_id.is_some_and(|id| self.exportable.contains(id))
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
