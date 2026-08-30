//! The session index — the generic `session_id → project` join, sink-independent and append-only.
//! The receiver needs it to attribute project-less OTel datapoints to a project regardless of the
//! configured sink. One line per session start; the reader folds last-wins, so concurrent hooks
//! never race on a read-modify-write. It is a [`crate::rolling`] log, so it rotates and its archives
//! are pruned on the retention sweep like any ledger — bounding the one store that lives outside a
//! Kind's ledger.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::project::ProjectRef;
use crate::rolling;

/// The active index file's name; archives carry the rolling `.YYYYMMDD.<pid>` suffix.
const INDEX_BASE: &str = "session_index.jsonl";

#[derive(Debug, Serialize, Deserialize)]
struct IndexLine {
    session_id: String,
    project_key: String,
    project_label: String,
    /// Write time (RFC-3339 UTC), so the fold picks the latest record for a session by parsed
    /// instant rather than by file or read order. A line from before this field existed defaults to
    /// empty, which parses to no instant and so loses to any dated record.
    #[serde(default)]
    ts: String,
}

/// A session's project attribution (keyed by session id in the loaded map).
#[derive(Debug, Clone, Default)]
pub struct SessionRow {
    pub project_key: String,
    pub project_label: String,
}

pub struct SessionIndex {
    state_dir: PathBuf,
}

impl SessionIndex {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Append one line for a session start: its project, or its absence. Called once per session,
    /// so there is no read-modify-write and no race between concurrent hook processes. A session
    /// with no project is recorded too — the index is what separates a session that has none from
    /// one whose start has not been seen, and only the second becomes attributable by waiting.
    pub fn record(&self, session_id: &str, project: Option<&ProjectRef>, rotate_bytes: u64) {
        let line = IndexLine {
            session_id: session_id.to_string(),
            project_key: project.map_or_else(String::new, |p| p.key.clone()),
            project_label: project.map_or_else(String::new, |p| p.label.clone()),
            ts: crate::now_iso_utc(),
        };
        let json = serde_json::to_string(&line).unwrap_or_default();
        if let Err(e) = rolling::append(&self.state_dir, INDEX_BASE, &json, rotate_bytes) {
            eprintln!("hatel: session index append failed: {e}");
        }
    }

    /// Fold the log (active + archives) into one row per session, last writer wins.
    pub fn load(&self) -> BTreeMap<String, SessionRow> {
        fold(rolling::read_parsed(
            &self.state_dir,
            INDEX_BASE,
            parse_index_line,
        ))
    }

    /// Drop index archives older than `cutoff_epoch` — the session index's half of the retention
    /// sweep. The active file is never touched (a still-recent session must stay attributable);
    /// only whole archives go, and an archive's mtime is its newest line's write time. A session
    /// past the retention horizon has no records left to attribute anyway. Returns archives removed.
    pub fn prune(&self, cutoff_epoch: i64) -> usize {
        rolling::prune_archives_of(&self.state_dir, INDEX_BASE, cutoff_epoch)
    }

    /// The newest write time across the index (active file + archives), or `None` when nothing has
    /// been recorded yet — so a caller can tell whether sessions have started recently without
    /// reaching into the index's storage layout or missing the brief post-rotation window where the
    /// active file is momentarily absent.
    pub fn newest_mtime(&self) -> Option<SystemTime> {
        rolling::fingerprint(&self.state_dir, INDEX_BASE).and_then(|(_, _, mtime)| mtime)
    }
}

/// Parse one index line, dropping a malformed one (fail-open on read).
fn parse_index_line(line: &str) -> Option<IndexLine> {
    serde_json::from_str(line).ok()
}

