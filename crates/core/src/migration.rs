//! Explicit import of a quiescent JSONL ledger. Original files are never modified.

use crate::{Envelope, model::RecordMetadata};
use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractSummary {
    pub kind: String,
    pub schema_id: Option<String>,
    pub kind_version: Option<u32>,
    pub records: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct Preflight {
    pub source: PathBuf,
    pub database: PathBuf,
    pub database_version: Option<i64>,
    pub source_files: Option<usize>,
    pub source_bytes: Option<u64>,
    pub records: i64,
    pub contracts: Vec<ContractSummary>,
}

// Diagnostic output has a fixed memory bound even for adversarial contract cardinality.
const MAX_CONTRACTS: usize = 4096;

pub fn check(cfg: &crate::Config) -> crate::Result<Preflight> {
    let database = cfg.state_dir.join("telemetry.db");
    let version = crate::sink::database_version(&database)?;
    if version.is_some_and(|v| v > crate::sink::DATABASE_VERSION) {
        return Err(io("SQLite schema is newer than this build supports"));
    }
    let mut result = Preflight {
        source: if cfg.sink == crate::SinkKind::Jsonl {
            cfg.ledger_dir.clone()
        } else {
            database.clone()
        },
        database,
        database_version: version,
        source_files: None,
        source_bytes: None,
        records: 0,
        contracts: Vec::new(),
    };
    if cfg.sink == crate::SinkKind::Jsonl {
        let (_, mut files) = source_files(&cfg.ledger_dir)?;
        let before = signature(&mut files)?;
        let mut bytes = 0u64;
        let mut contracts = BTreeMap::new();
        for (path, file) in &mut files {
            bytes = bytes
                .checked_add(file.metadata().map_err(io)?.len())
                .ok_or_else(|| io("source byte count overflow"))?;
            for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = line.map_err(io)?;
                if line.trim().is_empty() {
                    continue;
                }
                let (env, _) = parse_record(&line, path, index + 1)?;
                let key = (env.kind, env.metadata.schema_id, env.metadata.kind_version);
                *contracts.entry(key).or_insert(0i64) += 1;
                if contracts.len() > MAX_CONTRACTS {
                    return Err(io(
                        "more than 4096 contracts; inspect a smaller source before migration",
                    ));
                }
                result.records += 1;
            }
        }
        if signature(&mut files)? != before
            || signature(&mut source_files(&cfg.ledger_dir)?.1)? != before
        {
            return Err(io(
                "source changed during preflight; stop writers and retry",
            ));
        }
        result.source_files = Some(files.len());
        result.source_bytes = Some(bytes);
        result.contracts = contracts
            .into_iter()
            .map(
                |((kind, schema_id, kind_version), records)| ContractSummary {
                    kind,
                    schema_id,
                    kind_version,
                    records,
                },
            )
            .collect();
    } else if version.is_some() {
        let conn = rusqlite::Connection::open_with_flags(
            &result.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(io)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(io)?;
        conn.execute_batch("PRAGMA cache_size=-4096; PRAGMA temp_store=FILE; BEGIN;")
            .map_err(io)?;
        let integrity: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(io)?;
        if integrity != "ok" {
            return Err(io(format!("SQLite quick_check: {integrity}")));
        }
        let columns = conn
            .prepare("PRAGMA table_info(records)")
            .map_err(io)?
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(io)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(io)?;
        if columns.is_empty() {
            return Ok(result);
        }
        let metadata = if columns.iter().any(|c| c == "metadata") {
            "metadata"
        } else {
            "'{}'"
        };
        let mut stmt=conn.prepare(&format!("SELECT kind,json_extract({metadata},'$.schema_id'),json_extract({metadata},'$.kind_version'),COUNT(*) FROM records GROUP BY 1,2,3 ORDER BY 1,2,3 LIMIT 4097")).map_err(io)?;
        result.contracts = stmt
            .query_map([], |r| {
                Ok(ContractSummary {
                    kind: r.get(0)?,
                    schema_id: r.get(1)?,
                    kind_version: r.get(2)?,
                    records: r.get(3)?,
                })
            })
            .map_err(io)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(io)?;
        if result.contracts.len() > MAX_CONTRACTS {
            return Err(io(
                "more than 4096 contracts; preflight summary exceeds its limit",
            ));
        }
        result.records = result.contracts.iter().map(|c| c.records).sum();
        let missing = if columns.iter().any(|c| c == "occurred_seconds")
            && columns.iter().any(|c| c == "occurred_nanos")
        {
            "occurred_seconds IS NULL OR occurred_nanos IS NULL"
        } else {
            "1"
        };
        let mut times = conn
            .prepare(&format!("SELECT rowid,ts FROM records WHERE {missing}"))
            .map_err(io)?;
        let rows = times
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(io)?;
        for row in rows {
            let (id, ts) = row.map_err(io)?;
            ts.parse::<jiff::Timestamp>()
                .map_err(|_| io(format!("invalid timestamp in SQLite row {id}")))?;
        }
    }
    Ok(result)
}

fn source_files(source: &Path) -> crate::Result<(PathBuf, Vec<(PathBuf, std::fs::File)>)> {
    let source = match source.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::path::absolute(source).map_err(io)?
        }
        Err(error) => return Err(io(error)),
    };
    let mut files = Vec::new();
    for kind in super::sink::jsonl_kinds(&source).map_err(io)? {
        files.extend(crate::rolling::open_snapshot(&source, &format!("{kind}.jsonl")).map_err(io)?);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((source, files))
}

