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
        plugin_source: hatel_core::config::PluginSource::ConfigFile,
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
    }
}

fn example_plugin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/example.toml")
}

/// An unrestricted query over everything since `since`, showing `top_n` groups per Kind.
fn query(since: i64, top_n: usize, project: Option<&str>) -> report::Query<'_> {
    report::Query {
        since,
        top_n,
        project,
        kind: None,
        group_by: None,
        sort_by: None,
        filters: &[],
    }
}

/// A config rooted at an explicit dir, so a test can write a plugin file into it.
fn config_in(dir: PathBuf, plugins: Vec<PathBuf>) -> Config {
    Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.join("ledger"),
        state_dir: dir,
        plugins,
        plugin_source: hatel_core::config::PluginSource::ConfigFile,
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
fn bound_events_surfaces_an_out_of_vocabulary_binding() {
    // Core accepts a binding to any event string (it doesn't own the wiring vocabulary); the
    // registry surfaces every bound event so the CLI can flag one it can't wire, rather than let
    // a binding to e.g. `PreToolUse` load cleanly and then silently never fire.
    let dir = temp_dir();
    let plugin = dir.join("oov.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname = \"team.pre\"\nfields = [\"session_id\"]\ngroup_key = \"session_id\"\n\
         [[binding]]\nevent = \"PreToolUse\"\nkind = \"team.pre\"\nmap.session_id = { from = \"session_id\" }\n",
    )
    .unwrap();
    let reg = build_registry(&config_in(dir, vec![plugin])).unwrap();
    let bound: Vec<&str> = reg.bound_events().collect();
    assert!(
        bound.contains(&"PreToolUse"),
        "bound events include the out-of-vocab one: {bound:?}"
    );
}

#[test]
fn tool_kind_allow_list_keeps_it_content_free() {
    // The `tool` Kind is written from the tool-completion hooks, whose envelopes also
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
    let repo = temp_dir().join("myproj");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "S3", "cwd": repo.to_str().unwrap()
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let index = SessionIndex::new(cfg.state_dir.clone()).load();
    let row = index.get("S3").expect("session recorded");
    assert_eq!(row.project_label, "myproj");
    assert_eq!(
        std::path::Path::new(&row.project_key),
        std::fs::canonicalize(&repo).unwrap()
    );
}

#[test]
fn an_unattributable_session_start_is_recorded_without_a_project() {
    // A session outside a repository — or one whose cwd names no directory this process can
    // resolve — has no project, and is recorded as having none. The receiver reads that as an
    // answer: a session with no project is decided, unlike one whose start it has not seen, which
    // is the only kind worth holding an egress batch back for.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let outside = temp_dir();
    for (session_id, cwd) in [("S4", ""), ("S5", outside.to_str().unwrap())] {
        let mut event = serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": session_id, "cwd": cwd
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let index = SessionIndex::new(cfg.state_dir.clone()).load();
    let mut cache = hatel_core::SessionIndexCache::new(cfg.state_dir.clone());
    cache.refresh();
    for session_id in ["S4", "S5"] {
        assert!(
            index.contains_key(session_id),
            "the session start is recorded"
        );
        assert!(cache.is_unattributed(session_id), "with no project");
        assert_eq!(cache.label(session_id), None, "and nothing to attribute by");
    }
    assert!(
        !cache.contains("never-started"),
        "a session never seen stays undecided"
    );
}

#[test]
fn an_unattributable_session_records_the_empty_project_label() {
    // The label a Kind record carries for such a session is the one the cost snapshot already
    // stores for a session it could not attribute, so both describe it the same way rather than
    // the record naming the directory the work happened to run in.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let outside = temp_dir();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "S6", "cwd": outside.to_str().unwrap(), "prompt": "hello"
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "prompt", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("project").and_then(|v| v.as_str()),
        Some(""),
        "no directory lends its name to an unattributable session"
    );
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
        plugin_source: hatel_core::config::PluginSource::ConfigFile,
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
        cfg.ledger_dir.join("tool.jsonl.20240101.1"),
        format!("{}\n", line("B")),
    )
    .unwrap();
    let recs = hatel_core::sink::read_records(&cfg, "tool", None);
    assert_eq!(recs.len(), 2);
}

