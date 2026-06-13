//! A rolling append-only text log: one active file plus size-rotated archives in a directory.
//!
//! Appends are one `write_all` of a full line through an `O_APPEND` descriptor — the kernel
//! serializes same-file appends per write call, so concurrent processes interleave cleanly at the
//! line level. Records are small and bounded (a `~150`-byte index line; an allow-list-bounded ledger
//! record), so the write completes in a single syscall on a regular file — `PIPE_BUF` bounds atomic
//! appends to PIPES, not to regular files, where a small write is not split. The only way to leave a
//! partial line is a short write under disk-full/`EINTR`, which the reader drops as unparseable: an
//! honest undercount surfaced to stderr, never a crash or a fabricated value. Reads cover the active
//! file and every archive, retrying against a
//! fresh listing when a concurrent rotation or prune changes the matching set, so a rotation never
//! drops a line from a read. Archives are pruned by mtime. Both the per-Kind ledger and the session
//! index are built on this one primitive.
//!
//! A base name is the active file's full name (e.g. `tool.jsonl` or `session_index.jsonl`); an
//! archive is that name with a `.YYYYMMDD.<pid>[.N]` suffix. The active file is matched by exact
//! name and an archive by that precise suffix, so the two are never confused — no name a Kind can
//! produce collides with the archive form.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

/// The archive suffix exactly as rotation writes it: a date stamp (`YYYYMMDD`), the rotating
/// process's pid, and any collision sequence — each a `.`-separated number, so at least two numeric
/// segments. Every version that has ever rotated wrote `<base>.<stamp>.<pid>[.N]` (the pid was never
/// omitted), so requiring it orphans no real on-disk archive while still excluding a resident that
/// merely ends in a bare `.YYYYMMDD`. Anchored at the end, so it never matches an active file (which
/// ends in its non-numeric extension).
static ARCHIVE_SUFFIX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\.\d{8}\.\d+(?:\.\d+)*$").unwrap());

fn is_archive_name(name: &str) -> bool {
    ARCHIVE_SUFFIX.is_match(name)
}

/// Append `line` (a newline is added) to `<dir>/<base>`, rotating the active file to an archive
/// first when it has reached `rotate_bytes`. Rotation is best-effort: a failure — including a peer
/// process rotating first — never aborts, and thus never drops, the line being written.
pub fn append(dir: &Path, base: &str, line: &str, rotate_bytes: u64) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(base);
    let _ = rotate_if_needed(&path, base, rotate_bytes);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    file.write_all(buf.as_bytes())
}

/// Every parsed record from `<base>` and its archives. Cross-file order is deterministic but
/// carries no meaning — no consumer depends on it (the ledger aggregates; the session index folds
/// last-wins by a per-line timestamp), so a record landing in an archive vs the active file, or a
/// late cross-rotation append, never changes a result. `parse` is applied to each non-blank line
/// directly from the borrowed file slice — no owned `String` per line on the read path. A pass that
/// observes the matching-file set change mid-read —
/// a concurrent rotation created an archive it didn't list — is retried against a fresh listing;
/// since each line lives in exactly one file at any instant, a pass over an unchanged set is a
/// consistent snapshot. Retries are bounded; sustained churn degrades to a best-effort snapshot (a
/// possible undercount, never a silently empty result) with a stderr note.
pub fn read_parsed<R>(dir: &Path, base: &str, parse: impl Fn(&str) -> Option<R>) -> Vec<R> {
    for _ in 0..4 {
        if let Some(rows) = read_pass(dir, base, &parse) {
            return rows;
        }
    }
    eprintln!(
        "hatel: rolling-log read for {base:?} kept racing concurrent rotation/pruning — \
         returning a best-effort snapshot"
    );
    read_best_effort(dir, base, &parse)
}

/// The degraded read: whatever exists right now, skipping anything that vanishes mid-read. Records
/// can be missed under churn (the caller has already said so on stderr), but present data is never
/// discarded — the failure mode is an undercount, not an empty result.
fn read_best_effort<R>(dir: &Path, base: &str, parse: &impl Fn(&str) -> Option<R>) -> Vec<R> {
    let mut out = Vec::new();
    for path in matching_files(dir, base).unwrap_or_default() {
        if let Ok(text) = fs::read_to_string(&path) {
            out.extend(parse_lines(&text, parse));
        }
    }
    out
}