/// Fold index lines into one row per session, the latest write winning. The winner is decided by
/// each line's `ts` PARSED to an instant — not by file/read order, and not by string comparison
/// (jiff prints variable precision, so `…:05Z` would sort after `…:05.000001Z` lexically). An empty
/// or unparseable `ts` is `None`, which orders below any real instant, so a pre-`ts` line loses to
/// any dated record. The fold is thus independent of how archives are ordered or interleaved — a
/// re-recorded session resolves to its most recent project wherever its lines landed.
fn fold(lines: Vec<IndexLine>) -> BTreeMap<String, SessionRow> {
    let mut best: BTreeMap<String, (Option<jiff::Timestamp>, SessionRow)> = BTreeMap::new();
    for il in lines {
        let ts = il.ts.parse::<jiff::Timestamp>().ok();
        let newer = best.get(&il.session_id).is_none_or(|(prev, _)| ts >= *prev);
        if newer {
            let row = SessionRow {
                project_key: il.project_key,
                project_label: il.project_label,
            };
            best.insert(il.session_id, (ts, row));
        }
    }
    best.into_iter().map(|(sid, (_, row))| (sid, row)).collect()
}

/// A change-gated cache of the folded session index: it re-folds only when the index files actually
/// change — an append, a rotation, or a prune — so a hot read path (the receiver's live render,
/// each flush, the export forwarder) pays a directory stat rather than re-parsing the whole, and
/// ever-growing, index on every call.
pub struct SessionIndexCache {
    state_dir: PathBuf,
    fingerprint: Option<(usize, u64, Option<SystemTime>)>,
    /// Sessions carrying a project — the only ones that can attribute anything.
    map: BTreeMap<String, SessionRow>,
    /// Sessions recorded without one, kept apart so no accessor can hand out an empty label as
    /// though it were a project, while a caller waiting on attribution can still tell a decided
    /// session from one whose start has not been seen.
    unattributed: BTreeSet<String>,
}