#[test]
fn report_filter_restricts_to_matching_records() {
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let rec = |tool: &str, ok: i64| {
        serde_json::json!({
            "ts": hatel_core::now_iso_utc(), "kind": "tool", "_schema_version": 1,
            "payload": {"session_id": "S", "project": "p", "tool_name": tool,
                        "duration_ms": 5, "ok": ok}
        })
        .to_string()
    };
    std::fs::write(
        cfg.ledger_dir.join("tool.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            rec("Bash", 1),
            rec("Bash", 0),
            rec("Edit", 1)
        ),
    )
    .unwrap();
    // A string field filters to exactly its rows.
    let filters = vec![("tool_name".to_string(), "Bash".to_string())];
    let q = report::Query {
        filters: &filters,
        ..query(0, 0, None)
    };
    let groups = report::aggregate(&reg, &cfg, "tool", &q);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key.as_deref(), Some("Bash"));
    assert_eq!(groups[0].count, 2);
    // A numeric field matches its rendered form, and multiple filters AND-combine.
    let filters = vec![
        ("tool_name".to_string(), "Bash".to_string()),
        ("ok".to_string(), "1".to_string()),
    ];
    let q = report::Query {
        filters: &filters,
        ..query(0, 0, None)
    };
    let groups = report::aggregate(&reg, &cfg, "tool", &q);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, 1, "both filters must match");
    // A value matching nothing yields an empty report — not an error, not all rows.
    let filters = vec![("tool_name".to_string(), "Nope".to_string())];
    let q = report::Query {
        filters: &filters,
        ..query(0, 0, None)
    };
    assert!(report::aggregate(&reg, &cfg, "tool", &q).is_empty());
}

#[test]
fn a_redacted_field_is_filterable_by_its_stored_form() {
    // The write path hashes a redacted field; a query carrying the same hash (what the CLI's
    // `--filter` computes from the original value) must match the stored record, while the raw
    // value — which never reached the ledger — must match nothing.
    let cfg = test_config(vec![example_plugin()]);
    let reg = build_registry(&cfg).unwrap();
    let mut payload = Payload::new();
    payload.insert("check".into(), "lint".into());
    payload.insert("actor".into(), "alice@example.com".into());
    let env = make_envelope("ci_check", payload, &reg, false).unwrap();
    let mut sink = hatel_core::build_sink(&cfg);
    sink.write_record(&env);
    sink.flush();
    let stored = vec![(
        "actor".to_string(),
        hatel_core::pii::redacted("alice@example.com"),
    )];
    let q = report::Query {
        filters: &stored,
        ..query(0, 0, None)
    };
    let groups = report::aggregate(&reg, &cfg, "ci_check", &q);
    assert_eq!(
        groups.iter().map(|g| g.count).sum::<i64>(),
        1,
        "stored form matches the redacted record"
    );
    let raw = vec![("actor".to_string(), "alice@example.com".to_string())];
    let q = report::Query {
        filters: &raw,
        ..query(0, 0, None)
    };
    assert!(
        report::aggregate(&reg, &cfg, "ci_check", &q).is_empty(),
        "the raw value never exists on disk, so it matches nothing"
    );
}

