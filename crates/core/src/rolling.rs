//! A rolling append-only text log: one active file plus the archives rotated out of it, in a
//! directory. An append rotates by size; the retention sweep rotates by age.
//!
//! Appends are one `write_all` of a full line through an `O_APPEND` descriptor — the kernel
//! serializes same-file appends per write call, so concurrent processes interleave cleanly at the
//! line level. Records are small and bounded (a `~150`-byte index line; an allow-list-bounded ledger
//! record), so the write completes in a single syscall on a regular file — `PIPE_BUF` bounds atomic
//! appends to PIPES, not to regular files, where a small write is not split. The only way to leave a
//! partial line is a short write under disk-full/`EINTR`, which the reader drops as unparseable —
//! that line alone, an undercount of one record, never a crash or a fabricated value. Reads cover
//! the active file and every archive, retrying against a fresh listing when a concurrent rotation
//! or prune changes the matching set, so a rotation never drops a line from a read. Only archives are deleted, whole and by mtime, so the sweep also
//! archives an active file once it holds a record past its rotation horizon — a log that stays
//! small or stops being written would otherwise keep its first line forever. Both the per-Kind
//! ledger and the session index are built on this one primitive.
//!
//! A base name is the active file's full name (e.g. `tool.jsonl` or `session_index.jsonl`); an
//! archive is that name with a `.YYYYMMDD.<pid>[.N]` suffix. The active file is matched by exact
//! name and an archive by that precise suffix, so the two are never confused — no name a Kind can
//! produce collides with the archive form.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

/// The archive suffix exactly as rotation writes it: a date stamp (`YYYYMMDD`), the rotating
/// process's pid, and any collision sequence — each a `.`-separated number, so at least two numeric
/// segments. Every version that has ever rotated wrote `<base>.<stamp>.<pid>[.N]` (the pid was never
/// omitted), so requiring it orphans no real on-disk archive while still excluding a resident that
/// merely ends in a bare `.YYYYMMDD`. Anchored at the end, so it never matches an active file (which
/// ends in its non-numeric extension).
static ARCHIVE_SUFFIX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\.\d{8}\.\d+(?:\.\d+)*$").unwrap());

pub(crate) fn is_archive_name(name: &str) -> bool {
    ARCHIVE_SUFFIX.is_match(name)
}

/// The base an archive belongs to: its name without the rotation suffix, or `None` for a name that
/// is not an archive. A base ends in its non-numeric extension, so the suffix never reaches into it
/// and the split is exact — `foo.jsonl.jsonl.<stamp>.<pid>` belongs to `foo.jsonl.jsonl`, although
/// it also begins with `foo.jsonl.` like every archive of `foo.jsonl`.
pub(crate) fn archive_base(name: &str) -> Option<&str> {
    ARCHIVE_SUFFIX.find(name).map(|m| &name[..m.start()])
}

/// Serializes rotation within this process. The pid in an archive name keeps processes apart, but
/// threads of one process share it — the MCP server runs concurrent `emit` calls — and two of them
/// rotating at once would pick the same target, the later rename replacing the earlier archive.
static ROTATION: Mutex<()> = Mutex::new(());

/// Append `line` (a newline is added) to `<dir>/<base>`, rotating the active file to an archive
/// first when it has reached `rotate_bytes`. Rotation is best-effort: a failure — including a peer
/// process rotating first — never aborts, and thus never drops, the line being written.
pub fn append(dir: &Path, base: &str, line: &str, rotate_bytes: u64) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(base);
    let _ = rotate_when(&path, base, |meta| meta.len() >= rotate_bytes);
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
        if let Ok(bytes) = fs::read(&path) {
            out.extend(parse_lines(&bytes, parse));
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
        match fs::read(path) {
            Ok(bytes) => out.extend(parse_lines(&bytes, parse)),
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
    bytes: &'a [u8],
    parse: &'a impl Fn(&str) -> Option<R>,
) -> impl Iterator<Item = R> + 'a {
    text_lines(bytes)
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse)
}

/// Each line of `bytes` that is valid UTF-8, without its line ending. A line cut inside a
/// multibyte character is dropped alone, as any unparseable line is, rather than making the whole
/// file unreadable.
fn text_lines(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|b| *b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter_map(|line| std::str::from_utf8(line).ok())
}

/// The first parsable line of the active file and of every archive of `base`. A file is appended
/// in time order, so its first line is its oldest record and the set is what a reader needs to
/// know how far back the store reaches — without reading the store.
pub fn first_parsed<R>(dir: &Path, base: &str, parse: impl Fn(&str) -> Option<R>) -> Vec<R> {
    matching_files(dir, base)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| first_record(&path, &parse))
        .collect()
}

/// The first line of the file at `path` that `parse` accepts.
fn first_record<R>(path: &Path, parse: impl Fn(&str) -> Option<R>) -> Option<R> {
    let file = fs::File::open(path).ok()?;
    BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .find_map(|line| text_lines(&line).next().and_then(&parse))
}

