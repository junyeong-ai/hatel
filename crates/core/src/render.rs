//! Presentation. [`crate::report`] computes the answer; this renders it — as an aligned terminal
//! table or as Markdown. Both formats read the same [`Report`], so they cannot disagree about
//! what was asked or what came back.

use unicode_width::UnicodeWidthStr;

use crate::report::{
    GroupAgg, KindSection, ProjectScope, Report, cost_axes, cost_by_project, rank_value,
};

/// Width of the magnitude bar that prefixes a terminal row. Fixed, so rows stay aligned across
/// sections and the eye can compare within one.
const BAR_WIDTH: usize = 18;

/// Longest group key rendered before truncation. Group keys are file paths, session ids, and
/// rule names, and one long key must not push every numeric column off the terminal.
const KEY_WIDTH: usize = 34;

/// Space between a row's columns, and so between the bar and the key it labels.
const GUTTER: usize = 2;

pub fn format_text(report: &Report) -> String {
    let mut out = format!(
        "=== hatel — rolling {}{} ===\n",
        report.window,
        scope(report)
    );
    if let Some(unreadable) = &report.unreadable_kinds {
        out.push_str(&format!("  ({unreadable})\n"));
    }
    for section in &report.kinds {
        out.push('\n');
        out.push_str(&format!("{} — {}\n", section.kind, axes(section)));
        out.push_str(&text_table(
            &section.group_by,
            "count",
            section.sort_by.as_deref(),
            &section.groups,
            note(section),
        ));
    }
    if let Some(groups) = cost_groups(report) {
        let (dimension, rank) = cost_axes();
        out.push('\n');
        out.push_str(&format!("cost — by {dimension}, ranked by {rank}\n"));
        out.push_str(&text_table(
            dimension,
            "sessions",
            Some(rank),
            &groups,
            None,
        ));
    }
    out
}

pub fn format_markdown(report: &Report) -> String {
    let mut out = format!("# hatel — rolling {}{}\n", report.window, scope(report));
    if let Some(unreadable) = &report.unreadable_kinds {
        out.push_str(&format!("\n_{unreadable}_\n"));
    }
    for section in &report.kinds {
        out.push_str(&format!("\n## {} — {}\n\n", section.kind, axes(section)));
        out.push_str(&markdown_table(
            &section.group_by,
            "count",
            &section.groups,
            note(section),
        ));
    }
    if let Some(groups) = cost_groups(report) {
        let (dimension, rank) = cost_axes();
        out.push_str(&format!("\n## cost — by {dimension}, ranked by {rank}\n\n"));
        out.push_str(&markdown_table(dimension, "sessions", &groups, None));
    }
    out
}

/// The ranked cost rollup, or `None` when the report carries no cost rows (a Kind-scoped report,
/// or a window with no recorded spend).
fn cost_groups(report: &Report) -> Option<Vec<GroupAgg>> {
    let groups = cost_by_project(&report.cost, report.top_n);
    (!groups.is_empty()).then_some(groups)
}

/// The header's restriction summary — the project and each `field=value` filter — so a saved
/// report states what it covers.
fn scope(report: &Report) -> String {
    let mut scope = report
        .project
        .as_ref()
        .map(|p| format!(" — project {p}"))
        .unwrap_or_default();
    for f in &report.filters {
        scope.push_str(&format!(" — {}={}", f.field, f.value));
    }
    scope
}

/// A section's axes in the terms the query used, so a reader knows which question was answered.
fn axes(section: &KindSection) -> String {
    let rank = section.sort_by.as_deref().unwrap_or("count");
    format!(
        "by {}, ranked by {}",
        inline(&section.group_by),
        inline(rank)
    )
}

/// Collapse control characters so a value stays on the line it was written to. Field and measure
/// names come from a plugin's TOML, which can hold a newline; one in a heading would end the
/// heading, and one in a terminal table would break every column below it.
fn inline(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// What to say in place of rows, when there are none to say anything about.
fn note(section: &KindSection) -> Option<&'static str> {
    match section.project_scope {
        ProjectScope::Unsupported => {
            Some("records no project, so a project scope cannot select it")
        }
        _ if section.groups.is_empty() => Some("no records in this window"),
        _ => None,
    }
}

