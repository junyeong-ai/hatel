//! Folds decoded metric points and event counts into per-session totals. The fold
//! rule comes from the data, never an assumption: a delta datapoint accumulates;
//! a cumulative or gauge datapoint replaces the prior value for its series.
//!
//! Counts are best-effort under OTLP retransmission: the accumulate paths (delta
//! metrics and event counts) are not idempotent, so a replayed batch would inflate
//! them. In the intended deployment the exporter pushes to this local receiver,
//! which always answers 200 immediately, so retries do not occur in practice. The
//! one window that survives the immediate ack is a receiver crash between a flush
//! and the response: the persisted baseline then already holds a delta the client
//! resends to the next receiver. Deduplicating that would take a durable per-batch
//! identity — weight this local collector deliberately doesn't carry; cumulative
//! series are immune either way (a replayed total replaces, never adds).

use std::collections::BTreeMap;

use hatel_core::cost::Spend;

use super::decode::MetricPoint;

/// The bucket label for a series that does not carry the dimension being folded —
/// recorded as such, never guessed into a real bucket. One spelling, shared by every
/// breakdown (and the live view's hide-when-trivial check).
pub const UNATTRIBUTED: &str = "(unattributed)";

// Metric identities, normalized (the `claude_code.` prefix stripped) to match the
// names `decode::parse_metrics` produces.
const TOKENS: &str = "token.usage";
const COST: &str = "cost.usage";
const ACTIVE_TIME: &str = "active_time.total";
const LINES: &str = "lines_of_code.count";

type SeriesKey = (String, Vec<(String, String)>);

#[derive(Debug, Default, Clone)]
pub struct SessionTotals {
    metric_series: BTreeMap<SeriesKey, f64>,
    events: BTreeMap<String, i64>,
    /// Metric names that have seen a non-delta (cumulative / gauge) point. Tracked per
    /// metric, not per session, so the cross-restart baseline is added only to the
    /// metrics that are delta (a cumulative metric already carries its full total) even
    /// in the unusual case of a session mixing temporalities across metrics.
    cumulative_metrics: std::collections::BTreeSet<String>,
}

impl SessionTotals {
    fn metric_sum(&self, name: &str) -> f64 {
        let total: f64 = self
            .metric_series
            .iter()
            .filter(|((metric, _), _)| metric == name)
            .map(|(_, v)| *v)
            .sum();
        // f64's `Sum` identity is -0.0, so an empty series yields -0.0; normalize it.
        total + 0.0
    }

    pub fn tokens(&self) -> i64 {
        self.metric_sum(TOKENS) as i64
    }
    pub fn cost(&self) -> f64 {
        self.metric_sum(COST)
    }
    pub fn active_time_s(&self) -> f64 {
        self.metric_sum(ACTIVE_TIME)
    }
    pub fn lines(&self) -> i64 {
        self.metric_sum(LINES) as i64
    }
    pub fn event_count(&self, name: &str) -> i64 {
        self.events.get(name).copied().unwrap_or(0)
    }

    /// Whether `metric` is delta for this session — the only case in which the
    /// cross-restart baseline should be added (a cumulative metric already reports its
    /// full total). Used per cost field so a mixed-temporality session stays correct.
    fn metric_is_delta(&self, metric: &str) -> bool {
        !self.cumulative_metrics.contains(metric)
    }

    pub fn tokens_is_delta(&self) -> bool {
        self.metric_is_delta(TOKENS)
    }
    pub fn cost_is_delta(&self) -> bool {
        self.metric_is_delta(COST)
    }
    pub fn active_time_is_delta(&self) -> bool {
        self.metric_is_delta(ACTIVE_TIME)
    }
    pub fn lines_is_delta(&self) -> bool {
        self.metric_is_delta(LINES)
    }

