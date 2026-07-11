//! Native-OTel cost snapshot. Cost/tokens are a *snapshot* of current totals, not
//! an event stream, so they belong in a rewritten file keyed by session — not the
//! append-only event sink (which would bloat with near-identical rows). The
//! receiver merges current totals into this file periodically and on shutdown.
//! Because Claude Code exports to a single OTel endpoint, exactly one receiver is
//! ever the active writer for a given state dir; each write still goes through a
//! uniquely-named temp file plus an atomic rename, so even an accidental overlap
//! stays consistent. One line per session means no growth, and `report` reads it
//! so cost survives offline.

use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

fn snapshot_path(state_dir: &Path) -> PathBuf {
    state_dir.join("cost_snapshot.jsonl")
}

pub fn read_snapshot(state_dir: &Path) -> Vec<CostRow> {
    let Ok(text) = std::fs::read_to_string(snapshot_path(state_dir)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CostRow>(l).ok())
        .collect()
}

/// Merge current per-session totals into the snapshot by `session_id` and rewrite it
/// atomically (temp + rename). Existing sessions are preserved across receiver restarts;
/// current ones are replaced. Rows older than `retain_since` (epoch seconds) are dropped,
/// so the durable file and the per-flush rewrite stay bounded at the report horizon — a
/// session past the retention window is beyond any report's reach. Fail-open: a write
/// error is a stderr note.
pub fn merge_snapshot(state_dir: &Path, rows: Vec<CostRow>, retain_since: i64) {
    let mut by_session: BTreeMap<String, CostRow> = read_snapshot(state_dir)
        .into_iter()
        .map(|r| (r.session_id.clone(), r))
        .collect();
    for row in rows {
        by_session.insert(row.session_id.clone(), row);
    }
    let kept: Vec<&CostRow> = by_session
        .values()
        .filter(|r| crate::ts_epoch(&r.ts).is_some_and(|t| t >= retain_since))
        .collect();
    // A fully-aged-out snapshot leaves no trace: when nothing survives the retain cutoff,
    // remove any existing file rather than rewriting it as a lone newline — and skip the write
    // entirely when there was never a file. Symmetric with the never-create case.
    if kept.is_empty() {
        let path = snapshot_path(state_dir);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return;
    }
    let body = kept
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    if let Err(e) = write_atomic(state_dir, &body) {
        eprintln!("hatel: cost snapshot write failed: {e}");
    }
}

fn write_atomic(state_dir: &Path, body: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let final_path = snapshot_path(state_dir);
    // A unique temp name (pid + sequence) means two overlapping flushes — e.g. the
    // periodic task and the shutdown flush — never share a temp path, so neither
    // rename can fail on the other's file.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = final_path.with_extension(format!("jsonl.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{body}\n"))?;
    std::fs::rename(&tmp, &final_path)
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
}
