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
use crate::{Config, ts_epoch};

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
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Measure {
    pub name: String,
    pub sum: f64,
    pub valid_count: i64,
    pub missing_count: i64,
    pub invalid_count: i64,
    pub avg: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: Option<String>,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bucket {
    pub upper_bound: f64,
    pub count: i64,
}

/// One group's aggregate: its key, how many records it had, and the sum of each
/// declared measure (in `measures` order). The key is `None` for the records that carry no
/// value for the dimension at all — distinct from ones whose value is empty, which group under
/// the empty string — so a machine reader tells an absent field from any value by shape.
#[derive(Debug, Clone, Serialize)]
pub struct GroupAgg {
    pub key: Option<String>,
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
    /// An exact stored contract, or an explicit unverified interpretation of legacy records.
    pub contract: Option<&'a str>,
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

/// Whether this query applies a project restriction. Common metadata supplies attribution for
/// every Kind; legacy records without attribution cannot match a project restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectScope {
    Unrestricted,
    Applied,
}

/// One Kind's answer, self-describing: which dimension it was grouped by, which measure ranked
/// it (`None` — and so count — when the Kind declares no measures), what each group's count
/// counts (`identity`: distinct values of that field, else records), how the project scope
/// applied, and the oldest retained occurrence (`retained_since`). This timestamp does not
/// establish continuous collection; an empty period is an absence of records, not proof of inactivity.
#[derive(Debug, Clone, Serialize)]
pub struct KindSection {
    pub kind: String,
    pub mode: crate::registry::KindMode,
    pub group_by: String,
    pub sort_by: Option<String>,
    pub identity: Option<String>,
    pub project_scope: ProjectScope,
    pub retained_since: Option<String>,
    pub contract: String,
    pub contracts: Vec<crate::query::ContractCount>,
    pub groups: Vec<GroupAgg>,
    pub error: Option<String>,
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
    pub cost_error: Option<String>,
    pub cost_basis: &'static str,
    /// Kinds the ledger holds that this registry cannot read. A report answered over less than
    /// the store contains says so here, rather than presenting the part it could read as the
    /// whole.
    pub unreadable_kinds: Option<UnreadableKinds>,
    pub storage_error: Option<String>,
}

impl Report {
    pub fn build(reg: &Registry, cfg: &Config, window: &str, q: &Query) -> Report {
        let kinds = reg
            .kinds()
            .filter(|s| q.kind.is_none_or(|k| s.name == k))
            .map(|spec| {
                let project_scope = match q.project {
                    None => ProjectScope::Unrestricted,
                    Some(_) => ProjectScope::Applied,
                };
                let group_by = q.group_by.unwrap_or(&spec.group_key);
                let sort_by = q.sort_by.or(spec.measures.first().map(String::as_str));
                let result = crate::query::section(cfg, spec, q);
                let (groups, retained_since, contracts, error) = match result {
                    Ok(result) => match result.groups {
                        Ok(groups) => (groups, result.retained_since, result.contracts, None),
                        Err(error) => (
                            Vec::new(),
                            result.retained_since,
                            result.contracts,
                            Some(error.to_string()),
                        ),
                    },
                    Err(e) => (Vec::new(), None, Vec::new(), Some(e.to_string())),
                };
                KindSection {
                    kind: spec.name.clone(),
                    mode: spec.mode,
                    group_by: group_by.to_string(),
                    sort_by: sort_by.map(str::to_string),
                    identity: spec.identity.clone(),
                    project_scope,
                    retained_since,
                    contract: q.contract.unwrap_or(&spec.schema_id).into(),
                    contracts,
                    groups,
                    error,
                }
            })
            .collect();
        let (cost, cost_error) = match q.kind {
            Some(_) => (Vec::new(), None),
            None => match cost_rows(&cfg.state_dir, q) {
                Ok(rows) => (rows, None),
                Err(error) => (Vec::new(), Some(error.to_string())),
            },
        };
        let (unreadable_kinds, storage_error) = match UnreadableKinds::detect(reg, cfg) {
            Ok(found) => (found, None),
            Err(error) => (None, Some(error.to_string())),
        };
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
            cost,
            cost_error,
            cost_basis: "session_lifetime_totals_selected_by_last_seen",
            unreadable_kinds,
            storage_error,
        }
    }

    pub fn failed(&self) -> bool {
        self.storage_error.is_some()
            || self.cost_error.is_some()
            || self.kinds.iter().any(|kind| kind.error.is_some())
    }
}

/// Cost snapshot rows within the window, optionally restricted to one project — so the cost
/// section honors `--window`/`--project` exactly as the Kind aggregation does.
fn cost_rows(state_dir: &std::path::Path, q: &Query) -> std::io::Result<Vec<CostRow>> {
    Ok(cost::read_snapshot(state_dir)?
        .into_iter()
        .filter(|r| {
            q.project
                .is_none_or(|p| r.project == p || r.project_key.as_deref() == Some(p))
        })
        .filter(|r| ts_epoch(&r.ts).is_some_and(|t| t >= q.since))
        .collect())
}

