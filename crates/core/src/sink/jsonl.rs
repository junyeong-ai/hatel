//! Default sink: one append-only JSONL file per Kind in the ledger directory, rotated by size and
//! by age. The rolling-log mechanics — rotation, race-safe read across active + archives, and
//! archive pruning — live in [`crate::rolling`]; this is the JSONL-specific layer: the
//! `<kind>.jsonl` base name and Envelope (de)serialization.

use std::path::{Path, PathBuf};

use super::Sink;
use crate::config::Retention;
use crate::{Envelope, rolling};

fn base(kind: &str) -> String {
    format!("{kind}.jsonl")
}

/// Read every record for `kind` — the active ledger plus its rotated archives, so a rotation never
/// drops records from a report. The read half of the storage abstraction.
pub fn read_records(dir: &Path, kind: &str) -> Vec<Envelope> {
    rolling::read_parsed(dir, &base(kind), Envelope::from_json_line)
}

/// The oldest record of `kind` — of `project` when named — still stored, as its timestamp: how
/// far back the store reaches.
pub fn oldest_ts(dir: &Path, kind: &str, project: Option<&str>) -> Option<String> {
    rolling::first_parsed(dir, &base(kind), |line| {
        Envelope::from_json_line(line).filter(|env| {
            project.is_none_or(|p| env.payload.get("project").and_then(|v| v.as_str()) == Some(p))
        })
    })
    .into_iter()
    .map(|env| env.ts)
    .min()
}

/// Apply `retention` to the ledger — the JSONL half of the retention sweep. Files go whole: every
/// archive whose last write has expired, a removed plugin's orphans included, and then each active
/// ledger holding a record from before `retention.rotate_before` becomes an archive, to expire in
/// turn. Returns files removed.
pub fn prune(dir: &Path, retention: Retention) -> usize {
    let removed = rolling::prune_archives(dir, retention.cutoff);
    for kind in stored_kinds(dir).unwrap_or_default() {
        rolling::rotate_aged(dir, &base(&kind), retention.rotate_before, |line| {
            Envelope::from_json_line(line).and_then(|env| crate::ts_epoch(&env.ts))
        });
    }
    removed
}

/// Every Kind with records in `dir`, recovered from the file names this sink writes. `base`
/// appends one fixed extension to a Kind name, so the text before that extension is the Kind —
/// for the active ledger and equally for an archive (`<kind>.jsonl.<stamp>.<pid>`), which the
/// read path also reads and which can outlive its active file after a prune. The Kind-name
/// character set rejects anything else that lands in the directory.
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
            let kind = match name.strip_suffix(".jsonl") {
                Some(kind) => kind,
                // Rotation owns the archive spelling, so it decides what is one and whose it is —
                // a temp file that merely contains the extension is not.
                None => rolling::archive_base(&name)?.strip_suffix(".jsonl")?,
            };
            crate::registry::is_valid_kind_name(kind).then(|| kind.to_string())
        })
        .collect();
    kinds.sort();
    kinds.dedup();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_covers_exactly_what_the_read_path_reads() {
        // The reader takes the active ledger and its archives, so enumeration must see a Kind
        // whose active file a prune has already removed — and must not invent one from a
        // neighbouring file.
        let dir = std::env::temp_dir().join(format!("ht-enum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "tool.jsonl",
            "tool.jsonl.20260804.1",
            "global.rules.jsonl.20260804.1",
            "cost_snapshot.jsonl.1.2.tmp",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert_eq!(
            stored_kinds(&dir).unwrap(),
            vec!["global.rules".to_string(), "tool".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_archive_belongs_to_the_one_kind_its_name_ends_with() {
        // A Kind may be named `foo.jsonl`. Its archive then begins with `foo.jsonl.`, as every
        // archive of the Kind `foo` does, and must still be read and listed as `foo.jsonl`'s alone.
        let dir = std::env::temp_dir().join(format!("ht-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let record = |kind: &str| {
            format!("{{\"ts\":\"2026-09-26T00:00:00Z\",\"kind\":\"{kind}\",\"payload\":{{}}}}\n")
        };
        std::fs::write(dir.join("foo.jsonl"), record("foo")).unwrap();
        std::fs::write(dir.join("foo.jsonl.jsonl.20260926.1"), record("foo.jsonl")).unwrap();
        assert_eq!(
            stored_kinds(&dir).unwrap(),
            vec!["foo".to_string(), "foo.jsonl".to_string()]
        );
        let kinds_read = |kind: &str| -> Vec<String> {
            read_records(&dir, kind)
                .into_iter()
                .map(|env| env.kind)
                .collect()
        };
        assert_eq!(kinds_read("foo"), vec!["foo"]);
        assert_eq!(kinds_read("foo.jsonl"), vec!["foo.jsonl"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_that_was_never_written_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("ht-enum-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(stored_kinds(&dir).unwrap(), Vec::<String>::new());
    }
}
