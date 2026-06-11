//! End-to-end tests for the core pipeline: registry build, declarative field maps,
//! sanitization, the JSONL sink, the session index, and windowed reads — all driven
//! through explicit temp-dir configs so they run in parallel without shared state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hatel_core::registry::FieldMap;
use hatel_core::schema::{build_registry, load_core};
use hatel_core::{Config, Payload, SessionIndex, SinkKind, make_envelope, report};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ht-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config(plugins: Vec<PathBuf>) -> Config {
    let dir = temp_dir();
    Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.join("ledger"),
        state_dir: dir,
        plugins,
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
    }
}

fn example_plugin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/example.toml")
}

/// A config rooted at an explicit dir, so a test can write a plugin file into it.
fn config_in(dir: PathBuf, plugins: Vec<PathBuf>) -> Config {
    Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.join("ledger"),
        state_dir: dir,
        plugins,
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
    }
}

#[test]
fn core_registry_is_self_consistent() {
    let reg = load_core().expect("core schema loads");
    for spec in reg.kinds() {
        assert!(
            spec.fields.contains(&spec.group_key),
            "group_key in fields for {}",
            spec.name
        );
        for r in &spec.redact {
            assert!(
                spec.fields.contains(r),
                "redact field in fields for {}",
                spec.name
            );
        }
        for m in &spec.measures {
            assert!(
                spec.fields.contains(m),
                "measure in fields for {}",
                spec.name
            );
        }
    }
    assert!(reg.kind("tool").is_some());
    assert!(reg.kind("memory").is_some());
    assert!(reg.kind("compaction").is_some());
}

#[test]
fn example_plugin_merges_cleanly() {
    let reg = build_registry(&test_config(vec![example_plugin()])).expect("example plugin loads");
    assert!(reg.kind("branch_work").is_some());
    let ci = reg.kind("ci_check").unwrap();
    assert_eq!(ci.group_key, "check");
    assert_eq!(
        ci.measures,
        vec!["runs".to_string(), "failures".to_string()]
    );
    assert!(ci.redact.contains("actor"));
}

#[test]
fn emit_redacts_declared_field() {
    let reg = build_registry(&test_config(vec![example_plugin()])).unwrap();
    let mut payload = Payload::new();
    payload.insert("check".to_string(), "lint".into());
    payload.insert("actor".to_string(), "alice@example.com".into());
    let env = make_envelope("ci_check", payload, &reg, false).unwrap();
    let stored = env.payload.get("actor").and_then(|v| v.as_str()).unwrap();
    assert_ne!(
        stored, "alice@example.com",
        "raw identity must never be stored"
    );
    assert_eq!(stored.len(), 16, "redacted to a 16-hex blake3 hash");
}

#[test]
fn emit_payload_is_allow_list_filtered() {
    // make_envelope is exactly what `hatel emit` runs.
    let reg = build_registry(&test_config(vec![example_plugin()])).unwrap();
    let mut payload = Payload::new();
    payload.insert("check".to_string(), "lint".into());
    payload.insert("runs".to_string(), serde_json::json!(14000));
    payload.insert("failures".to_string(), serde_json::json!(3));
    payload.insert("secret".to_string(), "leak".into()); // outside the allow-list
    let env = make_envelope("ci_check", payload, &reg, false).unwrap();
    assert_eq!(
        env.payload.get("runs").and_then(|v| v.as_i64()),
        Some(14000)
    );
    assert!(
        !env.payload.contains_key("secret"),
        "non-allowed key dropped on emit"
    );
}

#[test]
fn duplicate_kind_is_a_hard_error() {
    let err = build_registry(&test_config(vec![example_plugin(), example_plugin()])).unwrap_err();
    assert!(format!("{err}").contains("duplicate"), "got: {err}");
}

