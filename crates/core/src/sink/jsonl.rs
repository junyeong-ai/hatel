//! Default sink: one append-only JSONL file per Kind, rotated at 10 MB. Writes use
//! `O_APPEND` so concurrent hook subprocesses interleave cleanly at the line level.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::Sink;
use crate::Envelope;

/// Read every record for `kind` — the active ledger plus its rotated archives, so a
/// rotation never drops records from a report. The read half of the storage abstraction.
///
/// A rotation can change the set of matching files between the directory listing and the
/// reads — the active file is renamed to a fresh archive (not in the original listing) and
/// a new active file is created. A single pass could then miss the just-rotated archive. So
/// a pass that observes the matching-file set change (re-listed after reading) is retried
/// against a fresh listing. Since each line lives in exactly one file at any instant, a pass
/// over an unchanged set is a consistent snapshot. Bounded retries; rotation is finite.
pub fn read_records(dir: &Path, kind: &str) -> Vec<Envelope> {
    for _ in 0..4 {
        if let Some(records) = read_pass(dir, kind) {
            return records;
        }
    }
    read_pass(dir, kind).unwrap_or_default()
}

/// Returns the set of files matching `kind` (active + archives), sorted, or `None` if the
/// directory can't be listed.
fn matching_files(dir: &Path, kind: &str) -> Option<Vec<PathBuf>> {
    let active = format!("{kind}.jsonl");
    let archive_prefix = format!("{kind}.jsonl.");
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == active || n.starts_with(&archive_prefix)
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    Some(files)
}

/// One read pass. Returns `None` if a concurrent rotation was observed — either a matching
/// file vanished mid-read, or the matching-file set changed between the start and end of the
/// pass — signalling the caller to retry against a fresh listing.
fn read_pass(dir: &Path, kind: &str) -> Option<Vec<Envelope>> {
    let Some(before) = matching_files(dir, kind) else {
        return Some(Vec::new()); // dir absent → genuinely no records, not a race
    };
    let mut out = Vec::new();
    for path in &before {
        match fs::read_to_string(path) {
            Ok(text) => out.extend(text.lines().filter_map(Envelope::from_json_line)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None, // rotated mid-read
            Err(_) => {} // other read error → skip this file, fail-open
        }
    }
    // If the matching set changed while we read, a rotation may have created an archive we
    // didn't see — retry.
    match matching_files(dir, kind) {
        Some(after) if after == before => Some(out),
        _ => None,
    }
}

pub struct JsonlSink {
    dir: PathBuf,
    rotate_bytes: u64,
}

impl JsonlSink {
    pub fn new(dir: PathBuf, rotate_bytes: u64) -> Self {
        Self { dir, rotate_bytes }
    }

    fn try_write(&self, env: &Envelope) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{}.jsonl", env.kind));
        // Rotation is best-effort: a failure (including a concurrent rotation) must
        // never abort — and thus never drop — the record being written.
        let _ = rotate_if_needed(&path, self.rotate_bytes);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut line = env.to_json_line();
        line.push('\n');
        file.write_all(line.as_bytes())
    }
}

impl Sink for JsonlSink {
    fn write_record(&mut self, env: &Envelope) {
        if let Err(e) = self.try_write(env) {
            eprintln!("hatel: jsonl write failed kind={}: {e}", env.kind);
        }
    }
}

/// Rotate an oversized ledger to `<kind>.jsonl.YYYYMMDD.<pid>[.N]`. Including the
/// pid makes concurrent rotations by different hook processes pick distinct targets,
/// so a rename can never clobber another process's archive; a `NotFound` means a
/// peer already rotated, which is fine — the active ledger is recreated on the open.
fn rotate_if_needed(path: &Path, threshold: u64) -> std::io::Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() < threshold {
        return Ok(());
    }
    let stamp = date_stamp();
    let pid = std::process::id();
    let mut target = sibling(path, &format!("jsonl.{stamp}.{pid}"));
    let mut n = 1;
    while target.exists() {
        target = sibling(path, &format!("jsonl.{stamp}.{pid}.{n}"));
        n += 1;
    }
    match fs::rename(path, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn sibling(path: &Path, ext: &str) -> PathBuf {
    path.with_extension(ext)
}

/// `YYYYMMDD` derived from the RFC-3339 timestamp's date portion (no extra
/// datetime-formatting dependency, no timezone database touched).
fn date_stamp() -> String {
    crate::now_iso_utc()
        .chars()
        .take(10)
        .filter(|c| *c != '-')
        .collect()
}