#[test]
fn retention_prunes_old_archives_but_never_the_active_ledger() {
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let active = cfg.ledger_dir.join("tool.jsonl");
    let old_archive = cfg.ledger_dir.join("tool.jsonl.20240101.1");
    let fresh_archive = cfg.ledger_dir.join("tool.jsonl.20990101.1");
    std::fs::write(&active, format!("{}\n", line("A"))).unwrap();
    std::fs::write(&old_archive, format!("{}\n", line("B"))).unwrap();
    std::fs::write(&fresh_archive, format!("{}\n", line("C"))).unwrap();
    // Age the active ledger AND one archive past the horizon: only the archive may go —
    // the active file is never pruned, whatever its age.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 86_400);
    for p in [&active, &old_archive] {
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
    let cutoff = hatel_core::now_epoch() - 90 * 86_400;
    let removed = hatel_core::sink::prune_before(&cfg, cutoff);
    assert_eq!(removed, 1, "exactly the aged archive is removed");
    assert!(active.exists(), "the active ledger is never pruned");
    assert!(!old_archive.exists(), "the aged archive is gone");
    assert!(fresh_archive.exists(), "a fresh archive is kept");
}

#[test]
fn retention_never_prunes_an_active_ledger_for_a_dotted_kind_name() {
    // Kind names may contain dots (the charset allows them), so a Kind named `foo.jsonl` has
    // the active file `foo.jsonl.jsonl` — which contains `.jsonl.` but, like every active
    // ledger, ENDS with `.jsonl`. The sweep must spare it however old it is, while that same
    // Kind's archives are still prunable.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let dotted_active = cfg.ledger_dir.join("foo.jsonl.jsonl");
    let dotted_archive = cfg.ledger_dir.join("foo.jsonl.jsonl.20240101.1");
    std::fs::write(&dotted_active, format!("{}\n", line("A"))).unwrap();
    std::fs::write(&dotted_archive, format!("{}\n", line("B"))).unwrap();
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 86_400);
    for p in [&dotted_active, &dotted_archive] {
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
    let cutoff = hatel_core::now_epoch() - 90 * 86_400;
    assert_eq!(hatel_core::sink::prune_before(&cfg, cutoff), 1);
    assert!(dotted_active.exists(), "dotted-Kind active ledger spared");
    assert!(!dotted_archive.exists(), "its aged archive is pruned");
}

#[test]
fn sqlite_retention_prunes_only_rows_older_than_the_cutoff() {
    let dir = temp_dir();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.clone(), vec![])
    };
    // A current row through the real write path…
    let reg = load_core().unwrap();
    let mut payload = Payload::new();
    payload.insert("tool_name".into(), "Bash".into());
    let env = make_envelope("tool", payload, &reg, false).unwrap();
    let mut sink = hatel_core::build_sink(&cfg);
    sink.write_record(&env);
    sink.flush();
    // …and an ancient row inserted directly.
    {
        let conn = rusqlite::Connection::open(dir.join("telemetry.db")).unwrap();
        conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload) VALUES (?1,'tool',1,'{}')",
            ["2000-01-01T00:00:00Z"],
        )
        .unwrap();
    }
    let cutoff = hatel_core::now_epoch() - 86_400;
    assert_eq!(
        hatel_core::sink::prune_before(&cfg, cutoff),
        1,
        "exactly the ancient row is deleted"
    );
    let rows = hatel_core::sink::read_records(&cfg, "tool", None);
    assert_eq!(rows.len(), 1, "the in-window row remains");
}

#[test]
fn skill_version_tracks_the_workspace_version() {
    // SKILL.md ships version-locked with the binaries in every release archive; its
    // frontmatter `version` (kept for install-time tracking — not an official skill
    // field) must move with `workspace.package.version`. This gate makes forgetting
    // the bump a loud failure instead of silent drift.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.claude/skills/hatel/SKILL.md");
    let text = std::fs::read_to_string(&path).expect("SKILL.md readable");
    let want = format!("version: {}", env!("CARGO_PKG_VERSION"));
    assert!(
        text.lines().any(|l| l.trim() == want),
        "SKILL.md frontmatter must carry `{want}` (the workspace version)"
    );
}