#[test]
fn hook_binding_to_a_receiver_sourced_kind_is_rejected() {
    // `tool` is written by the receiver from the native `tool_result` event. A plugin that also
    // hook-binds it would double-write the Kind — the registry must reject the binding loudly,
    // rather than silently produce two records per tool call.
    let dir = temp_dir();
    let plugin = dir.join("dbl.toml");
    std::fs::write(
        &plugin,
        "[[binding]]\nevent = \"PostToolUse\"\nkind = \"tool\"\nmap.session_id = { from = \"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("receiver-sourced"), "got: {err}");
}

#[test]
fn a_plugin_kind_cannot_declare_receiver_sourced() {
    // `receiver_sourced` is core-only: the receiver writes only Kinds it has a native handler for,
    // so a plugin declaring it would create a Kind nothing ever writes (and that can't be
    // hook-bound either) — a dead extension point. Reject it loudly at load.
    let dir = temp_dir();
    let plugin = dir.join("rs.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname = \"team.native\"\nfields = [\"session_id\"]\ngroup_key = \"session_id\"\nreceiver_sourced = true\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("core-only"), "got: {err}");
}

#[test]
fn tool_kind_allow_list_keeps_it_content_free() {
    // The `tool` Kind is written by the receiver from the native `tool_result` event, which also
    // carries the user's email and full tool input. Its field allow-list is the guard that keeps
    // the ledger content-free: anything outside duration/outcome identity is dropped at envelope
    // time, so PII and tool content can never reach the sink.
    let reg = load_core().unwrap();
    let mut payload = Payload::new();
    payload.insert("session_id".to_string(), "S1".into());
    payload.insert("project".to_string(), "myproj".into());
    payload.insert("tool_name".to_string(), "Bash".into());
    payload.insert("duration_ms".to_string(), 23.into());
    payload.insert("ok".to_string(), 1.into());
    payload.insert("user.email".to_string(), "a@b.com".into()); // PII — outside the allow-list
    payload.insert("tool_input".to_string(), "rm -rf /".into()); // content — outside the allow-list
    let env = make_envelope("tool", payload, &reg, false).unwrap();
    assert_eq!(
        env.payload.get("tool_name").and_then(|v| v.as_str()),
        Some("Bash")
    );
    assert_eq!(
        env.payload.get("duration_ms").and_then(|v| v.as_i64()),
        Some(23)
    );
    assert_eq!(env.payload.get("ok").and_then(|v| v.as_i64()), Some(1));
    assert!(!env.payload.contains_key("user.email"), "PII dropped");
    assert!(
        !env.payload.contains_key("tool_input"),
        "tool content dropped"
    );
}

#[test]
fn prompt_stores_length_not_text() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "S2", "cwd": "/tmp/x",
        "prompt": "hello world"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "prompt", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("prompt_len").and_then(|v| v.as_i64()),
        Some(11)
    );
    assert!(!recs[0].payload.contains_key("prompt"));
}

#[test]
fn session_start_is_recorded_in_the_index() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "S3", "cwd": "/tmp/myproj"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let index = SessionIndex::new(cfg.state_dir.clone()).load();
    let row = index.get("S3").expect("session recorded");
    assert_eq!(row.project_label, "myproj");
    assert_eq!(row.project_key, "/tmp/myproj");
}

#[test]
fn field_map_capture_derives_value_or_omits() {
    let fm: FieldMap = toml::from_str("from = \"git_branch\"\ncapture = \"^spec/(.+)$\"").unwrap();
    let matched = serde_json::json!({"git_branch": "spec/my-feature"});
    assert_eq!(
        fm.apply(&matched),
        Some(serde_json::Value::from("my-feature"))
    );
    // No match → field omitted, never fabricated.
    let unmatched = serde_json::json!({"git_branch": "main"});
    assert_eq!(fm.apply(&unmatched), None);
    // Source absent → omitted.
    assert_eq!(fm.apply(&serde_json::json!({})), None);
}

#[test]
fn field_map_tries_multiple_source_keys() {
    let fm: FieldMap = toml::from_str("from = [\"trigger\", \"compact_trigger\"]").unwrap();
    // first key present wins
    assert_eq!(
        fm.apply(&serde_json::json!({"trigger": "auto"})),
        Some("auto".into())
    );
    // falls back to the second key
    assert_eq!(
        fm.apply(&serde_json::json!({"compact_trigger": "manual"})),
        Some("manual".into())
    );
    // neither present → omitted, never fabricated
    assert_eq!(fm.apply(&serde_json::json!({"other": "x"})), None);
}

