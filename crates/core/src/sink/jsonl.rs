//! Default sink: one append-only JSONL file per Kind, rotated at 10 MB. Each record is
//! a single `write` of one full line through an `O_APPEND` descriptor — that is what lets
//! concurrent hook subprocesses interleave cleanly at the line level (the kernel
//! serializes same-file appends per write call). Building the whole line first and
//! writing it once is therefore a correctness invariant, not a style choice; records are
//! allow-list-bounded and far below any size at which a single append could split.

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
/// over an unchanged set is a consistent snapshot. Retries are bounded; sustained churn (a
/// retention sweep unlinking many archives, back-to-back rotations) degrades to a best-effort
/// snapshot with a stderr note — never a silently empty result.
pub fn read_records(dir: &Path, kind: &str) -> Vec<Envelope> {
    for _ in 0..4 {
        if let Some(records) = read_pass(dir, kind) {
            return records;
        }
    }
    eprintln!(
        "hatel: ledger read for {kind:?} kept racing concurrent rotation/pruning — \
         returning a best-effort snapshot"
    );
    read_best_effort(dir, kind)
}

/// The degraded read: whatever exists right now, skipping anything that vanishes mid-read.
/// Records can be missed under churn (the caller has already said so on stderr), but present
/// data is never discarded — the failure mode is an undercount, not an empty report.
fn read_best_effort(dir: &Path, kind: &str) -> Vec<Envelope> {
    let mut out = Vec::new();
    for path in matching_files(dir, kind).unwrap_or_default() {
        if let Ok(text) = fs::read_to_string(&path) {
            out.extend(text.lines().filter_map(Envelope::from_json_line));
        }
    }
    out
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

/// Delete rotated archives whose last write predates `cutoff_epoch` — the JSONL half of the
/// retention sweep. The active `<kind>.jsonl` is never touched, so only whole archives go, and
/// an archive's mtime is its newest record's write time (rename preserves it) — deleting one
/// removes only records older than the cutoff. Returns files removed. Fail-open: an unreadable
/// entry or a failed remove is skipped, and a concurrent report read that observes the set
/// change retries against a fresh listing (same protocol as rotation).
pub fn prune_archives(dir: &Path, cutoff_epoch: i64) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0; // no ledger dir yet — nothing stored, nothing to prune
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Archives carry a numeric suffix after `.jsonl.` (date stamp, pid, sequence), so they
        // never END with `.jsonl` — while every active ledger does, even for a Kind whose own
        // name contains `.jsonl` (the Kind charset allows dots). Both conditions together make
        // the active file unmatchable.
        if !name.contains(".jsonl.") || name.ends_with(".jsonl") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| (d.as_secs() as i64) < cutoff_epoch);
        if old && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
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
        // One write call for the whole line (newline included) — the line-level atomicity
        // the module relies on under concurrent hook appends.
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