fn parse_record(
    line: &str,
    path: &Path,
    line_number: usize,
) -> crate::Result<(Envelope, jiff::Timestamp)> {
    let env = Envelope::from_json_line(line).ok_or_else(|| {
        io(format!(
            "{}:{line_number}: invalid or unsupported envelope",
            path.display()
        ))
    })?;
    if !crate::registry::is_valid_kind_name(&env.kind) {
        return Err(io(format!(
            "{}:{line_number}: invalid kind",
            path.display()
        )));
    }
    let occurred = env.ts.parse::<jiff::Timestamp>().map_err(|_| {
        io(format!(
            "{}:{line_number}: invalid timestamp",
            path.display()
        ))
    })?;
    Ok((env, occurred))
}

/// Explicit maintenance of a SQLite store whose writers have stopped.
pub fn upgrade_sqlite(database: &Path) -> crate::Result<usize> {
    crate::sink::upgrade_database(database).map(|(_, count)| count)
}

fn io(e: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(e.to_string())
}

fn signature(files: &mut [(std::path::PathBuf, std::fs::File)]) -> crate::Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 64 * 1024];
    for (path, file) in files {
        let name = path
            .file_name()
            .ok_or_else(|| io("missing source filename"))?
            .to_string_lossy();
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(&file.metadata().map_err(io)?.len().to_le_bytes());
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        loop {
            let n = file.read(&mut buffer).map_err(io)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        file.seek(SeekFrom::Start(0)).map_err(io)?;
    }
    Ok(hash.finalize().to_hex().to_string())
}

pub fn import_jsonl(source: &Path, database: &Path) -> crate::Result<usize> {
    let (source, mut files) = source_files(source)?;
    let source_key = source.to_string_lossy().into_owned();
    let before = signature(&mut files)?;
    let mut conn = super::sink::open_database(database)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS imports (source TEXT PRIMARY KEY, signature TEXT NOT NULL, records INTEGER NOT NULL);").map_err(io)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(io)?;
    use rusqlite::OptionalExtension;
    let existing: Option<String> = tx
        .query_row(
            "SELECT signature FROM imports WHERE source=?1",
            [&source_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)?;
    if let Some(existing) = existing {
        return if existing == before {
            Ok(0)
        } else {
            Err(io(
                "previously imported source changed; stop writers and restore the original source before retrying",
            ))
        };
    }
    let mut count = 0usize;
    {
        let mut insert = tx.prepare("INSERT INTO records (ts,kind,schema_version,payload,metadata,occurred_seconds,occurred_nanos) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT DO NOTHING").map_err(io)?;
        for (path, file) in &mut files {
            for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = line.map_err(io)?;
                if line.trim().is_empty() {
                    continue;
                }
                let (mut env, occurred) = parse_record(&line, path, index + 1)?;
                if env.metadata.record_id.is_none() {
                    env.metadata = RecordMetadata {
                        record_id: Some(uuid::Uuid::new_v4().to_string()),
                        ..env.metadata
                    };
                }
                let inserted = insert
                    .execute(rusqlite::params![
                        env.ts,
                        env.kind,
                        env.schema_version,
                        serde_json::to_string(&env.payload).map_err(io)?,
                        serde_json::to_string(&env.metadata).map_err(io)?,
                        occurred.as_second(),
                        occurred.subsec_nanosecond()
                    ])
                    .map_err(io)?;
                if inserted == 0 {
                    let id = env
                        .metadata
                        .record_id
                        .as_deref()
                        .ok_or_else(|| io("duplicate without record id"))?;
                    let stored = crate::sink::record_by_id(&tx, id)?;
                    if !env.same_record(&stored) {
                        return Err(io("one record id describes different imported data"));
                    }
                }
                count += inserted;
            }
        }
    }
    if signature(&mut files)? != before {
        return Err(io("source changed during import; transaction rolled back"));
    }
    let (_, mut current) = source_files(&source)?;
    if signature(&mut current)? != before {
        return Err(io(
            "source file set changed during import; transaction rolled back",
        ));
    }
    tx.execute(
        "INSERT INTO imports (source,signature,records) VALUES (?1,?2,?3)",
        rusqlite::params![source_key, before, count as i64],
    )
    .map_err(io)?;
    tx.commit().map_err(io)?;
    Ok(count)
}