#[test]
fn compaction_records_trigger_with_either_field_name() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "PreCompact", "session_id": "S", "cwd": "/tmp/x",
        "compact_trigger": "auto"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "compaction", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("trigger").and_then(|v| v.as_str()),
        Some("auto")
    );
}

#[test]
fn git_branch_is_injected_only_when_a_binding_uses_it() {
    let dir = temp_dir();
    let plugin = dir.join("branch.toml");
    std::fs::write(
        &plugin,
        r#"
[[kind]]
name = "work"
fields = ["session_id", "project", "spec_slug"]
group_key = "spec_slug"
[[binding]]
event = "SessionEnd"
kind = "work"
map.session_id = { from = "session_id" }
map.spec_slug = { from = "git_branch", capture = "^spec/(.+)$" }
"#,
    )
    .unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(
        repo.join(".git").join("HEAD"),
        "ref: refs/heads/spec/checkout\n",
    )
    .unwrap();
    let cfg = Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.join("ledger"),
        state_dir: dir.clone(),
        plugins: vec![plugin],
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
    };
    let reg = build_registry(&cfg).unwrap();

    // SessionEnd binding references git_branch → it is injected and spec_slug captured.
    let mut end = serde_json::json!({
        "hook_event_name": "SessionEnd", "session_id": "S", "cwd": repo.to_str().unwrap()
    });
    hatel_core::hook::process_event(&mut end, &cfg, &reg);
    assert_eq!(
        end.get("git_branch").and_then(|v| v.as_str()),
        Some("spec/checkout")
    );
    let recs = hatel_core::sink::read_records(&cfg, "work", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("spec_slug").and_then(|v| v.as_str()),
        Some("checkout")
    );

    // PostToolUse has no git_branch-referencing binding → the branch is never read.
    let mut tool = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "S", "cwd": repo.to_str().unwrap(), "tool_name": "Bash"
    });
    hatel_core::hook::process_event(&mut tool, &cfg, &reg);
    assert!(
        tool.get("git_branch").is_none(),
        "git_branch not read when no binding uses it"
    );
}

#[test]
fn strict_mode_rejects_unknown_keys() {
    let reg = load_core().unwrap();
    let mut payload = Payload::new();
    payload.insert("session_id".to_string(), "X".into());
    payload.insert("tool_name".to_string(), "Bash".into());
    payload.insert("secret".to_string(), "leak".into());
    let err = make_envelope("tool", payload, &reg, true).unwrap_err();
    assert!(format!("{err}").contains("disallowed"), "got: {err}");
}

#[test]
fn reports_read_active_and_rotated_archives() {
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    std::fs::write(
        cfg.ledger_dir.join("tool.jsonl"),
        format!("{}\n", line("A")),
    )
    .unwrap();
    std::fs::write(
        cfg.ledger_dir.join("tool.jsonl.20240101"),
        format!("{}\n", line("B")),
    )
    .unwrap();
    let recs = hatel_core::sink::read_records(&cfg, "tool", None);
    assert_eq!(recs.len(), 2);
}

#[test]
fn cost_snapshot_merges_by_session() {
    use hatel_core::cost::{self, CostRow};
    let cfg = test_config(vec![]);
    let row = |sid: &str, tokens: i64| CostRow {
        session_id: sid.to_string(),
        project: "p".to_string(),
        tokens,
        cost_usd: 0.0,
        active_time_s: 0.0,
        lines: 0,
        ts: "2024-01-01T00:00:00Z".to_string(),
    };
    cost::merge_snapshot(&cfg.state_dir, vec![row("S1", 10), row("S2", 5)], 0);
    cost::merge_snapshot(&cfg.state_dir, vec![row("S1", 99)], 0); // update S1, keep S2 (retain all)
    let mut rows = cost::read_snapshot(&cfg.state_dir);
    rows.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tokens, 99, "S1 updated");
    assert_eq!(rows[1].tokens, 5, "S2 preserved across merge");
}

