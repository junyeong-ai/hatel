//! Native-OTel cost snapshot. Cost/tokens are a *snapshot* of current totals, not an event stream,
//! so they are kept as one row per session — not in the append-only event sink, which would bloat
//! with near-identical rows. The receiver is the one writer (its state lock keeps out a second) and holds
//! the rows in memory ([`Snapshot`]); on disk they are a checkpoint rewritten at each retention
//! sweep plus the rows changed since, so what a flush writes follows what changed rather than how
//! many sessions are retained. `report` reads both, so cost survives offline.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// One dimension bucket's spend: the two budget measures every breakdown carries.
/// A named pair rather than a tuple so the serialized form is self-describing
/// (`{"cost_usd": …, "tokens": …}`), which is what a machine consumer keys on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    pub session_id: String,
    pub project: String,
    pub tokens: i64,
    pub cost_usd: f64,
    pub active_time_s: f64,
    pub lines: i64,
    /// Dimensional breakdowns of the totals above, bucketed by the OTel series
    /// attributes: token counts by token type (`input` / `output` / `cacheRead` /
    /// `cacheCreation` — the cache-hit accounting), and spend by model and by
    /// subagent attribution. A series missing the dimension buckets under
    /// `(unattributed)` — recorded, never guessed. A breakdown sums to its total
    /// only when every contributing record carried the dimension: rows persisted
    /// before a dimension existed deserialize to an empty map (`serde(default)`),
    /// which is the honest reading — the breakdown genuinely was not recorded.
    #[serde(default)]
    pub tokens_by_type: BTreeMap<String, i64>,
    #[serde(default)]
    pub by_model: BTreeMap<String, Spend>,
    #[serde(default)]
    pub by_agent: BTreeMap<String, Spend>,
    pub ts: String,
}

/// Merge one breakdown across a receiver restart, per bucket key: a key in both maps
/// accumulates (delta temporality) or is replaced by the current value (cumulative — the
/// current point already carries its full total); a key only in the pre-restart baseline
/// keeps its last known value under either temporality (its spend really happened and the
/// current run simply has no series for it); a key only in the current run needs no
/// baseline. This is the per-key form of the same per-metric rule the scalar totals use.
pub fn merge_counts(
    base: &BTreeMap<String, i64>,
    current: BTreeMap<String, i64>,
    delta: bool,
) -> BTreeMap<String, i64> {
    let mut out = base.clone();
    for (key, value) in current {
        let slot = out.entry(key).or_insert(0);
        *slot = if delta { *slot + value } else { value };
    }
    out
}

/// `merge_counts` for a `Spend` breakdown. The two measures merge under their own
/// metric's temporality — tokens and cost are distinct OTel metrics, so a session mixing
/// temporalities across them stays correct per component.
pub fn merge_spend(
    base: &BTreeMap<String, Spend>,
    current: BTreeMap<String, Spend>,
    tokens_delta: bool,
    cost_delta: bool,
) -> BTreeMap<String, Spend> {
    let mut out = base.clone();
    for (key, value) in current {
        let slot = out.entry(key).or_default();
        slot.tokens = if tokens_delta {
            slot.tokens + value.tokens
        } else {
            value.tokens
        };
        slot.cost_usd = if cost_delta {
            slot.cost_usd + value.cost_usd
        } else {
            value.cost_usd
        };
    }
    out
}

/// The checkpoint: every retained session's row as of the last retention sweep.
const SNAPSHOT_NAME: &str = "cost_snapshot.jsonl";
/// The rows changed since that checkpoint.
const CHANGES_NAME: &str = "cost_changes.jsonl";

fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SNAPSHOT_NAME)
}

/// Remove temp files a write never renamed, and return how many.
///
/// Call only where a single writer is guaranteed, and before it first writes —
/// the receiver, holding its state lock, at startup. Every temp on disk was then
/// left by a writer that is gone. The retention sweep cannot collect these: it
/// deletes whole archives by age, and a temp is neither.
pub fn sweep_orphan_temps(state_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            [SNAPSHOT_NAME, CHANGES_NAME]
                .iter()
                .any(|file| name.starts_with(&format!("{file}.")))
                && name.ends_with(".tmp")
        })
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

/// Every session's row, for a reader. The changes are read before the snapshot: a checkpoint writes
/// the snapshot before it removes the changes, so a read that races one finds each row in at least
/// one of the two.
pub fn read_snapshot(state_dir: &Path) -> Vec<CostRow> {
    fold(
        read_rows(&state_dir.join(CHANGES_NAME)),
        read_rows(&snapshot_path(state_dir)),
    )
    .into_values()
    .collect()
}

/// The cost snapshot as its one writer, the receiver, holds it: every session's latest row in
/// memory, persisted so that a flush writes what changed rather than every retained session. A
/// checkpoint rewrites the snapshot file and clears the changes; between checkpoints a flush that
/// changed a row rewrites the changes file, which holds only the sessions heard since the last one.
pub struct Snapshot {
    state_dir: PathBuf,
    rows: BTreeMap<String, CostRow>,
    /// The sessions whose rows the changes file holds.
    changed: BTreeSet<String>,
    /// Whether the changes file lags `rows` after a failed write, so the next record retries it.
    unsaved: bool,
}