#[test]
fn cost_snapshot_merges_by_session() {
    use hatel_core::cost::{self, CostRow};
    let cfg = test_config(vec![]);
    let row = |sid: &str, tokens: i64| CostRow {
        session_id: sid.to_string(),
        project: "p".to_string(),
        tokens,
        ts: "2024-01-01T00:00:00Z".to_string(),
        ..CostRow::default()
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
        ts: "2000-01-01T00:00:00Z".to_string(),
        ..CostRow::default()
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
    assert!(
        !cfg.state_dir.join("cost_snapshot.jsonl").exists(),
        "a fully-aged-out snapshot leaves no file behind"
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
        ts: ts.to_string(),
        ..CostRow::default()
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
    let groups = report::aggregate(&reg, &cfg, "ci_check", &query(0, 5, None));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key.as_deref(), Some("lint"));
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
        report::aggregate(&reg, &cfg, "tool", &query(0, 5, p))
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
    assert!(format!("{err}").contains("non-empty `from`"), "got: {err}");
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
    let groups = report::aggregate(&reg, &cfg, "ci_check", &query(0, 5, None));
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
    let groups = report::aggregate(&reg, &cfg, "prompt", &query(0, 5, None));
    assert_eq!(
        groups.iter().map(|g| g.count).sum::<i64>(),
        1,
        "report aggregates sqlite records"
    );
}

#[test]
fn a_kind_section_says_how_far_back_its_store_reaches() {
    // Retention prunes a store from the back, so a window that starts before the oldest record
    // still held was not measured whole; the section names that record's time so a reader can
    // tell an empty early stretch from one nobody recorded. It is read from every file's first
    // line rather than from the records the window admits — an archive older than the window is
    // exactly what places the reach before it.
    let reg = load_core().unwrap();
    let stamped = |ts: &str, tool: &str| {
        serde_json::json!({
            "ts": ts, "kind": "tool", "_schema_version": 1,
            "payload": {"session_id": "S", "project": "p", "tool_name": tool}
        })
        .to_string()
    };
    for sink in [SinkKind::Jsonl, SinkKind::Sqlite] {
        let cfg = Config {
            sink,
            ..config_in(temp_dir(), vec![])
        };
        let section = |cfg: &Config| {
            report::Report::build(&reg, cfg, "30d", &query(0, 0, None))
                .kinds
                .into_iter()
                .find(|k| k.kind == "tool")
                .unwrap()
        };
        assert_eq!(
            section(&cfg).retained_since,
            None,
            "{sink:?}: an empty store reaches nowhere"
        );
        match sink {
            SinkKind::Jsonl => {
                std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
                std::fs::write(
                    cfg.ledger_dir.join("tool.jsonl"),
                    format!("{}\n", stamped("2026-03-01T00:00:00Z", "Read")),
                )
                .unwrap();
                std::fs::write(
                    cfg.ledger_dir.join("tool.jsonl.20260101.1"),
                    format!(
                        "{}\n{}\n",
                        stamped("2026-01-01T00:00:00Z", "Bash"),
                        stamped("2026-01-02T00:00:00Z", "Bash")
                    ),
                )
                .unwrap();
            }
            SinkKind::Sqlite => {
                let mut s = hatel_core::sink::build_sink(&cfg);
                for (ts, tool) in [
                    ("2026-03-01T00:00:00Z", "Read"),
                    ("2026-01-01T00:00:00Z", "Bash"),
                ] {
                    s.write_record(
                        &hatel_core::Envelope::from_json_line(&stamped(ts, tool)).unwrap(),
                    );
                }
                s.flush();
            }
        }
        assert_eq!(
            section(&cfg).retained_since.as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "{sink:?}: the oldest record, wherever it is held"
        );
    }
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
        report::aggregate(&reg, &cfg, "tool", &query(0, 3, None)).len(),
        3,
        "capped at 3"
    );
    assert_eq!(
        report::aggregate(&reg, &cfg, "tool", &query(0, 0, None)).len(),
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

#[test]
fn a_report_names_the_stored_kinds_no_loaded_schema_declares() {
    // Collection and reporting are configured separately, so a ledger can hold a Kind whose
    // schema is not loaded: every query is then blind to it while nothing about the wiring is
    // wrong. A report that stayed silent would present the part it could read as the whole of
    // what was collected.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    std::fs::write(cfg.ledger_dir.join("tool.jsonl"), "").unwrap();
    let reg = load_core().unwrap();
    let build = |cfg: &Config, reg: &_| report::Report::build(reg, cfg, "7d", &query(0, 5, None));
    assert!(
        build(&cfg, &reg).unreadable_kinds.is_none(),
        "a store holding only declared Kinds has no gap to report"
    );

    std::fs::write(cfg.ledger_dir.join("ci_check.jsonl"), "").unwrap();
    let gap = build(&cfg, &reg)
        .unreadable_kinds
        .expect("a stored Kind no schema declares must be named");
    assert_eq!(gap.names, vec!["ci_check"]);

    // Loading the plugin that declares it closes the gap — the report is then answering over
    // everything the store holds, which is the state the message asks the operator to reach.
    let with_plugin = config_in(cfg.state_dir.clone(), vec![example_plugin()]);
    let reg = build_registry(&with_plugin).unwrap();
    assert!(build(&with_plugin, &reg).unreadable_kinds.is_none());
}

#[test]
fn a_resumed_agent_counts_once_however_many_turns_it_stops_on() {
    // SubagentStop marks a turn boundary, not a spawn: an agent resumed with a message stops
    // again, and a teammate driven through a conversation stops on every turn. Counting records
    // would report one agent that ran once as having run four times. `agent_id` is stable across
    // those stops, so declaring it as the Kind's identity makes the spawn the unit.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for turn in 0..4 {
        let mut event = serde_json::json!({
            "hook_event_name": "SubagentStop",
            "session_id": "S", "cwd": "/tmp/x",
            "agent_id": "a43e754af29bd6784", "agent_type": "general-purpose",
            "turn": turn,
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key.as_deref(), Some("general-purpose"));
    assert_eq!(groups[0].count, 1, "four stops, one agent");
    assert_eq!(
        hatel_core::sink::read_records(&cfg, "subagent", None).len(),
        4,
        "every observed stop is still stored — the deduplication is the query's, not the write's"
    );
}

#[test]
fn agents_without_an_identity_never_merge_into_one() {
    // A record carrying no `agent_id` says nothing about being the same agent as another. Merging
    // them under one bucket would undercount silently, which is the opposite failure from the
    // over-count the identity fixes — so each counts as its own.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for _ in 0..3 {
        let mut event = serde_json::json!({
            "hook_event_name": "SubagentStop",
            "session_id": "S", "cwd": "/tmp/x",
            "agent_type": "general-purpose",
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
    assert_eq!(groups[0].count, 3);
}

#[test]
fn an_identity_is_scoped_to_the_group_it_is_counted_in() {
    // Deduplication happens per group, not across the report: were it global, which group kept an
    // agent seen under two labels would depend on storage order, which the SQLite backend does not
    // fix. Per group, both groups answer for what their own records say.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for agent in ["reviewer", "reviewer", "implementer"] {
        let mut event = serde_json::json!({
            "hook_event_name": "SubagentStop",
            "session_id": "S", "cwd": "/tmp/x",
            "agent_id": "a1", "agent_type": agent,
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|g| g.count == 1));
}

#[test]
fn an_identity_must_name_a_field_of_its_kind() {
    use hatel_core::registry::{KindSpec, KindSpecRaw};
    let raw = |identity: Option<&str>| KindSpecRaw {
        name: "k".into(),
        fields: vec!["id".into(), "label".into(), "ms".into()],
        group_key: "label".into(),
        redact: vec![],
        measures: vec!["ms".into()],
        identity: identity.map(str::to_string),
    };
    assert!(KindSpec::from_raw(raw(Some("id"))).is_ok());
    assert!(
        KindSpec::from_raw(raw(Some("agent_id"))).is_err(),
        "an identity naming no field would silently count every record as its own entity"
    );
    assert!(KindSpec::from_raw(raw(None)).is_ok());
}

#[test]
fn each_start_of_one_session_counts_its_own_resume_cost() {
    // `/resume` re-enters an existing conversation under the SAME session id, firing SessionStart
    // again and paying to re-establish the prompt cache each time. Counting sessions rather than
    // starts would collapse every resume into the first one and lose that spend entirely.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut fresh = serde_json::json!({
        "hook_event_name": "SessionStart", "session_id": "S", "cwd": "/tmp/x",
        "source": "startup",
    });
    hatel_core::hook::process_event(&mut fresh, &cfg, &reg);
    for usd in [0.40, 0.55] {
        let mut resumed = serde_json::json!({
            "hook_event_name": "SessionStart", "session_id": "S", "cwd": "/tmp/x",
            "source": "resume", "context_tokens": 40_000,
            "seconds_since_last_response": 30, "estimated_cache_write_usd": usd,
            "prompt_cache_likely_expired": false,
        });
        hatel_core::hook::process_event(&mut resumed, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "session", &query(0, 0, None));
    let resume = groups
        .iter()
        .find(|g| g.key.as_deref() == Some("resume"))
        .expect("resume group");
    assert_eq!(resume.count, 2, "two resumes of one session are two starts");
    let spend = resume
        .sums
        .iter()
        .find(|m| m.name == "estimated_cache_write_usd")
        .unwrap();
    assert!(
        (spend.sum - 0.95).abs() < 1e-9,
        "resume cost sums across starts"
    );
}

#[test]
fn a_fresh_start_carries_no_resume_cost_rather_than_a_zero() {
    // A fresh start has no cache to re-establish, so Claude Code sends no cost fields at all.
    // The record must omit them — a stored zero would be indistinguishable from a resume that
    // genuinely cost nothing.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut fresh = serde_json::json!({
        "hook_event_name": "SessionStart", "session_id": "S", "cwd": "/tmp/x",
        "source": "startup",
    });
    hatel_core::hook::process_event(&mut fresh, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "session", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("source").and_then(|v| v.as_str()),
        Some("startup")
    );
    for absent in [
        "estimated_cache_write_usd",
        "context_tokens",
        "since_last_response_s",
        "cache_likely_expired",
    ] {
        assert!(
            !recs[0].payload.contains_key(absent),
            "{absent} must be absent, not zero"
        );
    }
}

#[test]
fn both_backends_represent_an_entity_by_its_earliest_record() {
    // An identity Kind reports each entity once, carrying that entity's earliest record — measures
    // included. Storage order does not supply that: the JSONL reader returns the active file before
    // its older archives, so a rotation puts the newest record first. The two backends are compared
    // against one another, with a rotation forced between the records, so an answer that depends on
    // storage layout cannot pass.
    let dir = temp_dir();
    let plugin = dir.join("ordered.toml");
    std::fs::write(
        &plugin,
        r#"
[[kind]]
name = "ord.job"
fields = ["job_id", "stage", "ms"]
group_key = "stage"
identity = "job_id"
measures = ["ms"]
"#,
    )
    .unwrap();
    // The last record written stays in the active file, which the JSONL reader lists before the
    // archives — so the entity's LATEST record is the one storage order offers first.
    let records = [
        ("j1", "build", 10.0),
        ("j2", "build", 5.0),
        ("j1", "build", 99.0),
    ];
    let answer = |sink: SinkKind| {
        let mut cfg = config_in(temp_dir(), vec![plugin.clone()]);
        cfg.sink = sink;
        // One record per file, so the active file holds the newest and the archives the older.
        cfg.rotate_bytes = 1;
        let reg = build_registry(&cfg).unwrap();
        for (job, stage, ms) in records {
            let mut payload = Payload::new();
            payload.insert("job_id".into(), job.into());
            payload.insert("stage".into(), stage.into());
            payload.insert("ms".into(), ms.into());
            let mut out = hatel_core::build_sink(&cfg);
            out.write_record(&make_envelope("ord.job", payload, &reg, true).unwrap());
            out.flush();
        }
        report::aggregate(&reg, &cfg, "ord.job", &query(0, 0, None))
    };
    let jsonl = answer(SinkKind::Jsonl);
    let sqlite = answer(SinkKind::Sqlite);
    assert_eq!(jsonl.len(), 1);
    assert_eq!(jsonl[0].count, 2, "two jobs, three records");
    assert_eq!(
        jsonl[0].sums[0].sum, 15.0,
        "j1's earliest record represents it, so 10 + 5 rather than 99"
    );
    assert_eq!(jsonl[0].count, sqlite[0].count);
    assert_eq!(jsonl[0].sums[0].sum, sqlite[0].sums[0].sum);
}
#[test]
fn a_tool_call_is_attributed_to_the_agent_that_made_it() {
    // Only a subagent's call carries `agent_id`, so its absence names the main agent. Without the
    // field a session's tool counts are one undifferentiated total, which is what hides delegated
    // work inside the figure for the session that delegated it.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let call = |use_id: &str, agent: Option<&str>| {
        let mut event = serde_json::json!({
            "hook_event_name": "PostToolUse", "session_id": "S", "cwd": "/tmp/x",
            "tool_name": "Bash", "tool_use_id": use_id, "prompt_id": "P", "duration_ms": 12,
        });
        if let Some(a) = agent {
            event["agent_id"] = serde_json::Value::from(a);
        }
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    };
    call("t1", None);
    call("t2", Some("a9"));
    call("t3", Some("a9"));
    let groups = report::aggregate(
        &reg,
        &cfg,
        "tool",
        &report::Query {
            group_by: Some("agent_id"),
            ..query(0, 0, None)
        },
    );
    let by_key: std::collections::BTreeMap<Option<&str>, i64> =
        groups.iter().map(|g| (g.key.as_deref(), g.count)).collect();
    assert_eq!(
        by_key.get(&Some("a9")),
        Some(&2),
        "the subagent's two calls"
    );
    assert_eq!(
        by_key.get(&None),
        Some(&1),
        "the main agent's call carries no agent_id and is not invented one"
    );
}

#[test]
fn a_skill_the_model_invoked_is_named_and_no_other_tool_input_is_kept() {
    // A skill the model loads on its own expands no command, so the `Skill` call is the only
    // record of which skill it was. Another tool's input can carry a `skill` key of its own, and
    // that is the operator's content.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for (event, tool, id, input) in [
        (
            "PostToolUse",
            "Skill",
            "t1",
            serde_json::json!({"skill": "greet", "args": "a secret"}),
        ),
        (
            "PostToolUseFailure",
            "Skill",
            "t2",
            serde_json::json!({"skill": "greet"}),
        ),
        (
            "PostToolUse",
            "mcp__kb__search",
            "t3",
            serde_json::json!({"skill": "internal note"}),
        ),
        (
            "PostToolUse",
            "Skill",
            "t4",
            serde_json::json!({"skill": {"args": "a secret"}}),
        ),
    ] {
        let mut e = serde_json::json!({
            "hook_event_name": event, "session_id": "S", "cwd": "/tmp/x", "prompt_id": "P",
            "tool_name": tool, "tool_use_id": id, "duration_ms": 3, "tool_input": input,
        });
        hatel_core::hook::process_event(&mut e, &cfg, &reg);
    }
    let recs = hatel_core::sink::read_records(&cfg, "tool", None);
    let skill_of = |id: &str| {
        let rec = recs
            .iter()
            .find(|r| r.payload.get("tool_use_id").and_then(|v| v.as_str()) == Some(id))
            .unwrap();
        rec.payload.get("skill").and_then(|v| v.as_str())
    };
    assert_eq!(skill_of("t1"), Some("greet"));
    assert_eq!(skill_of("t2"), Some("greet"));
    assert_eq!(skill_of("t3"), None);
    assert_eq!(
        skill_of("t4"),
        None,
        "a source that is not one value is not stored"
    );
    let filters = [("tool_name".to_string(), "Skill".to_string())];
    let groups = report::aggregate(
        &reg,
        &cfg,
        "tool",
        &report::Query {
            group_by: Some("skill"),
            filters: &filters,
            ..query(0, 0, None)
        },
    );
    let by_key: std::collections::BTreeMap<Option<&str>, i64> =
        groups.iter().map(|g| (g.key.as_deref(), g.count)).collect();
    assert_eq!(by_key.get(&Some("greet")), Some(&2));
    assert_eq!(
        by_key.get(&None),
        Some(&1),
        "the call whose skill was not one value is still a call, under no skill"
    );
}