#[test]
fn merge_with_no_rows_still_prunes_stale_entries() {
    // An idle flush (no active sessions) must still prune old rows, so a quiet receiver
    // can't let stale prior-run entries linger forever.
    use hatel_core::cost::{self, CostRow};
    let cfg = test_config(vec![]);
    let old = CostRow {
        session_id: "old".to_string(),
        project: "p".to_string(),
        tokens: 1,
        cost_usd: 0.0,
        active_time_s: 0.0,
        lines: 0,
        ts: "2000-01-01T00:00:00Z".to_string(),
    };
    cost::merge_snapshot(&cfg.state_dir, vec![old], 0); // seed (retain all)
    assert_eq!(cost::read_snapshot(&cfg.state_dir).len(), 1);
    let cutoff = hatel_core::now_iso_utc()
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_second();
    cost::merge_snapshot(&cfg.state_dir, vec![], cutoff); // idle flush, but prunes
    assert!(
        cost::read_snapshot(&cfg.state_dir).is_empty(),
        "stale row pruned on empty merge"
    );
}

#[test]
fn cost_snapshot_prunes_rows_past_retention() {
    use hatel_core::cost::{self, CostRow};
    let cfg = test_config(vec![]);
    let row = |sid: &str, ts: &str| CostRow {
        session_id: sid.to_string(),
        project: "p".to_string(),
        tokens: 1,
        cost_usd: 0.0,
        active_time_s: 0.0,
        lines: 0,
        ts: ts.to_string(),
    };
    let now = hatel_core::now_iso_utc();
    cost::merge_snapshot(
        &cfg.state_dir,
        vec![row("old", "2000-01-01T00:00:00Z"), row("recent", &now)],
        0,
    );
    assert_eq!(
        cost::read_snapshot(&cfg.state_dir).len(),
        2,
        "both retained at retain_since=0"
    );
    // A retain_since of "one day ago" drops the year-2000 row, keeps the recent one.
    let cutoff = now.parse::<jiff::Timestamp>().unwrap().as_second() - 86_400;
    cost::merge_snapshot(&cfg.state_dir, vec![], cutoff);
    let rows = cost::read_snapshot(&cfg.state_dir);
    assert_eq!(rows.len(), 1, "old row pruned");
    assert_eq!(rows[0].session_id, "recent");
}

#[test]
fn compaction_writes_one_record_per_compaction() {
    // Both PreCompact and PostCompact fire for one compaction; only PreCompact is
    // bound, so the ledger gets exactly one record — no double-count.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for name in ["PreCompact", "PostCompact"] {
        let mut ev = serde_json::json!({
            "hook_event_name": name, "session_id": "S", "cwd": "/tmp/x", "compact_trigger": "auto"
        });
        hatel_core::hook::process_event(&mut ev, &cfg, &reg);
    }
    assert_eq!(
        hatel_core::sink::read_records(&cfg, "compaction", None).len(),
        1
    );
}

#[test]
fn binding_writing_a_non_allowlisted_field_is_rejected_at_build() {
    let dir = temp_dir();
    let plugin = dir.join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.typo={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("not in the kind's fields"),
        "got: {err}"
    );
}

#[test]
fn invalid_capture_regex_is_rejected_at_build() {
    let dir = temp_dir();
    let plugin = dir.join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.spec={ from=\"git_branch\", capture=\"([\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("invalid capture regex"),
        "got: {err}"
    );
}

#[test]
fn unsafe_kind_name_is_rejected() {
    let dir = temp_dir();
    let plugin = dir.join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"../escape\"\nfields=[\"a\"]\ngroup_key=\"a\"\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("[A-Za-z0-9._-]"), "got: {err}");
}

#[test]
fn parse_window_rejects_overflow_and_nonsense() {
    assert!(report::parse_window("30d").is_some());
    assert!(report::parse_window("0d").is_none());
    assert!(report::parse_window("7h").is_none());
    assert!(report::parse_window("999999999999999999d").is_none()); // would overflow
}

#[test]
fn report_sums_measures_and_coerces_numeric_strings() {
    let cfg = test_config(vec![example_plugin()]);
    let reg = build_registry(&cfg).unwrap();
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let rec = |runs: serde_json::Value, fails: i64| {
        serde_json::json!({
            "ts": hatel_core::now_iso_utc(), "kind": "ci_check", "_schema_version": 1,
            "payload": {"check": "lint", "runs": runs, "failures": fails}
        })
        .to_string()
    };
    std::fs::write(
        cfg.ledger_dir.join("ci_check.jsonl"),
        format!(
            "{}\n{}\n",
            rec(serde_json::json!(14000), 3),
            rec(serde_json::json!("1000"), 2)
        ),
    )
    .unwrap();
    let groups = report::aggregate(&reg, &cfg, "ci_check", 0, 5, None);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key, "lint");
    assert_eq!(groups[0].count, 2);
    // runs: 14000 (number) + "1000" (numeric string, coerced) = 15000
    assert_eq!(groups[0].sums[0].name, "runs");
    assert_eq!(groups[0].sums[0].sum, 15000.0);
    assert_eq!(groups[0].sums[1].name, "failures");
    assert_eq!(groups[0].sums[1].sum, 5.0);
}

