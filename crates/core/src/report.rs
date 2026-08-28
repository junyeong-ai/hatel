//! Windowed aggregation over the configured storage backend (read via the sink abstraction),
//! and the report model every output format renders.
//!
//! A Kind declares its data — an allow-list of fields, which of them are numeric measures, and
//! which dimension and measure a report defaults to. A query asks a question of that data: over
//! what window, restricted how, grouped by which dimension, ranked by which measure. Keeping the
//! two apart is what lets one schema answer more than one question, and what keeps every output
//! format rendering the same computed answer rather than deriving its own.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::cost::{self, CostRow};
use crate::registry::Registry;
use crate::schema::UnreadableKinds;
use crate::{Config, sink, ts_epoch};

/// How many groups a report shows per Kind.
pub const TOP_N: usize = 5;

/// Maximum window, so `days * 86_400` can never overflow and an absurd value is
/// rejected rather than silently wrapping (~273 years is far beyond any real use).
const MAX_WINDOW_DAYS: i64 = 100_000;

/// The dimension the cost snapshot rolls up by, and the measure it ranks by. Cost rows are
/// per-session records of a project's spend, so the project is the only dimension they carry.
const COST_DIMENSION: &str = "project";
const COST_RANK: &str = "cost_usd";

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

/// One `field=value` restriction, carried as a pair rather than a `"field=value"` string — a
/// value may itself contain `=`, and a machine consumer should never have to re-split what the
/// CLI already parsed.
#[derive(Debug, Clone, Serialize)]
pub struct Filter {
    pub field: String,
    pub value: String,
}

/// One report query: the window boundary plus every user restriction, carried as a unit
/// through all output formats so they cannot disagree on what is in scope.
#[derive(Debug, Clone, Copy)]
pub struct Query<'a> {
    /// Epoch-second lower bound (the rolling window's start). The caller computes it once
    /// so every Kind AND the cost section share one boundary.
    pub since: i64,
    /// Groups shown per Kind (0 = all).
    pub top_n: usize,
    /// Restrict to one project (its label).
    pub project: Option<&'a str>,
    /// Restrict to one registered Kind.
    pub kind: Option<&'a str>,
    /// Dimension to group by, overriding the Kind's declared default. Validated against that
    /// Kind's allow-list by the caller, so a dimension that reaches here is one the Kind records.
    pub group_by: Option<&'a str>,
    /// Measure to rank groups by, overriding the Kind's first declared measure. Validated
    /// against that Kind's measures by the caller.
    pub sort_by: Option<&'a str>,
    /// `(field, value)` exact-match restrictions — a record counts only when every pair
    /// matches. Values compare against the same rendering the group-key column shows, so
    /// what a report displays is exactly what can be filtered on.
    pub filters: &'a [(String, String)],
}

/// Where a Kind's records stand relative to a requested project scope. A Kind that does not
/// record `project` carries no project at all, so a project-scoped report has nothing of its to
/// show — which is a different statement from "this project did none of it", and is reported as
/// such rather than as an empty result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectScope {
    Unrestricted,
    Applied,
    Unsupported,
}

/// One Kind's answer, self-describing: which dimension it was grouped by, which measure ranked
/// it (`None` — and so record count — when the Kind declares no measures), and how the project
/// scope applied.
#[derive(Debug, Clone, Serialize)]
pub struct KindSection {
    pub kind: String,
    pub group_by: String,
    pub sort_by: Option<String>,
    pub project_scope: ProjectScope,
    pub groups: Vec<GroupAgg>,
}

/// A whole report: the query that produced it and the answer to it. Computed once, rendered by
/// every format from this one value.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub window: String,
    pub project: Option<String>,
    pub filters: Vec<Filter>,
    pub top_n: usize,
    pub kinds: Vec<KindSection>,
    /// The native-OTel cost snapshot, one row per session in the window. Kept whole — it is the
    /// join table a caller reaches for when relating spend to its own records — while
    /// [`cost_by_project`] provides the ranked rollup a summary shows.
    pub cost: Vec<CostRow>,
    /// Kinds the ledger holds that this registry cannot read. A report answered over less than
    /// the store contains says so here, rather than presenting the part it could read as the
    /// whole — the same honesty [`ProjectScope::Unsupported`] gives a Kind that records no
    /// project.
    pub unreadable_kinds: Option<UnreadableKinds>,
}

