//! Windowed aggregation over the configured storage backend (read via the sink
//! abstraction). Records are grouped by each Kind's `group_key`; the Kind's
//! `measures` are summed per group.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::registry::Registry;
use crate::{Config, sink, ts_epoch};

/// How many groups a report shows per Kind.
pub const TOP_N: usize = 5;

/// Maximum window, so `days * 86_400` can never overflow and an absurd value is
/// rejected rather than silently wrapping (~273 years is far beyond any real use).
const MAX_WINDOW_DAYS: i64 = 100_000;

/// Parse a `<n>d` window into seconds.
pub fn parse_window(spec: &str) -> Option<i64> {
    let days: i64 = spec.strip_suffix('d')?.parse().ok()?;
    if days <= 0 || days > MAX_WINDOW_DAYS {
        return None;
    }
    Some(days * 86_400)
}

/// One measure's summed value for a group.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Measure {
    pub name: String,
    pub sum: f64,
}

/// One group's aggregate: its key, how many records it had, and the sum of each
/// declared measure (in `measures` order).
#[derive(Debug, Clone, Serialize)]
pub struct GroupAgg {
    pub key: String,
    pub count: i64,
    pub sums: Vec<Measure>,
}

/// Aggregate one Kind from `since` (an epoch-second cutoff) to now: group by `group_key`,
/// count records, and sum each measure. Optionally restricted to one `project`. Records
/// are read from the configured storage backend (JSONL / SQLite). The caller computes a
/// single `since` so every Kind AND the cost section share one window boundary. Groups are
/// ranked by the first measure's sum (the primary metric) when the Kind declares measures,
/// otherwise by record count, then by key; capped at `top_n`.
pub fn aggregate(
    reg: &Registry,
    cfg: &Config,
    kind: &str,
    since: i64,
    top_n: usize,
    project: Option<&str>,
) -> Vec<GroupAgg> {
    let Some(spec) = reg.kind(kind) else {
        return Vec::new();
    };
    let mut groups: BTreeMap<String, (i64, Vec<f64>)> = BTreeMap::new();
    // `since` lets the backend skip out-of-window history (SQLite); the exact filter
    // below is the correctness gate (and does the windowing for JSONL).
    for env in sink::read_records(cfg, kind, Some(since)) {
        // A record with an unparseable timestamp is dropped (not silently bucketed at
        // epoch 0, which would flip between always-in and always-out by window size).
        match ts_epoch(&env.ts) {
            Some(ts) if ts >= since => {}
            _ => continue,
        }
        if let Some(p) = project
            && env.payload.get("project").and_then(|v| v.as_str()) != Some(p)
        {
            continue;
        }
        let key = env
            .payload
            .get(&spec.group_key)
            .map(value_label)
            .unwrap_or_else(|| "—".to_string());
        let entry = groups
            .entry(key)
            .or_insert_with(|| (0, vec![0.0; spec.measures.len()]));
        entry.0 += 1;
        for (i, m) in spec.measures.iter().enumerate() {
            entry.1[i] += env.payload.get(m).map(numeric).unwrap_or(0.0);
        }
    }
    let mut rows: Vec<GroupAgg> = groups
        .into_iter()
        .map(|(key, (count, sums))| GroupAgg {
            key,
            count,
            sums: spec
                .measures
                .iter()
                .cloned()
                .zip(sums)
                .map(|(name, sum)| Measure { name, sum })
                .collect(),
        })
        .collect();
    let rank = |g: &GroupAgg| g.sums.first().map(|m| m.sum).unwrap_or(g.count as f64);
    rows.sort_by(|a, b| {
        rank(b)
            .partial_cmp(&rank(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    if top_n > 0 {
        rows.truncate(top_n);
    }
    rows
}

fn value_label(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A measure's numeric value, accepting a JSON number or a numeric string — so a
/// field emitted as `runs=14000` (string) sums the same as `runs:=14000` (number),
/// turning an easy type slip into correct data rather than a silent zero. Non-finite
/// values (`NaN` / `inf`, including the string forms) are rejected so they cannot
/// poison a sum or the ranking.
fn numeric(v: &serde_json::Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