/// Roll cost rows up by project, ranked by spend and capped at `top_n` (0 = all). The group
/// count is sessions, and the measures are the four totals a snapshot row carries — the same
/// shape a Kind's groups take, so one renderer serves both. A row whose project was never
/// resolved keys on the empty string it actually holds, so it can never merge with a real
/// project however that project happens to be named; naming it is the renderer's job.
pub fn cost_by_project(rows: &[CostRow], top_n: usize) -> Vec<GroupAgg> {
    let mut by_project: BTreeMap<&str, (i64, [f64; 4], [i64; 4])> = BTreeMap::new();
    let metrics = [
        "token.usage",
        "cost.usage",
        "active_time.total",
        "lines_of_code.count",
    ];
    for row in rows {
        let entry = by_project
            .entry(row.project_key.as_deref().unwrap_or(&row.project))
            .or_insert((0, [0.0; 4], [0; 4]));
        entry.0 += 1;
        for (i, value) in [
            row.tokens as f64,
            row.cost_usd,
            row.active_time_s,
            row.lines as f64,
        ]
        .into_iter()
        .enumerate()
        {
            if row
                .observed_metrics
                .as_ref()
                .is_some_and(|observed| observed.contains(metrics[i]))
            {
                entry.1[i] += value;
                entry.2[i] += 1;
            }
        }
    }
    let mut groups: Vec<GroupAgg> = by_project
        .into_iter()
        .map(|(key, (count, sums, observed))| GroupAgg {
            key: Some(key.to_string()),
            count,
            sums: ["tokens", COST_RANK, "active_time_s", "lines"]
                .iter()
                .zip(sums)
                .zip(observed)
                .map(|((name, sum), valid_count)| Measure {
                    name: name.to_string(),
                    sum,
                    valid_count,
                    missing_count: count - valid_count,
                    ..Default::default()
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

/// Aggregate one Kind through the shared SQL engine without discarding query failures.
pub fn aggregate(
    reg: &Registry,
    cfg: &Config,
    kind: &str,
    q: &Query,
) -> crate::Result<Vec<GroupAgg>> {
    let Some(spec) = reg.kind(kind) else {
        return Err(crate::Error::UnknownKind(kind.into()));
    };
    crate::query::aggregate(cfg, spec, q)
}

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

/// Order groups by `measure` descending, then by key so ties are stable, the records carrying
/// no value for the dimension after every named group.
fn rank(groups: &mut [GroupAgg], measure: Option<&str>) {
    groups.sort_by(|a, b| {
        rank_value(b, measure)
            .partial_cmp(&rank_value(a, measure))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| (a.key.is_none(), &a.key).cmp(&(b.key.is_none(), &b.key)))
    });
}

fn truncate(groups: &mut Vec<GroupAgg>, top_n: usize) {
    if top_n > 0 {
        groups.truncate(top_n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(key: &str, count: i64, sums: &[(&str, f64)]) -> GroupAgg {
        GroupAgg {
            key: Some(key.to_string()),
            count,
            sums: sums
                .iter()
                .map(|(name, sum)| Measure {
                    name: name.to_string(),
                    sum: *sum,
                    ..Default::default()
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
        assert_eq!(groups[0].key.as_deref(), Some("noisy"));
        rank(&mut groups, Some("violations"));
        assert_eq!(groups[0].key.as_deref(), Some("real"));
    }

    #[test]
    fn a_measureless_kind_ranks_by_record_count() {
        let mut groups = vec![group("few", 2, &[]), group("many", 40, &[])];
        rank(&mut groups, None);
        assert_eq!(groups[0].key.as_deref(), Some("many"));
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
            observed_metrics: Some(
                [
                    "token.usage",
                    "cost.usage",
                    "active_time.total",
                    "lines_of_code.count",
                ]
                .map(String::from)
                .into(),
            ),
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
        assert_eq!(groups[0].key.as_deref(), Some("alpha"));
        assert_eq!(groups[0].count, 2, "the group count is sessions");
        assert_eq!(groups[0].sums[0].sum, 40.0, "tokens summed across sessions");
        assert_eq!(groups[0].sums[1].sum, 10.0);
        assert_eq!(groups[1].key.as_deref(), Some("beta"));
        // A row whose project was never resolved keys on the empty string it holds, so a real
        // project can never absorb its spend — whatever that project is called.
        assert_eq!(groups[2].key.as_deref(), Some(""));
    }

    #[test]
    fn cost_summary_uses_explicit_observations_and_keeps_true_zero() {
        let rows = [
            CostRow {
                project: "repo".into(),
                cost_usd: 0.0,
                observed_metrics: Some(["cost.usage".into()].into()),
                ..Default::default()
            },
            CostRow {
                project: "repo".into(),
                cost_usd: 19.0,
                ..Default::default()
            },
        ];
        let groups = cost_by_project(&rows, 0);
        let cost = &groups[0].sums[1];
        assert_eq!(
            (cost.sum, cost.valid_count, cost.missing_count),
            (0.0, 1, 1)
        );
        assert_eq!(groups[0].sums[0].valid_count, 0);
    }
}
