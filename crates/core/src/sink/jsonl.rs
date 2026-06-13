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
