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
        out.push_str(&format!(
            "| {} | {} |\n",
            escape_md_cell(&spec.name),
            escape_md_cell(&summary_line(&groups))
        ));
    }
    out
}

/// Neutralize a string for a GitHub-flavored-Markdown table cell. A literal `|` would open a new
/// column and any newline a new row, so a field value carrying either — a `tool_name`, a project
/// label from a directory name, a git branch used as a group key — would otherwise break or inject
/// into the table. `|` is backslash-escaped; every control character (newlines included) collapses
/// to a space. Stored data is untouched — this is render-only.
pub fn escape_md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '|' => out.push_str("\\|"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_md_cell_neutralizes_pipes_and_newlines() {
        // A group key like a git branch `feat/a|b` or a multiline value must not add columns/rows.
        assert_eq!(escape_md_cell("feat/a|b"), "feat/a\\|b");
        assert_eq!(escape_md_cell("line1\nline2"), "line1 line2");
        assert_eq!(escape_md_cell("a\r\nb\tc"), "a  b c");
        // Ordinary text (including the empty-group em-dash) is untouched.
        assert_eq!(escape_md_cell("Bash"), "Bash");
        assert_eq!(escape_md_cell("—"), "—");
    }

    #[test]
    fn markdown_row_stays_one_row_with_hostile_group_key() {
        // End-to-end: a record whose group key carries a pipe and a newline must still render as a
        // single table row with exactly the column delimiters of the header.
        let dir = std::env::temp_dir().join(format!("ht-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ledger")).unwrap();
        let line = r#"{"ts":"2026-01-01T00:00:00Z","kind":"tool","_schema_version":1,"payload":{"session_id":"s","project":"p","tool_name":"a|b\nc","duration_ms":1,"ok":1}}"#;
        std::fs::write(dir.join("ledger/tool.jsonl"), format!("{line}\n")).unwrap();
        let cfg = Config {
            sink: crate::sink::SinkKind::Jsonl,
            state_dir: dir.clone(),
            ledger_dir: dir.join("ledger"),
            plugins: vec![],
            rotate_bytes: 1 << 20,
            retention_days: 90,
            disabled: false,
            strict: false,
        };
        let reg = crate::schema::load_core().unwrap();
        let q = Query {
            since: 0,
            top_n: 10,
            project: None,
            kind: Some("tool"),
            filters: &[],
        };
        let md = format_markdown(&reg, &cfg, "30d", &q);
        let _ = std::fs::remove_dir_all(&dir);
        let tool_row = md
            .lines()
            .find(|l| l.starts_with("| tool "))
            .expect("a tool row");
        assert!(
            !tool_row.contains("a|b"),
            "raw pipe must be escaped: {tool_row}"
        );
        assert!(
            tool_row.contains("a\\|b"),
            "pipe escaped as \\|: {tool_row}"
        );
        // The newline inside the value collapsed to a space, so the text after it stays on the SAME
        // row (no injected table row) and the row still closes cleanly.
        assert!(
            tool_row.contains("c [count=1"),
            "newline collapsed; value stays one row: {tool_row}"
        );
        assert!(tool_row.ends_with(" |"), "row closes cleanly: {tool_row}");
    }
}
