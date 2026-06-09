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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRow {
    pub session_id: String,
    pub project: String,
    pub tokens: i64,
    pub cost_usd: f64,
    pub active_time_s: f64,
    pub lines: i64,
    pub ts: String,
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
    // Nothing to persist and no file to prune → don't create a spurious empty snapshot
    // (a quiet receiver leaves no trace).
    if kept.is_empty() && !snapshot_path(state_dir).exists() {
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