/// One read pass. `None` signals the caller to retry against a fresh listing: either a matching
/// file vanished mid-read (rotated/pruned away), or the matching-file set changed between the start
/// and end of the pass.
fn read_pass<R>(dir: &Path, base: &str, parse: &impl Fn(&str) -> Option<R>) -> Option<Vec<R>> {
    let Some(before) = matching_files(dir, base) else {
        return Some(Vec::new()); // dir absent → genuinely no lines, not a race
    };
    let mut out = Vec::new();
    for path in &before {
        match fs::read_to_string(path) {
            Ok(text) => out.extend(parse_lines(&text, parse)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None, // rotated mid-read
            Err(e) => {
                // Not the rotation race (that is NotFound, handled above) — a genuine read error
                // on a file we just listed. Skip it so one bad file can't empty a whole report,
                // but surface it: silently dropping a ledger's worth of records would undercount
                // a report or attribution pass with no trace.
                eprintln!("hatel: skipping unreadable {}: {e}", path.display());
            }
        }
    }
    match matching_files(dir, base) {
        Some(after) if after == before => Some(out),
        _ => None,
    }
}

/// Apply `parse` to each non-blank line, reading directly from the borrowed slice.
fn parse_lines<'a, R>(
    text: &'a str,
    parse: &'a impl Fn(&str) -> Option<R>,
) -> impl Iterator<Item = R> + 'a {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse)
}

/// The active file and every archive of `base`, sorted by name. The order is used ONLY to make the
/// read-race set comparison stable (filenames don't change once written), never as a semantic
/// ordering — consumers are order-independent (aggregate, or fold last-wins by timestamp), so the
/// non-monotone pid in an archive name and a late cross-rotation append are both immaterial. `None`
/// if the directory can't be listed.
fn matching_files(dir: &Path, base: &str) -> Option<Vec<PathBuf>> {
    let archive_prefix = format!("{base}.");
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == base || (n.starts_with(&archive_prefix) && is_archive_name(&n))
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    Some(files)
}