    /// Per-subagent spend, bucketed by attribution. The label is the `agent.name`
    /// (the specific subagent type) when present; otherwise the `query_source`
    /// category (`main` / `subagent` / `auxiliary`); and only `(unattributed)` when
    /// neither is on the series — never a guessed `main`. This is the differentiated
    /// signal — which subagent spent the budget — that session-level totals flatten
    /// away.
    pub fn by_agent(&self) -> BTreeMap<String, Spend> {
        self.spend_by(|series| {
            attr(series, "agent.name")
                .or_else(|| attr(series, "query_source"))
                .unwrap_or_else(|| UNATTRIBUTED.to_string())
        })
    }

    /// Per-model spend — the model-mix accounting (which model the budget went to),
    /// bucketed by the series' `model` attribute.
    pub fn by_model(&self) -> BTreeMap<String, Spend> {
        self.spend_by(|series| attr(series, "model").unwrap_or_else(|| UNATTRIBUTED.to_string()))
    }

    /// Token counts by the series' `type` attribute (`input` / `output` /
    /// `cacheRead` / `cacheCreation`) — the cache-hit accounting. Tokens only: the
    /// cost metric carries no `type`, so a cost-by-type map could only ever say
    /// `(unattributed)`.
    pub fn tokens_by_type(&self) -> BTreeMap<String, i64> {
        let mut out: BTreeMap<String, i64> = BTreeMap::new();
        for ((metric, series), value) in &self.metric_series {
            if metric != TOKENS {
                continue;
            }
            let label = attr(series, "type").unwrap_or_else(|| UNATTRIBUTED.to_string());
            *out.entry(label).or_insert(0) += *value as i64;
        }
        out
    }

    /// Tokens + cost summed per `label` bucket over each contributing series — the
    /// shared fold behind every spend breakdown.
    fn spend_by(&self, label: impl Fn(&[(String, String)]) -> String) -> BTreeMap<String, Spend> {
        let mut out: BTreeMap<String, Spend> = BTreeMap::new();
        for ((metric, series), value) in &self.metric_series {
            if metric != TOKENS && metric != COST {
                continue;
            }
            let entry = out.entry(label(series)).or_default();
            if metric == TOKENS {
                entry.tokens += *value as i64;
            } else {
                entry.cost_usd += *value;
            }
        }
        out
    }
}