fn text_table(
    dimension: &str,
    count_header: &str,
    rank: Option<&str>,
    groups: &[GroupAgg],
    note: Option<&str>,
) -> String {
    if let Some(note) = note {
        return format!("  ({note})\n");
    }
    let measures: Vec<&str> = groups
        .first()
        .map(|g| g.sums.iter().map(|m| m.name.as_str()).collect())
        .unwrap_or_default();
    let cells: Vec<Vec<String>> = groups.iter().map(numeric_cells).collect();
    let key_width = groups
        .iter()
        .map(|g| display_width(key_label(&g.key)).min(KEY_WIDTH))
        .chain(std::iter::once(display_width(dimension)))
        .max()
        .unwrap_or(0);
    // Widen each column to its own cells by walking rows in step with the columns, so a row that
    // carried a different number of measures than the header row would simply contribute nothing
    // to the columns it lacks rather than indexing past its end.
    let mut widths: Vec<usize> = std::iter::once(count_header)
        .chain(measures.iter().copied())
        .map(display_width)
        .collect();
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(display_width(cell));
        }
    }

    let mut out = String::new();
    out.push_str(&" ".repeat(GUTTER * 2 + BAR_WIDTH));
    out.push_str(&pad_right(&inline(dimension), key_width));
    for (header, width) in std::iter::once(count_header)
        .chain(measures.iter().copied())
        .zip(&widths)
    {
        out.push_str(&" ".repeat(GUTTER));
        out.push_str(&pad_left(&inline(header), *width));
    }
    out.push('\n');

    // Groups arrive ordered, so the leader sets the scale every other bar is drawn against.
    let max = groups.first().map_or(0.0, |g| rank_value(g, rank));
    for (group, row) in groups.iter().zip(&cells) {
        out.push_str(&" ".repeat(GUTTER));
        out.push_str(&bar(rank_value(group, rank), max));
        out.push_str(&" ".repeat(GUTTER));
        out.push_str(&pad_right(
            &truncate(key_label(&group.key), KEY_WIDTH),
            key_width,
        ));
        for (cell, width) in row.iter().zip(&widths) {
            out.push_str(&" ".repeat(GUTTER));
            out.push_str(&pad_left(cell, *width));
        }
        out.push('\n');
    }
    out
}

fn markdown_table(
    dimension: &str,
    count_header: &str,
    groups: &[GroupAgg],
    note: Option<&str>,
) -> String {
    if let Some(note) = note {
        return format!("_{note}_\n");
    }
    let measures: Vec<&str> = groups
        .first()
        .map(|g| g.sums.iter().map(|m| m.name.as_str()).collect())
        .unwrap_or_default();
    // Header cells are field and measure names, which a plugin author writes freely — they are
    // escaped exactly as values are, or one carrying a pipe would add a column to every row.
    let headers: Vec<String> = std::iter::once(dimension)
        .chain(std::iter::once(count_header))
        .chain(measures)
        .map(escape_md_cell)
        .collect();
    let mut out = format!("| {} |\n", headers.join(" | "));
    out.push_str("|---|");
    out.push_str(&"---:|".repeat(headers.len() - 1));
    out.push('\n');
    for group in groups {
        out.push_str(&format!("| {} |", escape_md_cell(key_label(&group.key))));
        for cell in numeric_cells(group) {
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
    }
    out
}

/// A group key as a label. A record that stored an empty value for the dimension groups under the
/// empty string; naming it keeps that row from reading as an unlabelled one, next to the em-dash
/// that marks a record which carried no such field at all. The stored key is untouched — a machine
/// consumer reads the value, not this.
fn key_label(key: &str) -> &str {
    if key.is_empty() { "(empty)" } else { key }
}

/// A group's count and measure sums as display strings, in column order.
fn numeric_cells(group: &GroupAgg) -> Vec<String> {
    std::iter::once(fmt_num(group.count as f64))
        .chain(group.sums.iter().map(|m| fmt_num(m.sum)))
        .collect()
}

/// A proportional bar of exactly [`BAR_WIDTH`] cells, linear against the leading row — so it
/// states a share of the top group and nothing more.
fn bar(value: f64, max: f64) -> String {
    let filled = if max > 0.0 && value > 0.0 {
        ((value / max) * BAR_WIDTH as f64)
            .round()
            .clamp(0.0, BAR_WIDTH as f64) as usize
    } else {
        0
    };
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

/// Neutralize a string for a GitHub-flavored-Markdown table cell. A literal `|` would open a new
/// column and any newline a new row, so a field value carrying either — a `tool_name`, a project
/// label from a directory name, a git branch used as a group key — would otherwise break or inject
/// into the table. The backslash is escaped first (it is GFM's own escape leader, so a pre-existing
/// `\` before a `|` would otherwise form an even run that leaves the pipe live); then `|` is
/// backslash-escaped and every control character (newlines included) collapses to a space. Stored
/// data is untouched — this is render-only.
pub fn escape_md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Default fractional digits, and the cap on widening them for a small magnitude.
const DECIMALS: usize = 2;
const MAX_DECIMALS: usize = 12;

/// Digit-grouped decimal. Sums run to nine figures — `770991471` and `770,991,471` carry the same
/// information but only one can be read at a glance — and grouping is lossless, so it says nothing
/// about a value's unit, which a Kind does not declare.
///
/// A magnitude too small for the default precision widens to its first significant digit instead
/// of rounding away: a fraction of a cent that was really spent must not print as the `0` of a
/// project that spent nothing.
fn fmt_num(v: f64) -> String {
    let plain = if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.*}", significant_decimals(v))
    };
    group_digits(&plain)
}