impl Report {
    pub fn build(reg: &Registry, cfg: &Config, window: &str, q: &Query) -> Report {
        let kinds = reg
            .kinds()
            .filter(|s| q.kind.is_none_or(|k| s.name == k))
            .map(|spec| {
                let project_scope = match q.project {
                    None => ProjectScope::Unrestricted,
                    Some(_) if spec.fields.contains("project") => ProjectScope::Applied,
                    Some(_) => ProjectScope::Unsupported,
                };
                let group_by = q.group_by.unwrap_or(&spec.group_key);
                let sort_by = q.sort_by.or(spec.measures.first().map(String::as_str));
                KindSection {
                    kind: spec.name.clone(),
                    group_by: group_by.to_string(),
                    sort_by: sort_by.map(str::to_string),
                    project_scope,
                    groups: match project_scope {
                        ProjectScope::Unsupported => Vec::new(),
                        _ => aggregate(reg, cfg, &spec.name, q),
                    },
                }
            })
            .collect();
        Report {
            window: window.to_string(),
            project: q.project.map(str::to_string),
            filters: q
                .filters
                .iter()
                .map(|(field, value)| Filter {
                    field: field.clone(),
                    value: value.clone(),
                })
                .collect(),
            top_n: q.top_n,
            kinds,
            // The cost snapshot is not a Kind, so a Kind-scoped report leaves it out entirely
            // rather than showing spend the scope did not ask about.
            cost: match q.kind {
                Some(_) => Vec::new(),
                None => cost_rows(&cfg.state_dir, q),
            },
            unreadable_kinds: UnreadableKinds::detect_resilient(reg, cfg),
        }
    }
}

/// Cost snapshot rows within the window, optionally restricted to one project — so the cost
/// section honors `--window`/`--project` exactly as the Kind aggregation does.
fn cost_rows(state_dir: &std::path::Path, q: &Query) -> Vec<CostRow> {
    cost::read_snapshot(state_dir)
        .into_iter()
        .filter(|r| q.project.is_none_or(|p| r.project == p))
        .filter(|r| ts_epoch(&r.ts).is_some_and(|t| t >= q.since))
        .collect()
}

/// Roll cost rows up by project, ranked by spend and capped at `top_n` (0 = all). The group
/// count is sessions, and the measures are the four totals a snapshot row carries — the same
/// shape a Kind's groups take, so one renderer serves both. A row whose project was never
/// resolved keys on the empty string it actually holds, so it can never merge with a real
/// project however that project happens to be named; naming it is the renderer's job.
pub fn cost_by_project(rows: &[CostRow], top_n: usize) -> Vec<GroupAgg> {
    let mut by_project: BTreeMap<&str, (i64, [f64; 4])> = BTreeMap::new();
    for row in rows {
        let entry = by_project
            .entry(row.project.as_str())
            .or_insert((0, [0.0; 4]));
        entry.0 += 1;
        entry.1[0] += row.tokens as f64;
        entry.1[1] += row.cost_usd;
        entry.1[2] += row.active_time_s;
        entry.1[3] += row.lines as f64;
    }
    let mut groups: Vec<GroupAgg> = by_project
        .into_iter()
        .map(|(key, (count, sums))| GroupAgg {
            key: key.to_string(),
            count,
            sums: ["tokens", COST_RANK, "active_time_s", "lines"]
                .iter()
                .zip(sums)
                .map(|(name, sum)| Measure {
                    name: name.to_string(),
                    sum,
                })
                .collect(),
        })
        .collect();
    rank(&mut groups, Some(COST_RANK));
    truncate(&mut groups, top_n);
    groups
}

/// The dimension and rank measure the cost rollup uses, so a renderer describes it in the same
/// terms as a Kind section.
pub fn cost_axes() -> (&'static str, &'static str) {
    (COST_DIMENSION, COST_RANK)
}