#[test]
fn report_filters_by_project() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let rec = |proj: &str, tool: &str| {
        serde_json::json!({
            "ts": hatel_core::now_iso_utc(), "kind": "tool", "_schema_version": 1,
            "payload": {"session_id": "S", "project": proj, "tool_name": tool}
        })
        .to_string()
    };
    std::fs::write(
        cfg.ledger_dir.join("tool.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            rec("alpha", "Bash"),
            rec("alpha", "Edit"),
            rec("beta", "Bash")
        ),
    )
    .unwrap();
    let total = |p: Option<&str>| -> i64 {
        report::aggregate(&reg, &cfg, "tool", 0, 5, p)
            .iter()
            .map(|g| g.count)
            .sum()
    };
    assert_eq!(total(None), 3, "all projects");
    assert_eq!(total(Some("alpha")), 2, "only alpha");
    assert_eq!(total(Some("beta")), 1, "only beta");
}

#[test]
fn binding_mapping_project_is_rejected() {
    let dir = temp_dir();
    let plugin = dir.join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"project\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.project={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("may not map 'project'"),
        "got: {err}"
    );
}

#[test]
fn field_map_with_two_transforms_is_rejected() {
    let dir = temp_dir();
    let plugin = dir.join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"n\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.session_id={ from=\"session_id\" }\nmap.n={ from=\"p\", len=true, basename=true }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("at most one"), "got: {err}");
}

#[test]
fn field_map_transform_without_source_is_rejected() {
    let dir = temp_dir();
    let plugin = dir.join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"n\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.session_id={ from=\"session_id\" }\nmap.n={ len=true }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("needs a `from`"), "got: {err}");
}

#[test]
fn capture_regex_without_a_group_is_rejected() {
    let dir = temp_dir();
    let plugin = dir.join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.spec={ from=\"git_branch\", capture=\"spec/.+\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("needs a group"), "got: {err}");
}

#[test]
fn measures_reject_non_finite_values() {
    let cfg = test_config(vec![example_plugin()]);
    let reg = build_registry(&cfg).unwrap();
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let rec = |runs: serde_json::Value| {
        serde_json::json!({
            "ts": hatel_core::now_iso_utc(), "kind": "ci_check", "_schema_version": 1,
            "payload": {"check": "lint", "runs": runs}
        })
        .to_string()
    };
    // an `inf` string must contribute 0 — never poison the sum.
    std::fs::write(
        cfg.ledger_dir.join("ci_check.jsonl"),
        format!(
            "{}\n{}\n",
            rec(serde_json::json!("inf")),
            rec(serde_json::json!(50))
        ),
    )
    .unwrap();
    let groups = report::aggregate(&reg, &cfg, "ci_check", 0, 5, None);
    assert_eq!(groups[0].sums[0].sum, 50.0, "inf rejected, only 50 summed");
}

#[test]
fn duplicate_event_kind_binding_is_rejected() {
    // Two bindings for the same (event, kind) would write two records per fire.
    let dir = temp_dir();
    let plugin = dir.join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.session_id={ from=\"session_id\" }\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.session_id={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir, vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("already has a binding for kind"),
        "got: {err}"
    );
}

#[test]
fn sqlite_window_filter_excludes_old_records() {
    // The SQLite reader pushes the time window into SQL; an out-of-window row is excluded.
    let dir = temp_dir();
    let db = dir.join("telemetry.db");
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir, vec![])
    };
    let reg = load_core().unwrap();
    // recent record via the real write path
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    // an ancient record inserted directly
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload) VALUES (?1,'prompt',1,'{}')",
            ["2000-01-01T00:00:00Z"],
        )
        .unwrap();
    }
    let all = hatel_core::sink::read_records(&cfg, "prompt", None);
    assert_eq!(all.len(), 2, "both rows present");
    let recent_epoch = hatel_core::now_iso_utc()
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_second()
        - 86_400;
    let windowed = hatel_core::sink::read_records(&cfg, "prompt", Some(recent_epoch));
    assert_eq!(windowed.len(), 1, "ancient row excluded by SQL window");
}