/// The value of `key` on a series' attribute list.
fn attr(series: &[(String, String)], key: &str) -> Option<String> {
    series
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

#[derive(Debug, Default)]
pub struct Accumulator {
    by_session: BTreeMap<String, SessionTotals>,
}

impl Accumulator {
    pub fn update_metrics(&mut self, points: Vec<MetricPoint>) {
        for p in points {
            let totals = self.by_session.entry(p.session_id).or_default();
            if !p.delta {
                totals.cumulative_metrics.insert(p.name.clone());
            }
            let key = (p.name, p.series);
            let slot = totals.metric_series.entry(key).or_insert(0.0);
            if p.delta {
                *slot += p.value;
            } else {
                *slot = p.value;
            }
        }
    }

    pub fn update_events(&mut self, pairs: Vec<(String, String)>) {
        for (session, event) in pairs {
            *self
                .by_session
                .entry(session)
                .or_default()
                .events
                .entry(event)
                .or_insert(0) += 1;
        }
    }

    pub fn sessions(&self) -> &BTreeMap<String, SessionTotals> {
        &self.by_session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(name: &str, value: f64, delta: bool, series: Vec<(&str, &str)>) -> MetricPoint {
        MetricPoint {
            name: name.to_string(),
            value,
            session_id: "S".to_string(),
            series: series
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            delta,
        }
    }

    #[test]
    fn delta_accumulates_and_cumulative_replaces() {
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![point(TOKENS, 10.0, true, vec![("type", "output")])]);
        acc.update_metrics(vec![point(TOKENS, 5.0, true, vec![("type", "output")])]);
        assert_eq!(acc.sessions()["S"].tokens(), 15);

        let mut cum = Accumulator::default();
        cum.update_metrics(vec![point(COST, 1.0, false, vec![])]);
        cum.update_metrics(vec![point(COST, 3.0, false, vec![])]);
        assert!((cum.sessions()["S"].cost() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn distinct_series_fold_independently() {
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![
            point(TOKENS, 10.0, true, vec![("type", "input")]),
            point(TOKENS, 20.0, true, vec![("type", "output")]),
        ]);
        assert_eq!(acc.sessions()["S"].tokens(), 30);
    }

    #[test]
    fn empty_totals_are_positive_zero() {
        let t = SessionTotals::default();
        assert!(!t.active_time_s().is_sign_negative());
        assert!(!t.cost().is_sign_negative());
        assert_eq!(format!("{:.1}", t.active_time_s()), "0.0");
    }

    #[test]
    fn by_agent_labels_honestly() {
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![point(
            TOKENS,
            10.0,
            true,
            vec![("agent.name", "Explore")],
        )]);
        acc.update_metrics(vec![point(
            TOKENS,
            20.0,
            true,
            vec![("query_source", "main")],
        )]);
        acc.update_metrics(vec![point(COST, 1.0, false, vec![])]); // no attribution
        let agents = acc.sessions()["S"].by_agent();
        assert_eq!(agents.get("Explore").map(|s| s.tokens), Some(10));
        assert_eq!(agents.get("main").map(|s| s.tokens), Some(20));
        assert!(
            agents.contains_key(UNATTRIBUTED),
            "agentless cost is not guessed as main"
        );
    }

    #[test]
    fn by_model_buckets_tokens_and_cost_per_model() {
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![
            point(
                TOKENS,
                10.0,
                true,
                vec![("model", "opus"), ("type", "input")],
            ),
            point(
                TOKENS,
                5.0,
                true,
                vec![("model", "opus"), ("type", "output")],
            ),
            point(COST, 0.5, true, vec![("model", "haiku")]),
        ]);
        let models = acc.sessions()["S"].by_model();
        assert_eq!(models.get("opus").map(|s| s.tokens), Some(15));
        assert_eq!(models.get("haiku").map(|s| s.cost_usd), Some(0.5));
    }

    #[test]
    fn tokens_by_type_folds_the_cache_split_and_ignores_cost() {
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![
            point(TOKENS, 100.0, true, vec![("type", "cacheRead")]),
            point(TOKENS, 7.0, true, vec![("type", "input")]),
            point(TOKENS, 3.0, true, vec![]), // a typeless series is not guessed
            point(COST, 1.0, true, vec![]),   // cost carries no type — excluded
        ]);
        let by_type = acc.sessions()["S"].tokens_by_type();
        assert_eq!(by_type.get("cacheRead"), Some(&100));
        assert_eq!(by_type.get("input"), Some(&7));
        assert_eq!(by_type.get(UNATTRIBUTED), Some(&3));
        assert_eq!(by_type.values().sum::<i64>(), 110);
    }

    #[test]
    fn temporality_is_tracked_per_metric_for_baseline_decisions() {
        // A session mixing a delta tokens metric with a cumulative cost metric: the
        // baseline applies to tokens (delta) but not cost (already a full total).
        let mut acc = Accumulator::default();
        acc.update_metrics(vec![
            point(TOKENS, 10.0, true, vec![]),
            point(COST, 5.0, false, vec![]),
        ]);
        let t = &acc.sessions()["S"];
        assert!(t.tokens_is_delta(), "delta tokens → baseline added");
        assert!(!t.cost_is_delta(), "cumulative cost → baseline NOT added");
    }

    #[test]
    fn events_count_per_name() {
        let mut acc = Accumulator::default();
        acc.update_events(vec![
            ("S".to_string(), "skill_activated".to_string()),
            ("S".to_string(), "skill_activated".to_string()),
        ]);
        assert_eq!(acc.sessions()["S"].event_count("skill_activated"), 2);
    }
}