fn significant_decimals(v: f64) -> usize {
    let magnitude = v.abs();
    if magnitude == 0.0 || magnitude >= 10f64.powi(-(DECIMALS as i32)) {
        return DECIMALS;
    }
    let leading_zeros = -magnitude.log10().floor() as usize;
    leading_zeros.saturating_add(1).min(MAX_DECIMALS)
}

fn group_digits(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s),
    };
    let (int, frac) = match rest.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (rest, None),
    };
    let mut out = String::with_capacity(s.len() + int.len() / 3);
    out.push_str(sign);
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(frac) = frac {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// How many terminal columns a string occupies. Not its character count: a CJK character — which
/// a Korean rule name or project directory is made of — takes two columns, so counting characters
/// would leave every column to its right short by one per such character.
fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Clip to `width` terminal columns, marking the clip with an ellipsis (itself one column). A
/// character is taken only while it fits whole, so a two-column character is never half-printed.
fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = UnicodeWidthStr::width(c.encode_utf8(&mut [0u8; 4]) as &str);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn pad_right(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(display_width(s));
    format!("{s}{}", " ".repeat(pad))
}

fn pad_left(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(display_width(s));
    format!("{}{s}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Filter, Measure};

    fn section(kind: &str, groups: Vec<GroupAgg>) -> KindSection {
        KindSection {
            kind: kind.to_string(),
            group_by: "tool_name".to_string(),
            sort_by: Some("duration_ms".to_string()),
            project_scope: ProjectScope::Unrestricted,
            groups,
        }
    }

    fn group(key: &str, count: i64, sum: f64) -> GroupAgg {
        GroupAgg {
            key: key.to_string(),
            count,
            sums: vec![Measure {
                name: "duration_ms".to_string(),
                sum,
            }],
        }
    }

    fn report(kinds: Vec<KindSection>) -> Report {
        Report {
            window: "30d".to_string(),
            project: None,
            filters: Vec::new(),
            top_n: 5,
            kinds,
            cost: Vec::new(),
            unreadable_kinds: None,
        }
    }

    #[test]
    fn escape_md_cell_neutralizes_pipes_and_newlines() {
        // A group key like a git branch `feat/a|b` or a multiline value must not add columns/rows.
        assert_eq!(escape_md_cell("feat/a|b"), "feat/a\\|b");
        assert_eq!(escape_md_cell("line1\nline2"), "line1 line2");
        assert_eq!(escape_md_cell("a\r\nb\tc"), "a  b c");
        // The backslash is escaped first, so a `\` before a `|` can't leave the pipe live: `a\|b`
        // becomes `a\\\|b` (literal backslash, then escaped pipe), and a Windows path stays intact.
        assert_eq!(escape_md_cell(r"a\|b"), r"a\\\|b");
        assert_eq!(escape_md_cell(r"c:\path"), r"c:\\path");
        assert_eq!(escape_md_cell("Bash"), "Bash");
    }

    #[test]
    fn a_hostile_group_key_stays_one_markdown_row() {
        let md = format_markdown(&report(vec![section(
            "tool",
            vec![group("a|b\nc", 1, 5.0)],
        )]));
        let row = md
            .lines()
            .find(|l| l.starts_with("| a"))
            .expect("the group row");
        assert!(!row.contains("a|b"), "raw pipe must be escaped: {row}");
        assert!(row.contains("a\\|b c"), "newline collapsed inline: {row}");
        assert!(row.ends_with(" |"), "row closes cleanly: {row}");
    }

    #[test]
    fn digits_are_grouped_without_changing_the_value() {
        assert_eq!(fmt_num(770_991_471.0), "770,991,471");
        assert_eq!(fmt_num(1234.5678), "1,234.57");
        assert_eq!(fmt_num(-1_234.0), "-1,234");
        assert_eq!(fmt_num(999.0), "999");
        assert_eq!(fmt_num(0.0), "0");
    }

    #[test]
    fn a_value_that_was_measured_never_prints_as_one_that_was_not() {
        // A fraction of a cent really spent must stay distinguishable from no spend at all.
        assert_eq!(fmt_num(0.0), "0");
        assert_ne!(fmt_num(0.0042), fmt_num(0.0));
        assert_eq!(fmt_num(0.0042), "0.0042");
        assert_eq!(fmt_num(0.00000031), "0.00000031");
        assert_eq!(fmt_num(-0.0042), "-0.0042");
        // A magnitude below the cap still says "not zero" rather than rounding to it.
        assert_ne!(fmt_num(1e-20), "0");
    }

    #[test]
    fn a_wide_key_leaves_the_columns_aligned() {
        // Group keys are project directories, git branches and plugin field values — Korean ones
        // occupy two terminal columns per character.
        let text = format_text(&report(vec![section(
            "k",
            vec![
                group("한국어규칙이름", 1, 100.0),
                group("ascii-rule", 1, 90.0),
            ],
        )]));
        let cols = |line: &str, needle: &str| {
            let byte = line.find(needle).unwrap();
            UnicodeWidthStr::width(&line[..byte])
        };
        let wide = text.lines().find(|l| l.contains("한국어")).unwrap();
        let ascii = text.lines().find(|l| l.contains("ascii-rule")).unwrap();
        assert_eq!(
            cols(wide, "100") + 3,
            cols(ascii, "90") + 2,
            "measure columns end at the same terminal column:\n{wide}\n{ascii}"
        );
    }

    #[test]
    fn a_kind_that_records_no_project_says_so_instead_of_showing_nothing() {
        // The distinction the report exists to preserve: "this Kind cannot be project-scoped" is
        // not "this project did none of it".
        let mut s = section("aix.rules", Vec::new());
        s.project_scope = ProjectScope::Unsupported;
        let mut r = report(vec![s]);
        r.project = Some("acme".to_string());
        let text = format_text(&r);
        assert!(text.contains("records no project"), "{text}");
        assert!(!text.contains("no records in this window"), "{text}");
    }

    #[test]
    fn every_row_carries_a_bar_scaled_to_the_leader() {
        let text = format_text(&report(vec![section(
            "tool",
            vec![group("Bash", 9, 100.0), group("Edit", 2, 50.0)],
        )]));
        let bash = text.lines().find(|l| l.contains("Bash")).unwrap();
        let edit = text.lines().find(|l| l.contains("Edit")).unwrap();
        assert_eq!(bash.matches('█').count(), BAR_WIDTH, "leader fills the bar");
        assert_eq!(edit.matches('█').count(), BAR_WIDTH / 2, "half the leader");
        assert!(bash.contains("100"), "the measure is still printed: {bash}");
    }

    #[test]
    fn a_newline_in_a_field_name_cannot_break_the_layout() {
        // Field names come from a plugin's TOML, where a newline is expressible. One in the axes
        // line would end a Markdown heading early and misalign every terminal column below it.
        let mut s = section("k", vec![group("v", 1, 1.0)]);
        s.group_by = "rule\nname".to_string();
        for out in [
            format_text(&report(vec![s.clone()])),
            format_markdown(&report(vec![s])),
        ] {
            let axes = out.lines().find(|l| l.contains("rule")).unwrap();
            assert!(axes.contains("rule name"), "collapsed inline: {axes:?}");
        }
    }

    #[test]
    fn markdown_headers_are_escaped_like_values() {
        // A field name is written by a plugin author and is not character-restricted, so a pipe in
        // one would open a column on the header row that no data row has.
        let mut s = section("k", vec![group("v", 1, 1.0)]);
        s.group_by = "rule|name".to_string();
        let md = format_markdown(&report(vec![s]));
        let header = md.lines().find(|l| l.starts_with("| rule")).unwrap();
        assert!(header.contains("rule\\|name"), "{header}");
        assert_eq!(
            header.matches(" | ").count(),
            2,
            "3 columns, not 4: {header}"
        );
    }

    #[test]
    fn an_empty_group_key_is_named_not_blank() {
        // A record that stored an empty value must not render as an unlabelled row, and must stay
        // distinct from the em-dash marking a record that carried no such field at all.
        let text = format_text(&report(vec![section(
            "aix.sessions",
            vec![group("", 15, 1.0), group("—", 2, 1.0)],
        )]));
        assert!(text.contains("(empty)"), "{text}");
        assert!(text.contains('—'), "{text}");
    }

    #[test]
    fn bars_measure_what_the_rows_are_ordered_by() {
        // With the ranking measure declared second, a bar drawn from the leading measure would
        // contradict the order the rows are in — the leader's bar would not be the longest.
        let measured = |key: &str, first: f64, second: f64| GroupAgg {
            key: key.to_string(),
            count: 1,
            sums: vec![
                Measure {
                    name: "evaluations".to_string(),
                    sum: first,
                },
                Measure {
                    name: "violations".to_string(),
                    sum: second,
                },
            ],
        };
        let mut s = section(
            "aix.rules",
            vec![
                measured("citation", 21_000.0, 200.0),
                measured("imports", 13_000_000.0, 100.0),
            ],
        );
        s.sort_by = Some("violations".to_string());
        let text = format_text(&report(vec![s]));
        let citation = text.lines().find(|l| l.contains("citation")).unwrap();
        let imports = text.lines().find(|l| l.contains("imports")).unwrap();
        assert_eq!(citation.matches('█').count(), BAR_WIDTH);
        assert_eq!(imports.matches('█').count(), BAR_WIDTH / 2);
    }

    #[test]
    fn columns_line_up_under_a_non_ascii_header() {
        // Widths and padding must be counted in the same unit — terminal columns — or a header
        // that is not plain ASCII shifts every column to its right.
        let mut s = section("k", vec![group("v", 1, 1.0)]);
        s.group_by = "규칙".to_string();
        let text = format_text(&report(vec![s]));
        let mut lines = text.lines().skip_while(|l| !l.starts_with("k —")).skip(1);
        let header = lines.next().unwrap();
        let row = lines.next().unwrap();
        let column =
            |line: &str, needle: &str| UnicodeWidthStr::width(&line[..line.find(needle).unwrap()]);
        assert_eq!(column(header, "규칙"), column(row, "v"));
        assert_eq!(
            column(header, "count") + "count".len(),
            column(row, "1") + 1
        );
    }

    #[test]
    fn columns_line_up_under_their_headers() {
        let text = format_text(&report(vec![section(
            "tool",
            vec![group("Bash", 155_637, 770_991_471.0)],
        )]));
        let mut lines = text
            .lines()
            .skip_while(|l| !l.starts_with("tool —"))
            .skip(1);
        let header = lines.next().unwrap();
        let row = lines.next().unwrap();
        // Column, not byte offset — a bar cell is one column and three bytes.
        let column = |line: &str, needle: &str| {
            line.find(needle)
                .map(|byte| line[..byte].chars().count())
                .unwrap_or_else(|| panic!("{needle:?} not in {line:?}"))
        };
        assert_eq!(
            column(header, "tool_name"),
            column(row, "Bash"),
            "the dimension header sits over its keys:\n{header}\n{row}"
        );
        assert_eq!(
            column(header, "duration_ms") + "duration_ms".len(),
            column(row, "770,991,471") + "770,991,471".len(),
            "numeric columns are right-aligned under their headers:\n{header}\n{row}"
        );
    }

    #[test]
    fn the_header_states_the_scope_it_covers() {
        let mut r = report(vec![section("tool", vec![group("Bash", 1, 1.0)])]);
        r.project = Some("acme".to_string());
        r.filters = vec![Filter {
            field: "spec".to_string(),
            value: "auth".to_string(),
        }];
        assert!(
            format_text(&r).starts_with("=== hatel — rolling 30d — project acme — spec=auth ===")
        );
        assert!(
            format_markdown(&r).starts_with("# hatel — rolling 30d — project acme — spec=auth")
        );
    }
}
