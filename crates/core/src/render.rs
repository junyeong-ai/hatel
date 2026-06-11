//! Presentation. `report.rs` computes the aggregates; this formats them for a
//! terminal table or a Markdown report. Machine-readable JSON is assembled by the
//! CLI (it also folds in the cost snapshot).

use crate::Config;
use crate::registry::Registry;
use crate::report::{GroupAgg, Query, aggregate};

pub fn format_markdown(reg: &Registry, cfg: &Config, window_label: &str, q: &Query) -> String {
    let mut out = format!("# hatel — rolling {window_label}{}\n\n", scope_label(q));
    out.push_str("| kind | top groups |\n|---|---|\n");
    for spec in reg.kinds().filter(|s| q.kind.is_none_or(|k| s.name == k)) {
        let groups = aggregate(reg, cfg, &spec.name, q);
        out.push_str(&format!("| {} | {} |\n", spec.name, summary_line(&groups)));
    }
    out
}

pub fn format_table(reg: &Registry, cfg: &Config, window_label: &str, q: &Query) -> String {
    let mut out = format!("=== hatel — rolling {window_label}{} ===\n", scope_label(q));
    for spec in reg.kinds().filter(|s| q.kind.is_none_or(|k| s.name == k)) {
        let groups = aggregate(reg, cfg, &spec.name, q);
        out.push_str(&format!("{:<16} {}\n", spec.name, summary_line(&groups)));
    }
    out
}

/// The header's restriction summary — the project and each `field=value` filter — so a saved
/// report states what it covers.
fn scope_label(q: &Query) -> String {
    let mut scope = q
        .project
        .map(|p| format!(" — project {p}"))
        .unwrap_or_default();
    for (field, value) in q.filters {
        scope.push_str(&format!(" — {field}={value}"));
    }
    scope
}

fn summary_line(groups: &[GroupAgg]) -> String {
    if groups.is_empty() {
        return "—".to_string();
    }
    groups
        .iter()
        .map(group_summary)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `key(count)` for a plain Kind; `key [count=N, measure=sum, …]` when the Kind
/// declares measures.
fn group_summary(g: &GroupAgg) -> String {
    if g.sums.is_empty() {
        return format!("{}({})", g.key, g.count);
    }
    let measures = g
        .sums
        .iter()
        .map(|m| format!("{}={}", m.name, fmt_num(m.sum)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} [count={}, {}]", g.key, g.count, measures)
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}