/// The active file and every archive of `base`, sorted by name. The order is used ONLY to make the
/// read-race set comparison stable (filenames don't change once written), never as a semantic
/// ordering — consumers are order-independent (aggregate, or fold last-wins by timestamp), so the
/// non-monotone pid in an archive name and a late cross-rotation append are both immaterial. `None`
/// if the directory can't be listed.
fn matching_files(dir: &Path, base: &str) -> Option<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == base || archive_base(&n) == Some(base)
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
    prune_matching(dir, cutoff_epoch, |name| archive_base(name) == Some(base))
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
            .ok()
            .and_then(|meta| mtime_epoch(&meta))
            .is_some_and(|t| t < cutoff_epoch);
        if old && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Archive the active file of `base` once it holds a record from before `cutoff_epoch`. `epoch`
/// reads a line's timestamp. The file's last write bounds every line in it and its first dated line
/// is its oldest record (appends arrive in time order), so either one past the cutoff settles it.
///
/// Call it after the same sweep's prune, never before it. The archive keeps the mtime of its last
/// write, so a prune that followed could delete it while an append that opened the file just
/// before the rename is still writing into it.
pub fn rotate_aged(dir: &Path, base: &str, cutoff_epoch: i64, epoch: impl Fn(&str) -> Option<i64>) {
    let path = dir.join(base);
    let aged = |meta: &fs::Metadata| {
        let before = |t: i64| t < cutoff_epoch;
        mtime_epoch(meta).is_some_and(before) || first_record(&path, &epoch).is_some_and(before)
    };
    if let Err(e) = rotate_when(&path, base, aged) {
        eprintln!("hatel: cannot archive {}: {e}", path.display());
    }
}

/// The last-write time as epoch seconds.
fn mtime_epoch(meta: &fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_secs()).ok()
}

/// Rotate the active file at `path` to `<base>.YYYYMMDD.<pid>[.N]` when `due` holds for it. The pid
/// gives each process its own targets, and [`ROTATION`] holds one process's threads apart across
/// the check and the rename, so a thread arriving second judges the fresh active file rather than
/// the one already moved. A rename therefore never replaces another rotation's archive, and the
/// `exists()` bump for the `.N` suffix only separates this process's sequential same-day rotations
/// (or a dead process's leftovers after pid reuse). A `NotFound` means a peer process rotated
/// first, which is fine — the active file is recreated on the next open.
fn rotate_when(
    path: &Path,
    base: &str,
    due: impl FnOnce(&fs::Metadata) -> bool,
) -> std::io::Result<()> {
    let _serial = ROTATION.lock().unwrap_or_else(PoisonError::into_inner);
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if !due(&meta) {
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

    /// A line that is an integer is dated at that epoch second; any other line is undated.
    fn epoch(line: &str) -> Option<i64> {
        line.parse().ok()
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
    fn concurrent_rotations_in_one_process_lose_no_line() {
        // Threads of one process share the pid in the archive name, so two rotating at once pick
        // the same target unless rotation is serialized, and the later rename replaces the earlier
        // archive. A one-byte threshold makes every append after the first rotate.
        let dir = scratch();
        std::thread::scope(|s| {
            for t in 0..4 {
                let dir = &dir;
                s.spawn(move || {
                    for i in 0..100 {
                        append(dir, "log.jsonl", &format!("{t}-{i}"), 1).unwrap();
                    }
                });
            }
        });
        assert_eq!(lines(&dir, "log.jsonl").len(), 400);
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
    fn the_sweep_archives_an_active_file_by_its_oldest_dated_line() {
        // Written just now, so only the first dated line can place the file; an undated line ahead
        // of it — one from before a timestamp existed, or a torn write — is passed over.
        let dir = scratch();
        for line in ["undated", "100", "300"] {
            append(&dir, "log.jsonl", line, 1 << 20).unwrap();
        }
        rotate_aged(&dir, "log.jsonl", 100, epoch);
        assert!(dir.join("log.jsonl").exists(), "no line from before 100");
        rotate_aged(&dir, "log.jsonl", 200, epoch);
        assert!(
            !dir.join("log.jsonl").exists(),
            "the line at 100 is before 200"
        );
        let mut got = lines(&dir, "log.jsonl");
        got.sort();
        assert_eq!(got, vec!["100", "300", "undated"], "rotation drops nothing");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_sweep_archives_an_active_file_last_written_before_the_cutoff() {
        // No line is dated, so only the last write can place the file, and it bounds every line.
        let dir = scratch();
        append(&dir, "log.jsonl", "undated", 1 << 20).unwrap();
        rotate_aged(&dir, "log.jsonl", crate::now_epoch() - 60, epoch);
        assert!(dir.join("log.jsonl").exists(), "written within the minute");
        rotate_aged(&dir, "log.jsonl", crate::now_epoch() + 60, epoch);
        assert!(!dir.join("log.jsonl").exists());
        assert_eq!(lines(&dir, "log.jsonl"), vec!["undated"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_line_that_is_not_utf8_hides_only_itself() {
        // A short write can cut a record inside a multibyte character, and the next append then
        // follows it on the same line or the next.
        let dir = scratch();
        std::fs::write(dir.join("log.jsonl"), b"\xed\x95100\n150\n\xed\x95\n300\n").unwrap();
        assert_eq!(lines(&dir, "log.jsonl"), vec!["150", "300"]);
        rotate_aged(&dir, "log.jsonl", 200, epoch);
        assert!(
            !dir.join("log.jsonl").exists(),
            "the first readable dated line is 150"
        );
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