/// A cheap change signature over `base`'s files — `(file count, total bytes, newest mtime)` — for a
/// reader that caches the folded contents and only re-reads when this changes. Total bytes strictly
/// increases on append and shifts on rotation/prune, so it catches a change the 1-second mtime
/// granularity could miss. `None` when the directory can't be listed.
pub fn fingerprint(dir: &Path, base: &str) -> Option<(usize, u64, Option<SystemTime>)> {
    let files = matching_files(dir, base)?;
    let mut total = 0u64;
    let mut newest: Option<SystemTime> = None;
    for path in &files {
        if let Ok(meta) = fs::metadata(path) {
            total += meta.len();
            if let Ok(m) = meta.modified() {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
        }
    }
    Some((files.len(), total, newest))
}

/// Delete archives of *any* base in `dir` older than `cutoff_epoch` — for the ledger, which owns its
/// directory and holds one rolling log per Kind, so a removed plugin's orphaned archives are swept
/// too. The active file is never touched.
pub fn prune_archives(dir: &Path, cutoff_epoch: i64) -> usize {
    prune_matching(dir, cutoff_epoch, is_archive_name)
}

/// Delete archives of `base` specifically — for a rolling log that shares its directory with other
/// files (the session index alongside the cost snapshot and the db in the state dir). Symmetric with
/// the base-scoped read, so the prune can only ever touch this log's own archives.
pub fn prune_archives_of(dir: &Path, base: &str, cutoff_epoch: i64) -> usize {
    let prefix = format!("{base}.");
    prune_matching(dir, cutoff_epoch, |name| {
        name.starts_with(&prefix) && is_archive_name(name)
    })
}

/// Delete every archive `matches` selects whose last write predates the cutoff. An archive's mtime is
/// its newest line's write time (rename preserves it), so deleting one removes only lines older than
/// the cutoff. Returns archives removed. Fail-open: an unreadable entry or a failed remove is skipped.
fn prune_matching(dir: &Path, cutoff_epoch: i64, matches: impl Fn(&str) -> bool) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0; // no dir yet — nothing stored, nothing to prune
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !matches(&name.to_string_lossy()) {
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

/// Rotate an oversized active file to `<base>.YYYYMMDD.<pid>[.N]`. Including the pid makes
/// concurrent rotations by different processes pick distinct targets, so a rename can never clobber
/// another's archive; a `NotFound` means a peer already rotated, which is fine — the active file is
/// recreated on the next open.
fn rotate_if_needed(path: &Path, base: &str, threshold: u64) -> std::io::Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() < threshold {
        return Ok(());
    }
    let stamp = date_stamp();
    let pid = std::process::id();
    let mut target = path.with_file_name(format!("{base}.{stamp}.{pid}"));
    let mut n = 1;
    while target.exists() {
        target = path.with_file_name(format!("{base}.{stamp}.{pid}.{n}"));
        n += 1;
    }
    match fs::rename(path, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// `YYYYMMDD` from the RFC-3339 timestamp's date portion (no extra datetime-formatting dependency,
/// no timezone database touched).
fn date_stamp() -> String {
    crate::now_iso_utc()
        .chars()
        .take(10)
        .filter(|c| *c != '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ht-rolling-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read raw lines back (identity parse) — the shape the rotation/prune assertions check.
    fn lines(dir: &Path, base: &str) -> Vec<String> {
        read_parsed(dir, base, |l| Some(l.to_string()))
    }

    #[test]
    fn append_then_read_round_trips_in_order() {
        let dir = scratch();
        append(&dir, "log.jsonl", "a", 1 << 20).unwrap();
        append(&dir, "log.jsonl", "b", 1 << 20).unwrap();
        assert_eq!(lines(&dir, "log.jsonl"), vec!["a", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_preserves_all_lines() {
        let dir = scratch();
        // The first append creates the file; the second, with a tiny threshold, sees it
        // over-threshold, archives it, and writes `new` to a fresh active file. Both lines survive
        // the rotation — read order across files is unspecified, so compare as a set.
        append(&dir, "log.jsonl", "old", 1 << 20).unwrap();
        append(&dir, "log.jsonl", "new", 1).unwrap();
        let mut got = lines(&dir, "log.jsonl");
        got.sort();
        assert_eq!(got, vec!["new", "old"]);
        let archives = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| is_archive_name(&e.file_name().to_string_lossy()))
            .count();
        assert_eq!(archives, 1, "exactly one archive after one rotation");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_active_file_is_never_matched_as_an_archive() {
        // Even a base whose own name contains digits and dots must not look like an archive.
        assert!(!is_archive_name("session_index.jsonl"));
        assert!(!is_archive_name("v2.jsonl"));
        assert!(!is_archive_name("cost_snapshot.jsonl.12345.6.tmp")); // a temp file, not an archive
        assert!(
            !is_archive_name("tool.jsonl.20240101"),
            "a bare date with no pid is not the rotation format"
        );
        assert!(is_archive_name("session_index.jsonl.20260613.1071"));
        assert!(is_archive_name("tool.jsonl.20260613.1071.2"));
    }

    #[test]
    fn prune_removes_old_archives_only() {
        let dir = scratch();
        // Create the file, then two tiny-threshold appends each roll the active file into an
        // archive — two archives, the newest line in the active file.
        append(&dir, "log.jsonl", "a", 1 << 20).unwrap();
        append(&dir, "log.jsonl", "b", 1).unwrap();
        append(&dir, "log.jsonl", "c", 1).unwrap();
        let mut before = lines(&dir, "log.jsonl");
        before.sort();
        assert_eq!(before, vec!["a", "b", "c"]);
        let removed = prune_archives(&dir, i64::MAX); // cutoff in the far future → all archives old
        assert_eq!(removed, 2, "both archives pruned, active kept");
        assert_eq!(lines(&dir, "log.jsonl"), vec!["c"], "active file survives");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fingerprint_changes_on_append() {
        let dir = scratch();
        append(&dir, "log.jsonl", "a", 1 << 20).unwrap();
        let f1 = fingerprint(&dir, "log.jsonl").unwrap();
        append(&dir, "log.jsonl", "bb", 1 << 20).unwrap();
        let f2 = fingerprint(&dir, "log.jsonl").unwrap();
        assert_ne!(f1.1, f2.1, "total bytes grew, so the fingerprint changed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