#[test]
fn a_failed_call_and_a_returning_one_land_in_the_same_kind() {
    // A tool call fires exactly one of PostToolUse / PostToolUseFailure, so the two bindings write
    // one record per call and `ok` tells them apart. Summing `ok` over a group is its success count.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for (event_name, use_id) in [
        ("PostToolUse", "t1"),
        ("PostToolUse", "t2"),
        ("PostToolUseFailure", "t3"),
    ] {
        let mut event = serde_json::json!({
            "hook_event_name": event_name, "session_id": "S", "cwd": "/tmp/x",
            "tool_name": "Bash", "tool_use_id": use_id, "duration_ms": 10,
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "tool", &query(0, 0, None));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key.as_deref(), Some("Bash"));
    assert_eq!(groups[0].count, 3, "three calls");
    let ok = groups[0].sums.iter().find(|m| m.name == "ok").unwrap();
    assert_eq!(ok.sum, 2.0, "two of the three returned");
}

#[test]
fn one_call_delivered_twice_counts_once() {
    // A repository that binds the hook on top of the user's settings delivers each event twice.
    // `tool_use_id` identifies the call itself, so the second delivery joins the call it repeats.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    for _ in 0..2 {
        let mut event = serde_json::json!({
            "hook_event_name": "PostToolUse", "session_id": "S", "cwd": "/tmp/x",
            "tool_name": "Read", "tool_use_id": "t1", "duration_ms": 7,
        });
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = report::aggregate(&reg, &cfg, "tool", &query(0, 0, None));
    assert_eq!(groups[0].count, 1);
    let ms = groups[0]
        .sums
        .iter()
        .find(|m| m.name == "duration_ms")
        .unwrap();
    assert_eq!(ms.sum, 7.0, "the duplicate does not double the duration");
}