impl Snapshot {
    /// The snapshot as the files hold it. The rows only the changes file holds stay there until the
    /// next checkpoint, so a restart before it loses none of them.
    pub fn load(state_dir: &Path) -> Self {
        let changes = read_rows(&state_dir.join(CHANGES_NAME));
        let changed = changes.iter().map(|r| r.session_id.clone()).collect();
        Snapshot {
            state_dir: state_dir.to_path_buf(),
            rows: fold(changes, read_rows(&snapshot_path(state_dir))),
            changed,
            unsaved: false,
        }
    }

    pub fn rows(&self) -> &BTreeMap<String, CostRow> {
        &self.rows
    }

    /// Take `rows` as their sessions' current rows and rewrite the changes file when any differs
    /// from the row held — so a flush in which nothing changed writes nothing. A failed write is
    /// returned for the caller to note, and the next record retries it.
    pub fn record(&mut self, rows: impl IntoIterator<Item = CostRow>) -> std::io::Result<()> {
        for row in rows {
            if self.rows.get(&row.session_id) != Some(&row) {
                self.changed.insert(row.session_id.clone());
                self.rows.insert(row.session_id.clone(), row);
                self.unsaved = true;
            }
        }
        if !self.unsaved {
            return Ok(());
        }
        let changed = self.changed.iter().filter_map(|sid| self.rows.get(sid));
        write_rows(&self.state_dir, CHANGES_NAME, changed)?;
        self.unsaved = false;
        Ok(())
    }

    /// Drop the rows last heard before `retain_since` (epoch seconds), rewrite the snapshot file
    /// from the rest, and clear the changes. The snapshot is written first, so the changes are
    /// removed only once it holds them. A failed write is returned for the caller to note; the
    /// changes file keeps what the snapshot lacks.
    pub fn checkpoint(&mut self, retain_since: i64) -> std::io::Result<()> {
        self.rows
            .retain(|_, r| crate::ts_epoch(&r.ts).is_some_and(|t| t >= retain_since));
        write_rows(&self.state_dir, SNAPSHOT_NAME, self.rows.values())?;
        self.changed.clear();
        // The changes file is behind until it is cleared, so a failure here leaves the next record
        // to clear it.
        self.unsaved = true;
        write_rows(&self.state_dir, CHANGES_NAME, std::iter::empty())?;
        self.unsaved = false;
        Ok(())
    }
}

/// Each session's row from the changes and the snapshot: the later `ts` wins, and a tie goes to
/// the changes, which never predate the checkpoint they follow. An unparseable `ts` ranks below
/// any instant.
fn fold(changes: Vec<CostRow>, snapshot: Vec<CostRow>) -> BTreeMap<String, CostRow> {
    let mut rows: BTreeMap<String, CostRow> = BTreeMap::new();
    for row in changes.into_iter().chain(snapshot) {
        let at = row.ts.parse::<jiff::Timestamp>().ok();
        match rows.get(&row.session_id) {
            Some(held) if held.ts.parse::<jiff::Timestamp>().ok() >= at => {}
            _ => {
                rows.insert(row.session_id.clone(), row);
            }
        }
    }
    rows
}

/// The rows of one file, a malformed line dropped (fail-open on read); a missing file has none.
fn read_rows(path: &Path) -> Vec<CostRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CostRow>(l).ok())
        .collect()
}