/// Aggregate one Kind under `q`: group in-window records by the query's dimension (the Kind's
/// `group_key` unless overridden), count them, and sum each declared measure. Records are read
/// from the configured storage backend (JSONL / SQLite).
///
/// `kind` is the Kind being aggregated right now (the caller's loop variable); `q.kind` is
/// the report-level restriction the caller applies when choosing which Kinds to loop over,
/// and is not consulted here.
pub fn aggregate(reg: &Registry, cfg: &Config, kind: &str, q: &Query) -> Vec<GroupAgg> {
    let Some(spec) = reg.kind(kind) else {
        return Vec::new();
    };
    let dimension = q.group_by.unwrap_or(&spec.group_key);
    let mut groups: BTreeMap<String, (i64, Vec<f64>)> = BTreeMap::new();
    // `since` lets the backend skip out-of-window history (SQLite); the exact filter
    // below is the correctness gate (and does the windowing for JSONL).
    for env in sink::read_records(cfg, kind, Some(q.since)) {
        // A record with an unparseable timestamp is dropped (not silently bucketed at
        // epoch 0, which would flip between always-in and always-out by window size).
        match ts_epoch(&env.ts) {
            Some(ts) if ts >= q.since => {}
            _ => continue,
        }
        if let Some(p) = q.project
            && env.payload.get("project").and_then(|v| v.as_str()) != Some(p)
        {
            continue;
        }
        // `--filter field=value`: a record lacking the field never matches.
        if !q
            .filters
            .iter()
            .all(|(field, want)| env.payload.get(field).map(value_label).as_deref() == Some(want))
        {
            continue;
        }
        let key = env
            .payload
            .get(dimension)
            .map(value_label)
            .unwrap_or_else(|| MISSING_DIMENSION.to_string());
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
    rank(
        &mut rows,
        q.sort_by.or(spec.measures.first().map(String::as_str)),
    );
    truncate(&mut rows, q.top_n);
    rows
}

/// A record that does not carry the grouping dimension. Distinct from a record whose value for
/// it is empty, which groups under its own (empty) key.
const MISSING_DIMENSION: &str = "—";

/// The value a group is ranked by: its sum of `measure`, or its record count when the Kind
/// declares no measures. A renderer drawing magnitude reads it from here too, so a bar can never
/// describe a different quantity than the order it appears in.
pub fn rank_value(group: &GroupAgg, measure: Option<&str>) -> f64 {
    match measure {
        Some(m) => group
            .sums
            .iter()
            .find(|s| s.name == m)
            .map(|s| s.sum)
            .unwrap_or(0.0),
        None => group.count as f64,
    }
}

/// Order groups by `measure` descending, then by key so ties are stable.
fn rank(groups: &mut [GroupAgg], measure: Option<&str>) {
    groups.sort_by(|a, b| {
        rank_value(b, measure)
            .partial_cmp(&rank_value(a, measure))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
}

fn truncate(groups: &mut Vec<GroupAgg>, top_n: usize) {
    if top_n > 0 {
        groups.truncate(top_n);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn group(key: &str, count: i64, sums: &[(&str, f64)]) -> GroupAgg {
        GroupAgg {
            key: key.to_string(),
            count,
            sums: sums
                .iter()
                .map(|(name, sum)| Measure {
                    name: name.to_string(),
                    sum: *sum,
                })
                .collect(),
        }
    }

    #[test]
    fn ranking_follows_the_named_measure_not_its_position() {
        // A Kind whose first measure counts work done and whose second counts what went wrong:
        // ranking by either must be possible, because the schema records the data while the
        // query asks the question.
        let mut groups = vec![
            group(
                "noisy",
                9,
                &[("evaluations", 12_000_000.0), ("violations", 0.0)],
            ),
            group(
                "real",
                3,
                &[("evaluations", 21_000.0), ("violations", 203.0)],
            ),
        ];
        rank(&mut groups, Some("evaluations"));
        assert_eq!(groups[0].key, "noisy");
        rank(&mut groups, Some("violations"));
        assert_eq!(groups[0].key, "real");
    }

    #[test]
    fn a_measureless_kind_ranks_by_record_count() {
        let mut groups = vec![group("few", 2, &[]), group("many", 40, &[])];
        rank(&mut groups, None);
        assert_eq!(groups[0].key, "many");
    }

    #[test]
    fn cost_rolls_up_by_project_ranked_by_spend() {
        let row = |session: &str, project: &str, cost_usd: f64, tokens: i64| CostRow {
            session_id: session.to_string(),
            project: project.to_string(),
            tokens,
            cost_usd,
            active_time_s: 1.0,
            lines: 2,
            ts: "2026-01-01T00:00:00Z".to_string(),
            ..CostRow::default()
        };
        let groups = cost_by_project(
            &[
                row("s1", "alpha", 1.0, 10),
                row("s2", "beta", 5.0, 20),
                row("s3", "alpha", 9.0, 30),
                row("s4", "", 0.5, 1),
            ],
            0,
        );
        assert_eq!(groups[0].key, "alpha");
        assert_eq!(groups[0].count, 2, "the group count is sessions");
        assert_eq!(groups[0].sums[0].sum, 40.0, "tokens summed across sessions");
        assert_eq!(groups[0].sums[1].sum, 10.0);
        assert_eq!(groups[1].key, "beta");
        // A row whose project was never resolved keys on the empty string it holds, so a real
        // project can never absorb its spend — whatever that project is called.
        assert_eq!(groups[2].key, "");
    }
}