#[test]
fn sqlite_window_keeps_records_in_the_cutoff_second() {
    // A stored ts carries a fraction (`...:20.5Z`); a whole-second cutoff in the SAME
    // second must NOT drop it (the SQL pre-filter must be a safe superset).
    let dir = temp_dir();
    let db = dir.join("telemetry.db");
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir, vec![])
    };
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    // cutoff = the exact second of the record we just wrote.
    let recs = hatel_core::sink::read_records(&cfg, "prompt", None);
    let secs = recs[0].ts.parse::<jiff::Timestamp>().unwrap().as_second();
    let kept = hatel_core::sink::read_records(&cfg, "prompt", Some(secs));
    assert_eq!(
        kept.len(),
        1,
        "same-second record kept by the SQL superset filter"
    );
    let _ = db;
}

#[test]
fn sqlite_sink_round_trips_through_report() {
    // The storage abstraction is honest: a report reads the SQLite sink exactly as it
    // does JSONL, so the SQLite backend is fully usable (not write-only).
    let dir = temp_dir();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir, vec![])
    };
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);

    let recs = hatel_core::sink::read_records(&cfg, "prompt", None);
    assert_eq!(recs.len(), 1, "record read back from sqlite");
    assert_eq!(
        recs[0].payload.get("prompt_len").and_then(|v| v.as_i64()),
        Some(2)
    );
    let groups = report::aggregate(&reg, &cfg, "prompt", 0, 5, None);
    assert_eq!(
        groups.iter().map(|g| g.count).sum::<i64>(),
        1,
        "report aggregates sqlite records"
    );
}

#[test]
fn top_zero_means_all_groups() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let rec = |tool: &str| {
        serde_json::json!({
            "ts": hatel_core::now_iso_utc(), "kind": "tool", "_schema_version": 1,
            "payload": {"session_id": "S", "project": "p", "tool_name": tool}
        })
        .to_string()
    };
    let body: String = ["Bash", "Edit", "Read", "Write", "Grep", "Glob"]
        .iter()
        .map(|t| format!("{}\n", rec(t)))
        .collect();
    std::fs::write(cfg.ledger_dir.join("tool.jsonl"), body).unwrap();
    assert_eq!(
        report::aggregate(&reg, &cfg, "tool", 0, 3, None).len(),
        3,
        "capped at 3"
    );
    assert_eq!(
        report::aggregate(&reg, &cfg, "tool", 0, 0, None).len(),
        6,
        "0 = all 6"
    );
}

#[test]
fn records_newer_than_this_schema_version_are_skipped() {
    // A future-version record must not be mis-aggregated by today's build.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let v1 = serde_json::json!({
        "ts": hatel_core::now_iso_utc(), "kind": "tool", "_schema_version": 1,
        "payload": {"session_id": "S", "project": "p", "tool_name": "Bash"}
    });
    let v999 = serde_json::json!({
        "ts": hatel_core::now_iso_utc(), "kind": "tool", "_schema_version": 999,
        "payload": {"session_id": "S", "project": "p", "tool_name": "FromTheFuture"}
    });
    std::fs::write(cfg.ledger_dir.join("tool.jsonl"), format!("{v1}\n{v999}\n")).unwrap();
    let recs = hatel_core::sink::read_records(&cfg, "tool", None);
    assert_eq!(recs.len(), 1, "only the v1 record is read");
    assert_eq!(
        recs[0].payload.get("tool_name").and_then(|v| v.as_str()),
        Some("Bash")
    );
}

fn line(tool: &str) -> String {
    serde_json::json!({
        "ts": hatel_core::now_iso_utc(),
        "kind": "tool",
        "_schema_version": 1,
        "payload": {"session_id": "S", "project": "p", "tool_name": tool}
    })
    .to_string()
}