/// Replace `<state_dir>/<name>` with `rows`, atomically (a uniquely named temp plus a rename, so a
/// reader never sees a partial file). No rows removes the file, so an empty snapshot leaves no trace.
fn write_rows<'a>(
    state_dir: &Path,
    name: &str,
    rows: impl Iterator<Item = &'a CostRow>,
) -> std::io::Result<()> {
    let path = state_dir.join(name);
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).map_err(std::io::Error::other)?);
        body.push('\n');
    }
    if body.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(state_dir)?;
    // A unique temp name (pid + sequence) means two overlapping writes — e.g. the periodic flush
    // and the shutdown flush — never share a temp path, so neither rename can fail on the other's.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = state_dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn spends(pairs: &[(&str, i64, f64)]) -> BTreeMap<String, Spend> {
        pairs
            .iter()
            .map(|(k, tokens, cost_usd)| {
                (
                    k.to_string(),
                    Spend {
                        tokens: *tokens,
                        cost_usd: *cost_usd,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn merge_counts_sums_delta_and_replaces_cumulative_per_key() {
        let base = counts(&[("input", 100), ("cacheRead", 40)]);
        // Delta: both-keys sum; a base-only key keeps its pre-restart spend; a
        // current-only key needs no baseline.
        let delta = merge_counts(&base, counts(&[("input", 10), ("output", 5)]), true);
        assert_eq!(
            delta,
            counts(&[("input", 110), ("cacheRead", 40), ("output", 5)])
        );
        // Cumulative: the current point is already the full total, so it replaces;
        // the base-only key still keeps its last known value.
        let cumulative = merge_counts(&base, counts(&[("input", 10)]), false);
        assert_eq!(cumulative, counts(&[("input", 10), ("cacheRead", 40)]));
    }

    #[test]
    fn merge_spend_applies_each_measure_under_its_own_temporality() {
        // Tokens delta, cost cumulative — the mixed-temporality session: per key,
        // tokens accumulate while cost is replaced by its full total.
        let base = spends(&[("opus", 100, 1.0)]);
        let merged = merge_spend(&base, spends(&[("opus", 10, 3.0)]), true, false);
        assert_eq!(merged, spends(&[("opus", 110, 3.0)]));
    }

    #[test]
    fn a_row_persisted_without_breakdowns_deserializes_to_empty_maps() {
        // A snapshot line written before the dimensional breakdowns existed still
        // parses — its breakdowns read as empty (not recorded), never as a parse
        // failure that would silently drop the row (and its totals) from reports.
        let line = "{\"session_id\":\"S\",\"project\":\"alpha\",\"tokens\":7,\"cost_usd\":0.5,\
                    \"active_time_s\":1.0,\"lines\":2,\"ts\":\"2026-01-01T00:00:00Z\"}";
        let row: CostRow = serde_json::from_str(line).unwrap();
        assert_eq!(row.tokens, 7);
        assert!(row.tokens_by_type.is_empty());
        assert!(row.by_model.is_empty());
        assert!(row.by_agent.is_empty());
    }

    #[test]
    fn a_temp_a_previous_run_never_renamed_is_swept_and_the_snapshot_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = snapshot_path(dir.path());
        std::fs::write(&snapshot, "{}\n").unwrap();
        std::fs::write(dir.path().join("cost_snapshot.jsonl.4321.0.tmp"), "").unwrap();
        std::fs::write(dir.path().join("cost_snapshot.jsonl.4321.1.tmp"), "").unwrap();
        std::fs::write(dir.path().join("cost_changes.jsonl.4321.2.tmp"), "").unwrap();
        std::fs::write(dir.path().join("session_index.jsonl"), "").unwrap();

        assert_eq!(sweep_orphan_temps(dir.path()), 3);
        assert!(
            snapshot.exists(),
            "the file a rename produced is not a temp"
        );
        assert!(dir.path().join("session_index.jsonl").exists());
    }

    #[test]
    fn a_failed_write_is_retried_by_the_next_record() {
        // A non-empty directory where the changes file goes makes its rename fail, whoever runs.
        let dir = tempfile::tempdir().unwrap();
        let changes = dir.path().join(CHANGES_NAME);
        std::fs::create_dir_all(changes.join("blocker")).unwrap();
        let mut snapshot = Snapshot::load(dir.path());
        let failed = snapshot.record([CostRow {
            session_id: "S1".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            ..CostRow::default()
        }]);
        assert!(failed.is_err(), "the failure is the caller's to note");
        std::fs::remove_dir_all(&changes).unwrap();
        snapshot.record([]).unwrap();
        assert!(changes.is_file());
    }

    #[test]
    fn a_checkpoint_that_could_not_clear_the_changes_is_finished_by_the_next_record() {
        // A non-empty directory where the changes file goes makes its removal fail. Once the
        // obstacle is gone, a stale changes file stands in its place, holding a row the checkpoint
        // expired; the next record, with nothing changed, still clears it.
        let dir = tempfile::tempdir().unwrap();
        let changes = dir.path().join(CHANGES_NAME);
        let row = |sid: &str, ts: &str| CostRow {
            session_id: sid.into(),
            ts: ts.into(),
            ..CostRow::default()
        };
        let mut snapshot = Snapshot::load(dir.path());
        snapshot
            .record([row("kept", "2026-01-01T00:00:00Z")])
            .unwrap();
        std::fs::remove_file(&changes).unwrap();
        std::fs::create_dir_all(changes.join("blocker")).unwrap();
        assert!(snapshot.checkpoint(0).is_err());
        std::fs::remove_dir_all(&changes).unwrap();
        std::fs::write(
            &changes,
            serde_json::to_string(&row("expired", "2000-01-01T00:00:00Z")).unwrap() + "\n",
        )
        .unwrap();
        snapshot.record([]).unwrap();
        assert!(!changes.exists());
        let sessions: Vec<_> = read_snapshot(dir.path())
            .into_iter()
            .map(|r| r.session_id)
            .collect();
        assert_eq!(sessions, ["kept"]);
    }

    #[test]
    fn a_state_dir_that_is_not_there_sweeps_nothing_rather_than_failing() {
        assert_eq!(sweep_orphan_temps(Path::new("/nonexistent/hatel")), 0);
    }
}
