//! Explicit import of a quiescent JSONL ledger. Original files are never modified.

use crate::{Envelope, model::RecordMetadata};
use std::io::{BufRead, Read};
use std::path::Path;

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
    let source = source.canonicalize().map_err(io)?;
    let source_key = source.to_string_lossy().into_owned();
    let mut files = Vec::new();
    for kind in super::sink::jsonl_kinds(&source).map_err(io)? {
        files.extend(crate::rolling::open_snapshot(&source, &format!("{kind}.jsonl")).map_err(io)?);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
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
                let mut env = Envelope::from_json_line(&line).ok_or_else(|| {
                    io(format!(
                        "{}:{}: invalid or unsupported envelope; import rolled back",
                        path.display(),
                        index + 1
                    ))
                })?;
                if !crate::registry::is_valid_kind_name(&env.kind) {
                    return Err(io(format!(
                        "{}:{}: invalid kind or timestamp",
                        path.display(),
                        index + 1
                    )));
                }
                let occurred = env.ts.parse::<jiff::Timestamp>().map_err(|_| {
                    io(format!(
                        "{}:{}: invalid timestamp",
                        path.display(),
                        index + 1
                    ))
                })?;
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
    let mut current = Vec::new();
    for kind in super::sink::jsonl_kinds(&source).map_err(io)? {
        current
            .extend(crate::rolling::open_snapshot(&source, &format!("{kind}.jsonl")).map_err(io)?);
    }
    current.sort_by(|a, b| a.0.cmp(&b.0));
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