#[test]
fn a_command_record_keeps_the_name_and_nothing_the_operator_typed() {
    // The envelope carries the arguments and the expanded prompt alongside the name. Only the name
    // answers "which commands get used", and the other two are the operator's own text.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptExpansion", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "P1", "expansion_type": "slash_command", "command_name": "hatel",
        "command_args": "a secret argument", "prompt": "/hatel a secret argument",
    });
    hatel_core::hook::process_event(&mut event, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "command", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("command_name").and_then(|v| v.as_str()),
        Some("hatel")
    );
    for leaked in ["command_args", "prompt", "expansion_type"] {
        assert!(
            !recs[0].payload.contains_key(leaked),
            "{leaked} must not be stored"
        );
    }
}

#[test]
fn memory_files_are_told_apart_by_their_path_in_the_repository() {
    // Every directory level can hold its own `CLAUDE.md`, so a final path component alone merges
    // the root file with each nested one and says nothing about which rule was loaded.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let repo = temp_dir().join("acme");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cwd = repo.join("crates/api");
    std::fs::create_dir_all(&cwd).unwrap();
    let at = |rel: &str| repo.join(rel).to_string_lossy().into_owned();
    let cwd = cwd.to_string_lossy().into_owned();
    for mut event in [
        serde_json::json!({
            "hook_event_name": "InstructionsLoaded", "session_id": "S", "cwd": cwd,
            "file_path": at("CLAUDE.md"), "memory_type": "Project", "load_reason": "session_start",
        }),
        serde_json::json!({
            "hook_event_name": "InstructionsLoaded", "session_id": "S", "cwd": cwd,
            "prompt_id": "T1", "file_path": at("crates/api/CLAUDE.md"), "memory_type": "Project",
            "load_reason": "nested_traversal", "trigger_file_path": at("crates/api/src/lib.rs"),
        }),
        serde_json::json!({
            "hook_event_name": "InstructionsLoaded", "session_id": "S", "cwd": cwd,
            "prompt_id": "T1", "file_path": at("docs/style.md"), "memory_type": "Project",
            "load_reason": "include", "trigger_file_path": at("crates/api/src/lib.rs"),
            "parent_file_path": at("crates/api/CLAUDE.md"),
        }),
    ] {
        hatel_core::hook::process_event(&mut event, &cfg, &reg);
    }

    let recs = hatel_core::sink::read_records(&cfg, "memory", None);
    let fields = |i: usize| {
        recs[i]
            .payload
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(recs.len(), 3);
    assert_eq!(
        fields(0),
        [
            ("file_path", "CLAUDE.md"),
            ("load_reason", "session_start"),
            ("memory_type", "Project"),
            ("project", "acme"),
            ("session_id", "S"),
        ]
    );
    assert_eq!(
        fields(1),
        [
            ("file_path", "crates/api/CLAUDE.md"),
            ("load_reason", "nested_traversal"),
            ("memory_type", "Project"),
            ("project", "acme"),
            ("prompt_id", "T1"),
            ("session_id", "S"),
            ("trigger_file_path", "crates/api/src/lib.rs"),
        ]
    );
    assert_eq!(
        fields(2),
        [
            ("file_path", "docs/style.md"),
            ("load_reason", "include"),
            ("memory_type", "Project"),
            ("parent_file_path", "crates/api/CLAUDE.md"),
            ("project", "acme"),
            ("prompt_id", "T1"),
            ("session_id", "S"),
            ("trigger_file_path", "crates/api/src/lib.rs"),
        ]
    );
}

#[test]
fn one_turn_joins_its_records_across_kinds() {
    // `prompt_id` is the turn. Carrying it on every turn-scoped Kind is what lets one query ask
    // what a single request set off, without a schema that models turns.
    let cfg = test_config(vec![]);
    let reg = load_core().unwrap();
    let mut prompt = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "T1", "prompt": "do the thing",
    });
    hatel_core::hook::process_event(&mut prompt, &cfg, &reg);
    let mut call = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "T1", "tool_name": "Bash", "tool_use_id": "t1", "duration_ms": 5,
    });
    hatel_core::hook::process_event(&mut call, &cfg, &reg);
    let mut agent = serde_json::json!({
        "hook_event_name": "SubagentStop", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "T1", "agent_id": "a1", "agent_type": "general-purpose",
    });
    hatel_core::hook::process_event(&mut agent, &cfg, &reg);
    let turn = |kind: &str| {
        report::aggregate(
            &reg,
            &cfg,
            kind,
            &report::Query {
                group_by: Some("prompt_id"),
                ..query(0, 0, None)
            },
        )
    };
    for kind in ["prompt", "tool", "subagent"] {
        let groups = turn(kind);
        assert_eq!(groups.len(), 1, "{kind} groups under one turn");
        assert_eq!(
            groups[0].key.as_deref(),
            Some("T1"),
            "{kind} carries the turn it belongs to"
        );
    }
}