impl SessionIndexCache {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            fingerprint: None,
            map: BTreeMap::new(),
            unattributed: BTreeSet::new(),
        }
    }

    /// Reload the folded index only when its fingerprint advanced. A fold yielding no rows while the
    /// files hold bytes is treated as a transient read race: the prior state AND the prior
    /// fingerprint are both kept, so the next refresh re-reads rather than caching the gap. An
    /// index removed (state reset) folds to an empty set and is adopted, dropping stale rows.
    pub fn refresh(&mut self) {
        let fp = rolling::fingerprint(&self.state_dir, INDEX_BASE);
        match fp {
            None => return, // dir momentarily unlistable — keep what we have
            Some(f) if Some(f) == self.fingerprint => return, // unchanged since last load
            Some(_) => {}
        }
        let had_bytes = matches!(fp, Some((_, bytes, _)) if bytes > 0);
        let rows = fold(rolling::read_parsed(
            &self.state_dir,
            INDEX_BASE,
            parse_index_line,
        ));
        // Adopt the new revision — and only then advance the fingerprint — when the fold produced
        // rows, or the index is genuinely empty. Otherwise keep the prior state and the prior
        // fingerprint so the next refresh retries instead of stranding stale attribution.
        if rows.is_empty() && had_bytes {
            return;
        }
        let (labelled, unattributed): (BTreeMap<_, _>, BTreeMap<_, _>) = rows
            .into_iter()
            .partition(|(_, r)| !r.project_label.is_empty());
        self.map = labelled;
        self.unattributed = unattributed.into_keys().collect();
        self.fingerprint = fp;
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionRow> {
        self.map.get(session_id)
    }

    /// Whether the index has decided a session — it carries a project, or was recorded as having
    /// none. The readiness probe parked egress bodies use: only an undecided session is worth
    /// waiting for.
    pub fn contains(&self, session_id: &str) -> bool {
        self.map.contains_key(session_id) || self.unattributed.contains(session_id)
    }

    /// Whether a session was recorded with no project, so no amount of waiting makes it
    /// attributable.
    pub fn is_unattributed(&self, session_id: &str) -> bool {
        self.unattributed.contains(session_id)
    }

    /// The project label for a session — what enrichment injects — or `None` when unknown (never
    /// fabricated).
    pub fn label(&self, session_id: &str) -> Option<String> {
        self.map.get(session_id).map(|r| r.project_label.clone())
    }

    /// The `(label, key)` for a session, for the egress filter decision. The key (absolute path) is
    /// used only to decide forward/skip and is never egressed.
    pub fn project(&self, session_id: &str) -> Option<(&str, &str)> {
        self.map
            .get(session_id)
            .map(|r| (r.project_label.as_str(), r.project_key.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ht-sessidx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pref(label: &str) -> ProjectRef {
        ProjectRef {
            key: format!("/k/{label}"),
            label: label.to_string(),
        }
    }

    fn set_mtime(path: &std::path::Path, t: SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// One well-formed index line, and the same bytes with its opening brace lost — a torn write,
    /// which leaves the file non-empty while nothing in it parses. Same length by construction, so
    /// the two can be written at one mtime to collide on the cache's fingerprint.
    const GOOD_LINE: &str =
        "{\"session_id\":\"S2\",\"project_key\":\"/k/\",\"project_label\":\"x\"}\n";

    fn torn(line: &str) -> String {
        line.replacen('{', " ", 1)
    }

    #[test]
    fn a_session_recorded_without_a_project_is_decided_but_not_attributable() {
        let dir = scratch();
        let idx = SessionIndex::new(dir.clone());
        idx.record("S1", Some(&pref("alpha")), 1 << 20);
        idx.record("S2", None, 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();

        assert_eq!(cache.project("S1"), Some(("alpha", "/k/alpha")));
        assert_eq!(
            cache.label("S2"),
            None,
            "no project is never served as a label"
        );
        assert_eq!(cache.project("S2"), None);
        assert!(cache.is_unattributed("S2"));
        assert!(
            cache.contains("S2"),
            "a decided session is not worth waiting for"
        );
        assert!(!cache.contains("ghost"));
        assert!(!cache.is_unattributed("ghost"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_then_load_folds_per_session() {
        let dir = scratch();
        let idx = SessionIndex::new(dir.clone());
        idx.record("S1", Some(&pref("alpha")), 1 << 20);
        idx.record("S2", Some(&pref("beta")), 1 << 20);
        let map = idx.load();
        assert_eq!(map.get("S1").unwrap().project_label, "alpha");
        assert_eq!(map.get("S2").unwrap().project_key, "/k/beta");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fold_keeps_the_latest_record_per_session_regardless_of_order() {
        let dir = scratch();
        let path = dir.join(INDEX_BASE);
        let old = "{\"session_id\":\"S1\",\"project_key\":\"/k/old\",\"project_label\":\"old\",\"ts\":\"2026-01-01T00:00:00Z\"}\n";
        let new = "{\"session_id\":\"S1\",\"project_key\":\"/k/new\",\"project_label\":\"new\",\"ts\":\"2026-06-01T00:00:00Z\"}\n";
        // The later timestamp wins whichever line order the two records appear in — the fold does
        // not depend on file or read order.
        std::fs::write(&path, format!("{old}{new}")).unwrap();
        let label = |dir: &PathBuf| {
            SessionIndex::new(dir.clone())
                .load()
                .get("S1")
                .unwrap()
                .project_label
                .clone()
        };
        assert_eq!(label(&dir), "new");
        std::fs::write(&path, format!("{new}{old}")).unwrap();
        assert_eq!(
            label(&dir),
            "new",
            "order-independent: latest ts still wins"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fold_decides_by_instant_not_string_within_a_second() {
        // jiff prints variable precision: a whole second as `…:05Z`, a sub-second as `…:05.000001Z`.
        // Lexically `Z` (0x5A) > `.` (0x2E), so a naive string compare would pick the earlier
        // whole-second record; comparing parsed instants correctly picks the later sub-second one.
        let dir = scratch();
        let path = dir.join(INDEX_BASE);
        let whole = "{\"session_id\":\"S1\",\"project_key\":\"/k/old\",\"project_label\":\"old\",\"ts\":\"2026-06-01T00:00:05Z\"}\n";
        let frac = "{\"session_id\":\"S1\",\"project_key\":\"/k/new\",\"project_label\":\"new\",\"ts\":\"2026-06-01T00:00:05.000001Z\"}\n";
        std::fs::write(&path, format!("{whole}{frac}")).unwrap();
        assert_eq!(
            SessionIndex::new(dir.clone())
                .load()
                .get("S1")
                .unwrap()
                .project_label,
            "new",
            "the later sub-second instant wins, not the lexically-greater whole-second string"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_and_prune_bound_the_index_without_losing_the_active_session() {
        let dir = scratch();
        let idx = SessionIndex::new(dir.clone());
        // A tiny rotate threshold rolls the active file into an archive before the second append.
        idx.record("S1", Some(&pref("alpha")), 1);
        idx.record("S2", Some(&pref("beta")), 1);
        // Both sessions remain readable across the archive + the active file.
        let map = idx.load();
        assert!(map.contains_key("S1") && map.contains_key("S2"));
        // A far-future cutoff makes every archive old: archives go, the active file (S2) stays.
        assert!(idx.prune(i64::MAX) >= 1, "at least one archive pruned");
        let after = idx.load();
        assert!(
            after.contains_key("S2"),
            "the active session survives pruning"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_serves_known_and_never_fabricates_unknown() {
        let dir = scratch();
        SessionIndex::new(dir.clone()).record("S1", Some(&pref("alpha")), 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();
        assert_eq!(cache.label("S1").as_deref(), Some("alpha"));
        assert_eq!(cache.project("S1"), Some(("alpha", "/k/alpha")));
        assert!(cache.contains("S1"));
        assert_eq!(
            cache.label("ghost"),
            None,
            "an unknown session is never fabricated"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_reloads_when_a_new_session_is_appended() {
        let dir = scratch();
        let idx = SessionIndex::new(dir.clone());
        idx.record("S1", Some(&pref("alpha")), 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();
        assert!(cache.contains("S1") && !cache.contains("S2"));
        idx.record("S2", Some(&pref("beta")), 1 << 20);
        cache.refresh(); // total bytes grew → the fingerprint advanced → re-folded
        assert!(
            cache.contains("S2"),
            "a freshly recorded session is picked up"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_drops_labels_when_the_index_is_reset() {
        let dir = scratch();
        SessionIndex::new(dir.clone()).record("S1", Some(&pref("alpha")), 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();
        assert!(cache.contains("S1"));
        // A state reset removes the index file; the cache must not keep serving the stale label.
        std::fs::remove_file(dir.join(INDEX_BASE)).unwrap();
        cache.refresh();
        assert!(
            !cache.contains("S1"),
            "stale label dropped after the index is removed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_keeps_prior_labels_when_a_nonempty_index_folds_empty() {
        let dir = scratch();
        SessionIndex::new(dir.clone()).record("S1", Some(&pref("alpha")), 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();
        assert_eq!(cache.label("S1").as_deref(), Some("alpha"));
        // Overwrite with a torn line: the file holds bytes but nothing parses, so the fold is
        // empty — a transient read race, and the prior good label is kept.
        std::fs::write(dir.join(INDEX_BASE), torn(GOOD_LINE)).unwrap();
        cache.refresh();
        assert_eq!(
            cache.label("S1").as_deref(),
            Some("alpha"),
            "prior labels kept on an empty fold"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_recovers_after_a_transient_empty_fold_at_a_colliding_fingerprint() {
        let dir = scratch();
        let path = dir.join(INDEX_BASE);
        // A torn line and its intact original share a byte length by construction, so written with
        // an equal mtime they share a fingerprint.
        let good = GOOD_LINE;
        let empty = torn(good);

        // A different good session seeds the cache so it starts non-empty (its own fingerprint).
        SessionIndex::new(dir.clone()).record("S1", Some(&pref("alpha")), 1 << 20);
        let mut cache = SessionIndexCache::new(dir.clone());
        cache.refresh();
        assert!(cache.contains("S1"));

        // A transient empty fold: the prior label is kept and the fingerprint is NOT advanced.
        let t = SystemTime::now();
        std::fs::write(&path, &empty).unwrap();
        set_mtime(&path, t);
        cache.refresh();
        assert!(
            cache.contains("S1"),
            "prior label kept across an empty fold"
        );

        // Good data then lands at a fingerprint that COLLIDES with the empty one (same length, same
        // mtime). Had the empty fold advanced the fingerprint, this would be skipped and stranded;
        // because it did not, the stored fingerprint still differs and the good data is re-read.
        std::fs::write(&path, good).unwrap();
        set_mtime(&path, t);
        cache.refresh();
        assert!(
            cache.contains("S2"),
            "good data after a colliding empty fold is not stranded"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
