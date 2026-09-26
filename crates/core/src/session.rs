//! The session index — the generic `session_id → project` join, sink-independent and append-only.
//! The receiver needs it to attribute project-less OTel datapoints to a project regardless of the
//! configured sink. One line per session start, and one per renewal of a session the receiver still
//! hears from; the reader folds them, the newest start winning, so concurrent writers never race on
//! a read-modify-write. It is a [`crate::rolling`] log, so the retention sweep rotates and prunes it
//! like any ledger — bounding the one store that lives outside a Kind's ledger.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::Retention;
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
    /// A copy the receiver wrote to keep a live session's attribution past the files holding its
    /// start, not a session start. A start outranks any renewal in the fold, so a renewal that raced
    /// a newer start — the session resumed in another repository — cannot undo it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    renewal: bool,
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
        let row = SessionRow {
            project_key: project.map_or_else(String::new, |p| p.key.clone()),
            project_label: project.map_or_else(String::new, |p| p.label.clone()),
        };
        self.append(session_id, &row, false, rotate_bytes);
    }

    /// Carry forward the attribution of each session in `heard` — its id and when the receiver last
    /// heard from it — that was heard from more than `span` after its newest line was written. Only
    /// a session start writes a line otherwise, so a session running long past its start would lose
    /// its project to the sweep while still sending telemetry. With this, an attribution outlasts
    /// the session's last activity by at least the horizon less one span. A session the index does
    /// not hold is skipped: renewal carries an attribution forward and never makes one up.
    ///
    /// A renewal never rotates the file by size, so it cannot hand the prune that follows it a file
    /// a hook is still writing into; the next hook append rotates it instead.
    pub fn renew<'a>(&self, heard: impl IntoIterator<Item = (&'a str, i64)>, span: i64) {
        let latest = latest(rolling::read_parsed(
            &self.state_dir,
            INDEX_BASE,
            parse_index_line,
        ));
        for (session_id, last_heard) in heard {
            if let Some(entry) = latest.get(session_id)
                && entry
                    .written
                    .is_none_or(|t| last_heard > t.as_second() + span)
            {
                self.append(session_id, &entry.row, true, u64::MAX);
            }
        }
    }

    fn append(&self, session_id: &str, row: &SessionRow, renewal: bool, rotate_bytes: u64) {
        let line = IndexLine {
            session_id: session_id.to_string(),
            project_key: row.project_key.clone(),
            project_label: row.project_label.clone(),
            ts: crate::now_iso_utc(),
            renewal,
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

    /// Apply `retention` to the index — its half of the retention sweep, on the ledger's terms:
    /// archives go whole once their newest line has expired, and then an active file holding a line
    /// from before `retention.rotate_before` becomes an archive. Rotation moves lines without
    /// dropping any, so a session stays attributable until its newest line expires — which
    /// [`Self::renew`], run first, keeps from happening to a session still being heard from.
    /// Returns archives removed.
    pub fn prune(&self, retention: Retention) -> usize {
        let removed = rolling::prune_archives_of(&self.state_dir, INDEX_BASE, retention.cutoff);
        rolling::rotate_aged(
            &self.state_dir,
            INDEX_BASE,
            retention.rotate_before,
            |line| parse_index_line(line).and_then(|l| crate::ts_epoch(&l.ts)),
        );
        removed
    }

    /// The newest write time across the index (active file + archives), or `None` when nothing has
    /// been recorded yet — so a caller can tell whether sessions have started recently without
    /// reaching into the index's storage layout or missing a stretch where the active file is
    /// absent, as it is after a rotation until the next line is written.
    pub fn newest_mtime(&self) -> Option<SystemTime> {
        rolling::fingerprint(&self.state_dir, INDEX_BASE).and_then(|(_, _, mtime)| mtime)
    }
}

/// Parse one index line, dropping a malformed one (fail-open on read).
fn parse_index_line(line: &str) -> Option<IndexLine> {
    serde_json::from_str(line).ok()
}

/// Fold index lines into one row per session — see [`latest`].
fn fold(lines: Vec<IndexLine>) -> BTreeMap<String, SessionRow> {
    latest(lines)
        .into_iter()
        .map(|(sid, entry)| (sid, entry.row))
        .collect()
}

/// What the index holds for one session.
struct Entry {
    /// The attribution in force.
    row: SessionRow,
    /// Which line `row` came from: a start outranks a renewal, then the later instant wins.
    rank: (bool, Option<jiff::Timestamp>),
    /// The newest line of either kind — when the attribution was last written down.
    written: Option<jiff::Timestamp>,
}

/// Each session's attribution: its newest start's, or once no start line remains, its newest
/// renewal's. Instants are each line's `ts` PARSED — not file/read order, and not string comparison
/// (jiff prints variable precision, so `…:05Z` would sort after `…:05.000001Z` lexically). An empty
/// or unparseable `ts` is `None`, which orders below any real instant, so a pre-`ts` line loses to
/// any dated record. The fold is thus independent of how archives are ordered or interleaved — a
/// re-recorded session resolves to its most recent project wherever its lines landed.
fn latest(lines: Vec<IndexLine>) -> BTreeMap<String, Entry> {
    let mut best: BTreeMap<String, Entry> = BTreeMap::new();
    for il in lines {
        let ts = il.ts.parse::<jiff::Timestamp>().ok();
        let rank = (!il.renewal, ts);
        let row = SessionRow {
            project_key: il.project_key,
            project_label: il.project_label,
        };
        match best.get_mut(&il.session_id) {
            Some(entry) => {
                entry.written = entry.written.max(ts);
                if rank >= entry.rank {
                    entry.rank = rank;
                    entry.row = row;
                }
            }
            None => {
                best.insert(
                    il.session_id,
                    Entry {
                        row,
                        rank,
                        written: ts,
                    },
                );
            }
        }
    }
    best
}

/// A change-gated cache of the folded session index: it re-folds only when the index files actually
/// change — an append or a prune; a rotation only renames them — so a hot read path (the receiver's
/// live render, each flush, the export forwarder) pays a directory stat rather than re-parsing the
/// whole, and ever-growing, index on every call.
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
        // A far-future cutoff makes every archive old: archives go, then the active file (S2)
        // is archived rather than deleted.
        let everything = Retention {
            cutoff: i64::MAX,
            rotate_before: i64::MAX,
        };
        assert!(idx.prune(everything) >= 1, "at least one archive pruned");
        let after = idx.load();
        assert!(
            after.contains_key("S2"),
            "the active session survives pruning"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_index_reaching_back_past_the_rotation_horizon_is_archived_and_still_attributes() {
        // A line from before `ts` existed carries no date, so the first dated line places the file.
        let dir = scratch();
        let path = dir.join(INDEX_BASE);
        let legacy = "{\"session_id\":\"S0\",\"project_key\":\"/k/a\",\"project_label\":\"a\"}\n";
        let dated = "{\"session_id\":\"S1\",\"project_key\":\"/k/b\",\"project_label\":\"b\",\"ts\":\"2026-06-01T00:00:00Z\"}\n";
        std::fs::write(&path, format!("{legacy}{dated}")).unwrap();
        let first = crate::ts_epoch("2026-06-01T00:00:00Z").unwrap();
        let idx = SessionIndex::new(dir.clone());
        let at = |rotate_before| Retention {
            cutoff: i64::MIN,
            rotate_before,
        };
        idx.prune(at(first));
        assert!(
            path.exists(),
            "the undated line is not taken for an older one"
        );
        idx.prune(at(first + 1));
        assert!(
            !path.exists(),
            "archived once its first dated start is past"
        );
        let map = idx.load();
        assert_eq!(map.get("S0").unwrap().project_label, "a");
        assert_eq!(map.get("S1").unwrap().project_label, "b");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renewal_carries_a_live_sessions_project_past_its_expired_start() {
        // S1 (a project) and S2 (none) started a hundred days ago and are still live; S4 started as
        // long ago and went quiet; S3 started today; `ghost` was never recorded.
        let dir = scratch();
        let now = crate::now_epoch();
        let line = |sid: &str, project: &str, days: i64| {
            let ts = jiff::Timestamp::from_second(now - days * 86_400).unwrap();
            let key = if project.is_empty() {
                String::new()
            } else {
                format!("/k/{project}")
            };
            format!(
                "{{\"session_id\":\"{sid}\",\"project_key\":\"{key}\",\"project_label\":\"{project}\",\"ts\":\"{ts}\"}}\n"
            )
        };
        let archive = dir.join(format!("{INDEX_BASE}.20260101.1"));
        std::fs::write(
            &archive,
            [
                line("S1", "a", 100),
                line("S2", "", 100),
                line("S4", "d", 100),
            ]
            .concat(),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(100 * 86_400))
            .unwrap();
        std::fs::write(dir.join(INDEX_BASE), line("S3", "c", 0)).unwrap();
        let idx = SessionIndex::new(dir.clone());
        let retention = Retention {
            cutoff: now - 90 * 86_400,
            rotate_before: now - 9 * 86_400,
        };
        let heard = ["S1", "S2", "S3", "ghost"].map(|sid| (sid, now));
        idx.renew(heard, 9 * 86_400);
        idx.prune(retention);
        assert!(!archive.exists(), "the expired starts are gone");
        let map = idx.load();
        assert_eq!(
            map["S1"].project_label, "a",
            "a live session keeps its project"
        );
        assert!(
            map["S2"].project_key.is_empty(),
            "a live session without one stays recorded as having none"
        );
        assert_eq!(map["S3"].project_label, "c");
        assert!(!map.contains_key("S4"), "a quiet session expires");
        assert!(
            !map.contains_key("ghost"),
            "renewal makes no attribution up"
        );
        let active = std::fs::read_to_string(dir.join(INDEX_BASE)).unwrap();
        assert_eq!(
            active.lines().count(),
            3,
            "S3's recent start needs no renewal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_renewal_never_outranks_a_start() {
        // The receiver renewed S1 in project a from what it had read, while the session resumed in
        // project b and that start landed first: the later renewal must not undo it. Once no start
        // is left, the newest renewal carries the attribution.
        let dir = scratch();
        let path = dir.join(INDEX_BASE);
        let line = |project: &str, day: u8, renewal: bool| {
            let flag = if renewal { ",\"renewal\":true" } else { "" };
            format!(
                "{{\"session_id\":\"S1\",\"project_key\":\"/k/{project}\",\"project_label\":\"{project}\",\"ts\":\"2026-06-0{day}T00:00:00Z\"{flag}}}\n"
            )
        };
        let idx = SessionIndex::new(dir.clone());
        std::fs::write(
            &path,
            [line("a", 1, false), line("b", 2, false), line("a", 3, true)].concat(),
        )
        .unwrap();
        assert_eq!(idx.load()["S1"].project_label, "b");
        std::fs::write(&path, [line("a", 3, true), line("b", 4, true)].concat()).unwrap();
        assert_eq!(idx.load()["S1"].project_label, "b");
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
