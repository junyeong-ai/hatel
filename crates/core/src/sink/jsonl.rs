//! Default sink: one append-only JSONL file per Kind in the ledger directory, size-rotated. The
//! rolling-log mechanics — rotation, race-safe read across active + archives, and archive pruning —
//! live in [`crate::rolling`]; this is the JSONL-specific layer: the `<kind>.jsonl` base name and
//! Envelope (de)serialization.

use std::path::{Path, PathBuf};

use super::Sink;
use crate::{Envelope, rolling};

fn base(kind: &str) -> String {
    format!("{kind}.jsonl")
}

/// Read every record for `kind` — the active ledger plus its rotated archives, so a rotation never
/// drops records from a report. The read half of the storage abstraction.
pub fn read_records(dir: &Path, kind: &str) -> Vec<Envelope> {
    rolling::read_parsed(dir, &base(kind), Envelope::from_json_line)
}

/// Delete rotated ledger archives whose last write predates `cutoff_epoch` — the JSONL half of the
/// retention sweep. Whole archives only; the active `<kind>.jsonl` is never touched. Returns files
/// removed.
pub fn prune_archives(dir: &Path, cutoff_epoch: i64) -> usize {
    rolling::prune_archives(dir, cutoff_epoch)
}

/// Every Kind with a ledger in `dir`, recovered from the file names this sink writes. `base`
/// appends one fixed extension to a Kind name, so stripping it is the exact inverse: an archive
/// (`<kind>.jsonl.<stamp>.<pid>`) does not end in the extension and so cannot be mistaken for a
/// ledger, and the Kind-name character set rejects anything else that lands in the directory.
pub fn stored_kinds(dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // No ledger directory means nothing has ever been written through this sink, which is an
        // empty store rather than an unreadable one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut kinds: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = name.strip_suffix(".jsonl")?;
            crate::registry::is_valid_kind_name(kind).then(|| kind.to_string())
        })
        .collect();
    kinds.sort();
    Ok(kinds)
}

pub struct JsonlSink {
    dir: PathBuf,
    rotate_bytes: u64,
}

impl JsonlSink {
    pub fn new(dir: PathBuf, rotate_bytes: u64) -> Self {
        Self { dir, rotate_bytes }
    }
}

impl Sink for JsonlSink {
    fn write_record(&mut self, env: &Envelope) {
        if let Err(e) = rolling::append(
            &self.dir,
            &base(&env.kind),
            &env.to_json_line(),
            self.rotate_bytes,
        ) {
            eprintln!("hatel: jsonl write failed kind={}: {e}", env.kind);
        }
    }
}
