//! End-to-end tests for the core pipeline: registry build, declarative field maps,
//! sanitization, the JSONL sink, the session index, and windowed reads — all driven
//! through explicit temp-dir configs so they run in parallel without shared state.

use std::path::{Path, PathBuf};

use hatel_core::registry::FieldMap;
use hatel_core::schema::{build_registry, load_core};
use hatel_core::{Config, Payload, SessionIndex, SinkKind, make_envelope, report};

/// A config over a fresh store, which is removed when this is dropped: use the config through
/// this value, not a clone of it.
struct TestConfig {
    cfg: Config,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TestConfig {
    type Target = Config;

    fn deref(&self) -> &Config {
        &self.cfg
    }
}

fn test_config(plugins: Vec<PathBuf>) -> TestConfig {
    let dir = tempfile::tempdir().unwrap();
    TestConfig {
        cfg: config_in(dir.path(), plugins),
        _dir: dir,
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
        contract: None,
        group_by: None,
        sort_by: None,
        filters: &[],
    }
}

fn legacy_query(since: i64, top_n: usize, project: Option<&str>) -> report::Query<'_> {
    report::Query {
        contract: Some("legacy"),
        ..query(since, top_n, project)
    }
}

fn aggregate(
    reg: &hatel_core::Registry,
    cfg: &Config,
    kind: &str,
    q: &report::Query,
) -> Vec<report::GroupAgg> {
    report::aggregate(reg, cfg, kind, q).unwrap()
}

/// A config rooted at an explicit dir, so a test can write a plugin file into it.
fn config_in(dir: &Path, plugins: Vec<PathBuf>) -> Config {
    Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.join("ledger"),
        state_dir: dir.to_path_buf(),
        plugins,
        plugin_source: hatel_core::config::PluginSource::ConfigFile,
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
        export: Ok(Default::default()),
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
fn contracts_survive_plugin_changes_and_commit_atomically_with_records() {
    let home = tempfile::tempdir().unwrap();
    let plugin = home.path().join("plugin.toml");
    let write_schema = |version: u32, unit: &str| {
        std::fs::write(&plugin, format!("[[kind]]\nname='sample.duration'\nversion={version}\nfields={{ name={{type='string',required=true}}, duration={{type='number',unit='{unit}'}} }}\ngroup_key='name'\nmeasures=['duration']\n")).unwrap();
    };
    let mut cfg = config_in(home.path(), vec![plugin.clone()]);
    cfg.sink = SinkKind::Sqlite;
    write_schema(1, "ms");
    let reg = build_registry(&cfg).unwrap();
    let first = make_envelope(
        "sample.duration",
        [
            ("name".into(), "lint".into()),
            ("duration".into(), 1.into()),
        ]
        .into(),
        &reg,
        true,
    )
    .unwrap();
    hatel_core::build_sink(&cfg, &reg)
        .write_record(&first)
        .unwrap();
    write_schema(2, "s");
    let reg = build_registry(&cfg).unwrap();
    let spec = reg.kind("sample.duration").unwrap();
    let mut second = first.clone();
    second.metadata.kind_version = Some(spec.version);
    second.metadata.schema_id = Some(spec.schema_id.clone());
    let mut sink = hatel_core::build_sink(&cfg, &reg);
    assert!(
        sink.write_record(&second).is_err(),
        "one ID cannot change its contract"
    );
    assert_eq!(
        hatel_core::catalog::stored(&cfg).unwrap().len(),
        1,
        "a rejected record must not commit its definition"
    );
    second.metadata.record_id = Some("second".into());
    sink.write_record(&second).unwrap();
    assert_eq!(hatel_core::catalog::stored(&cfg).unwrap().len(), 2);
    std::fs::remove_file(plugin).unwrap();
    cfg.plugins.clear();
    let old = hatel_core::catalog::select(
        &cfg,
        "sample.duration",
        first.metadata.schema_id.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(old.version, 1);
    assert_eq!(old.field_specs["duration"].unit.as_deref(), Some("ms"));
    let conn = rusqlite::Connection::open(home.path().join("telemetry.db")).unwrap();
    conn.execute(
        "INSERT INTO contracts(schema_id,kind,definition) VALUES ('broken','other','{}')",
        [],
    )
    .unwrap();
    assert!(hatel_core::catalog::stored(&cfg).is_err());
    assert!(
        hatel_core::catalog::select(
            &cfg,
            "sample.duration",
            first.metadata.schema_id.as_deref().unwrap()
        )
        .is_ok()
    );
    assert!(
        build_registry(&cfg)
            .unwrap()
            .kind("sample.duration")
            .is_none()
    );
}

#[test]
fn contract_identity_changes_are_distinct_from_query_defaults() {
    let schema = "name='sample'\nfields=['one','two']\ngroup_key='one'\nidentity='one'\n";
    let a = hatel_core::KindSpec::from_raw(toml::from_str(schema).unwrap()).unwrap();
    let b = hatel_core::KindSpec::from_raw(
        toml::from_str(&schema.replace("identity='one'", "identity='two'")).unwrap(),
    )
    .unwrap();
    assert_ne!(a.schema_id, b.schema_id);
    let c = hatel_core::KindSpec::from_raw(
        toml::from_str(&schema.replace("group_key='one'", "group_key='two'")).unwrap(),
    )
    .unwrap();
    assert_eq!(a.schema_id, c.schema_id);
    assert_eq!(
        hatel_core::KindSpec::from_raw(a.definition())
            .unwrap()
            .schema_id,
        a.schema_id
    );
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
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("oov.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname = \"team.pre\"\nfields = [\"session_id\"]\ngroup_key = \"session_id\"\n\
         [[binding]]\nevent = \"PreToolUse\"\nkind = \"team.pre\"\nmap.session_id = { from = \"session_id\" }\n",
    )
    .unwrap();
    let reg = build_registry(&config_in(dir.path(), vec![plugin])).unwrap();
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
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("myproj");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "SessionStart",
        "session_id": "S3", "cwd": repo.to_str().unwrap()
    });
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let outside = tempfile::tempdir().unwrap();
    for (session_id, cwd) in [("S4", ""), ("S5", outside.path().to_str().unwrap())] {
        let mut event = serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": session_id, "cwd": cwd
        });
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let outside = tempfile::tempdir().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "S6", "cwd": outside.path().to_str().unwrap(), "prompt": "hello"
    });
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    let recs = hatel_core::sink::read_records(&cfg, "compaction", None);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].payload.get("trigger").and_then(|v| v.as_str()),
        Some("auto")
    );
}

#[test]
fn git_branch_is_injected_only_when_a_binding_uses_it() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("branch.toml");
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
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(
        repo.join(".git").join("HEAD"),
        "ref: refs/heads/spec/checkout\n",
    )
    .unwrap();
    let cfg = Config {
        sink: SinkKind::Jsonl,
        ledger_dir: dir.path().join("ledger"),
        state_dir: dir.path().to_path_buf(),
        plugins: vec![plugin],
        plugin_source: hatel_core::config::PluginSource::ConfigFile,
        rotate_bytes: 10 * 1024 * 1024,
        retention_days: 90,
        disabled: false,
        strict: true,
        export: Ok(Default::default()),
    };
    let reg = build_registry(&cfg).unwrap();

    // SessionEnd binding references git_branch → it is injected and spec_slug captured.
    let mut end = serde_json::json!({
        "hook_event_name": "SessionEnd", "session_id": "S", "cwd": repo.to_str().unwrap()
    });
    hatel_core::claude::hook::process_event(&mut end, &cfg, &reg);
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
    hatel_core::claude::hook::process_event(&mut tool, &cfg, &reg);
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
        ..legacy_query(0, 0, None)
    };
    let groups = aggregate(&reg, &cfg, "tool", &q);
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
        ..legacy_query(0, 0, None)
    };
    let groups = aggregate(&reg, &cfg, "tool", &q);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, 1, "both filters must match");
    // A value matching nothing yields an empty report — not an error, not all rows.
    let filters = vec![("tool_name".to_string(), "Nope".to_string())];
    let q = report::Query {
        filters: &filters,
        ..legacy_query(0, 0, None)
    };
    assert!(aggregate(&reg, &cfg, "tool", &q).is_empty());
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
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    sink.write_record(&env).unwrap();
    sink.flush().unwrap();
    let stored = vec![(
        "actor".to_string(),
        hatel_core::pii::redacted("alice@example.com"),
    )];
    let q = report::Query {
        filters: &stored,
        ..query(0, 0, None)
    };
    let groups = aggregate(&reg, &cfg, "ci_check", &q);
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
        aggregate(&reg, &cfg, "ci_check", &q).is_empty(),
        "the raw value never exists on disk, so it matches nothing"
    );
}

/// Move a file's last write `days` into the past.
fn age(path: &std::path::Path, days: u64) {
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(past)
        .unwrap();
}

/// The `tool_name` of every stored `tool` record, sorted.
fn stored_tools(cfg: &Config) -> Vec<String> {
    let mut tools: Vec<String> = hatel_core::sink::read_records(cfg, "tool", None)
        .iter()
        .filter_map(|env| env.payload.get("tool_name")?.as_str().map(String::from))
        .collect();
    tools.sort();
    tools
}

#[test]
fn retention_expires_a_ledger_that_stopped_being_written() {
    // A Kind nothing writes any more never reaches the size that rotates it, and only archives
    // are deleted, so the sweep archives the quiet active ledger and a later sweep deletes it.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let active = cfg.ledger_dir.join("tool.jsonl");
    let old_archive = cfg.ledger_dir.join("tool.jsonl.20240101.1");
    let fresh_archive = cfg.ledger_dir.join("tool.jsonl.20990101.1");
    std::fs::write(&active, format!("{}\n", line("A"))).unwrap();
    std::fs::write(&old_archive, format!("{}\n", line("B"))).unwrap();
    std::fs::write(&fresh_archive, format!("{}\n", line("C"))).unwrap();
    age(&active, 100);
    age(&old_archive, 100);
    let retention = cfg.retention(hatel_core::now_epoch());
    assert_eq!(
        hatel_core::sink::prune(&cfg, retention),
        1,
        "the aged archive"
    );
    assert!(!active.exists(), "the quiet active ledger is archived");
    assert_eq!(
        stored_tools(&cfg),
        ["A", "C"],
        "a file archived by a sweep is not deleted by the same sweep"
    );
    assert_eq!(
        hatel_core::sink::prune(&cfg, retention),
        1,
        "the next sweep"
    );
    assert_eq!(stored_tools(&cfg), ["C"], "a fresh archive is kept");
}

#[test]
fn retention_archives_an_active_ledger_once_it_reaches_back_a_tenth_of_the_horizon() {
    // A Kind written slowly but steadily never reaches the size that rotates it, and its last
    // write is always recent, so its first record is what says how far back it reaches. Archived
    // at a tenth of the horizon, it expires at most that span plus two sweep intervals after its
    // oldest record should have.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let active = cfg.ledger_dir.join("tool.jsonl");
    let now = hatel_core::now_epoch();
    let retention = cfg.retention(now);
    let since = |days: i64| format!("{}\n{}\n", line_at("A", now - days * 86_400), line("B"));
    std::fs::write(&active, since(8)).unwrap();
    hatel_core::sink::prune(&cfg, retention);
    assert!(
        active.exists(),
        "eight days back on a 90-day horizon stays active"
    );
    std::fs::write(&active, since(10)).unwrap();
    assert_eq!(
        hatel_core::sink::prune(&cfg, retention),
        0,
        "nothing expired"
    );
    assert!(!active.exists(), "ten days back is archived");
    assert_eq!(
        stored_tools(&cfg),
        ["A", "B"],
        "with every record still read"
    );
}

#[test]
fn retention_never_deletes_an_active_ledger_for_a_dotted_kind_name() {
    // Kind names may contain dots (the charset allows them), so a Kind named `foo.jsonl` has
    // the active file `foo.jsonl.jsonl` — which contains `.jsonl.` but, like every active
    // ledger, ENDS with `.jsonl`. However old, the sweep must archive it rather than take it for
    // an archive and delete it, while that same Kind's archives are still prunable.
    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    let dotted_active = cfg.ledger_dir.join("foo.jsonl.jsonl");
    let dotted_archive = cfg.ledger_dir.join("foo.jsonl.jsonl.20240101.1");
    std::fs::write(&dotted_active, format!("{}\n", line("A"))).unwrap();
    std::fs::write(&dotted_archive, format!("{}\n", line("B"))).unwrap();
    age(&dotted_active, 100);
    age(&dotted_archive, 100);
    let retention = cfg.retention(hatel_core::now_epoch());
    assert_eq!(
        hatel_core::sink::prune(&cfg, retention),
        1,
        "its aged archive"
    );
    let kept: Vec<String> = hatel_core::sink::read_records(&cfg, "foo.jsonl", None)
        .iter()
        .filter_map(|env| env.payload.get("tool_name")?.as_str().map(String::from))
        .collect();
    assert!(!dotted_active.exists(), "its active ledger is archived");
    assert_eq!(kept, ["A"], "and not deleted");
}

#[test]
fn sqlite_retention_prunes_only_rows_older_than_the_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    // A current row through the real write path…
    let reg = load_core().unwrap();
    let mut payload = Payload::new();
    payload.insert("tool_name".into(), "Bash".into());
    let env = make_envelope("tool", payload, &reg, false).unwrap();
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    sink.write_record(&env).unwrap();
    sink.flush().unwrap();
    // …and an ancient row inserted directly.
    {
        let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
        conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload) VALUES (?1,'tool',1,'{}')",
            ["2000-01-01T00:00:00Z"],
        )
        .unwrap();
    }
    assert_eq!(
        hatel_core::sink::prune(&cfg, cfg.retention(hatel_core::now_epoch())),
        1,
        "exactly the ancient row is deleted"
    );
    let rows = hatel_core::sink::read_records(&cfg, "tool", None);
    assert_eq!(rows.len(), 1, "the in-window row remains");
}

#[test]
fn an_undeclared_kind_says_when_retention_removes_it() {
    // A Kind retired on purpose needs no plugin: nothing writes it, and a running receiver's
    // retention removes it — by the date that follows from when the store last wrote it, under
    // either sink.
    let at = |ts: &str| hatel_core::ts_epoch(ts).unwrap();
    let expires_by = |cfg: &Config| {
        let reg = build_registry(cfg).unwrap();
        let gap = hatel_core::schema::UnreadableKinds::detect(&reg, cfg)
            .unwrap()
            .unwrap();
        assert!(gap.to_string().ends_with(&format!(
            "a running receiver removes the last of them by {}",
            gap.expires_by.clone().unwrap()
        )));
        gap.expires_by
    };
    // 2026-03-01 plus the 90-day horizon plus one sweep interval (a day at that horizon).
    let removed_by = Some("2026-05-31".to_string());

    let cfg = test_config(vec![]);
    std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
    for (name, written) in [
        ("retired.jsonl.20260101.1", "2026-01-01T00:00:00Z"),
        ("retired.jsonl", "2026-03-01T00:00:00Z"),
        ("gone.jsonl", "2026-02-01T00:00:00Z"),
    ] {
        let path = cfg.ledger_dir.join(name);
        std::fs::write(&path, "").unwrap();
        let written = u64::try_from(at(written)).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(written))
            .unwrap();
    }
    assert_eq!(expires_by(&cfg), removed_by);

    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    sink.flush().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
    for (kind, ts) in [
        ("retired", "2026-01-01T00:00:00Z"),
        ("retired", "2026-03-01T00:00:00Z"),
        ("gone", "2026-02-01T00:00:00Z"),
    ] {
        conn.execute(
            "INSERT INTO records (ts, kind, schema_version, payload) VALUES (?1,?2,1,'{}')",
            [ts, kind],
        )
        .unwrap();
    }
    assert_eq!(expires_by(&cfg), removed_by);
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

/// A cost row for `sid` with `tokens`, last heard at `ts`.
fn cost_row(sid: &str, tokens: i64, ts: &str) -> hatel_core::cost::CostRow {
    hatel_core::cost::CostRow {
        session_id: sid.to_string(),
        project: "p".to_string(),
        tokens,
        ts: ts.to_string(),
        ..hatel_core::cost::CostRow::default()
    }
}

fn tokens_by_session(dir: &std::path::Path) -> Vec<(String, i64)> {
    let mut rows: Vec<(String, i64)> = hatel_core::cost::read_snapshot(dir)
        .unwrap()
        .into_iter()
        .map(|r| (r.session_id, r.tokens))
        .collect();
    rows.sort();
    rows
}

#[test]
fn the_cost_snapshot_writes_only_what_changed_and_checkpoints_the_rest() {
    use hatel_core::cost::Snapshot;
    let cfg = test_config(vec![]);
    let dir = &cfg.state_dir;
    let changes = dir.join("cost_changes.jsonl");
    let at = "2026-01-01T00:00:00Z";
    let mut snapshot = Snapshot::load(dir).unwrap();
    snapshot
        .record([cost_row("S1", 10, at), cost_row("S2", 5, at)])
        .unwrap();
    std::fs::remove_file(&changes).unwrap();
    snapshot
        .record([cost_row("S1", 10, at), cost_row("S2", 5, at)])
        .unwrap();
    assert!(
        !changes.exists(),
        "a flush in which nothing changed writes nothing"
    );
    snapshot.record([cost_row("S1", 99, at)]).unwrap();
    assert_eq!(
        tokens_by_session(dir),
        [("S1".into(), 99), ("S2".into(), 5)],
        "the changes hold every session changed since the checkpoint"
    );
    snapshot.checkpoint(0).unwrap();
    assert!(!changes.exists(), "a checkpoint takes the changes");
    assert_eq!(
        tokens_by_session(dir),
        [("S1".into(), 99), ("S2".into(), 5)]
    );
}

#[test]
fn a_checkpoint_drops_rows_past_retention_and_an_empty_snapshot_leaves_no_file() {
    use hatel_core::cost::Snapshot;
    let cfg = test_config(vec![]);
    let dir = &cfg.state_dir;
    let now = hatel_core::now_iso_utc();
    let mut snapshot = Snapshot::load(dir).unwrap();
    snapshot
        .record([
            cost_row("old", 1, "2000-01-01T00:00:00Z"),
            cost_row("recent", 2, &now),
        ])
        .unwrap();
    let cutoff = hatel_core::ts_epoch(&now).unwrap() - 86_400;
    snapshot.checkpoint(cutoff).unwrap();
    assert_eq!(tokens_by_session(dir), [("recent".into(), 2)]);
    snapshot.checkpoint(i64::MAX).unwrap();
    assert!(tokens_by_session(dir).is_empty());
    assert!(!dir.join("cost_snapshot.jsonl").exists());
    assert!(!dir.join("cost_changes.jsonl").exists());
}

#[test]
fn a_restart_keeps_the_changes_no_checkpoint_has_taken() {
    use hatel_core::cost::Snapshot;
    let cfg = test_config(vec![]);
    let dir = &cfg.state_dir;
    let at = "2026-01-01T00:00:00Z";
    Snapshot::load(dir)
        .unwrap()
        .record([cost_row("S1", 10, at)])
        .unwrap();
    Snapshot::load(dir)
        .unwrap()
        .record([cost_row("S2", 5, at)])
        .unwrap();
    assert_eq!(
        tokens_by_session(dir),
        [("S1".into(), 10), ("S2".into(), 5)]
    );
}

#[test]
fn each_session_reads_as_its_latest_row_whichever_file_holds_it() {
    let cfg = test_config(vec![]);
    let dir = &cfg.state_dir;
    std::fs::create_dir_all(dir).unwrap();
    let line = |row: hatel_core::cost::CostRow| serde_json::to_string(&row).unwrap() + "\n";
    let (earlier, later) = ("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z");
    std::fs::write(
        dir.join("cost_snapshot.jsonl"),
        line(cost_row("S1", 2, later)) + &line(cost_row("S2", 1, earlier)),
    )
    .unwrap();
    std::fs::write(
        dir.join("cost_changes.jsonl"),
        line(cost_row("S1", 1, earlier)) + &line(cost_row("S2", 2, earlier)),
    )
    .unwrap();
    assert_eq!(
        tokens_by_session(dir),
        [("S1".into(), 2), ("S2".into(), 2)],
        "the later row wins, and a tie goes to the changes"
    );
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
        hatel_core::claude::hook::process_event(&mut ev, &cfg, &reg);
    }
    assert_eq!(
        hatel_core::sink::read_records(&cfg, "compaction", None).len(),
        1
    );
}

#[test]
fn binding_writing_a_non_allowlisted_field_is_rejected_at_build() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.typo={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("not in the kind's fields"),
        "got: {err}"
    );
}

#[test]
fn invalid_capture_regex_is_rejected_at_build() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.spec={ from=\"git_branch\", capture=\"([\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("invalid capture regex"),
        "got: {err}"
    );
}

#[test]
fn unsafe_kind_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("bad.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"../escape\"\nfields=[\"a\"]\ngroup_key=\"a\"\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
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
fn report_separates_legacy_invalid_measures_from_real_numbers() {
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
    let groups = aggregate(&reg, &cfg, "ci_check", &legacy_query(0, 5, None));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].key.as_deref(), Some("lint"));
    assert_eq!(groups[0].count, 2);
    assert_eq!(groups[0].sums[0].name, "runs");
    assert_eq!(groups[0].sums[0].sum, 14000.0);
    assert_eq!(groups[0].sums[0].valid_count, 1);
    assert_eq!(groups[0].sums[0].invalid_count, 1);
    assert_eq!(groups[0].sums[0].missing_count, 0);
    assert_eq!(groups[0].sums[0].avg, Some(14000.0));
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
        aggregate(&reg, &cfg, "tool", &legacy_query(0, 5, p))
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
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"project\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.project={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("may not map 'project'"),
        "got: {err}"
    );
}

#[test]
fn field_map_with_two_transforms_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"n\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.session_id={ from=\"session_id\" }\nmap.n={ from=\"p\", len=true, basename=true }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("at most one"), "got: {err}");
}

#[test]
fn field_map_transform_without_source_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"n\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.session_id={ from=\"session_id\" }\nmap.n={ len=true }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(format!("{err}").contains("non-empty `from`"), "got: {err}");
}

#[test]
fn capture_regex_without_a_group_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\",\"spec\"]\ngroup_key=\"spec\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\n\
         map.spec={ from=\"git_branch\", capture=\"spec/.+\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
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
    let groups = aggregate(&reg, &cfg, "ci_check", &legacy_query(0, 5, None));
    assert_eq!(groups[0].sums[0].sum, 50.0, "inf rejected, only 50 summed");
}

#[test]
fn duplicate_event_kind_binding_is_rejected() {
    // Two bindings for the same (event, kind) would write two records per fire.
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("p.toml");
    std::fs::write(
        &plugin,
        "[[kind]]\nname=\"x\"\nfields=[\"session_id\"]\ngroup_key=\"session_id\"\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.session_id={ from=\"session_id\" }\n\
         [[binding]]\nevent=\"SessionEnd\"\nkind=\"x\"\nmap.session_id={ from=\"session_id\" }\n",
    )
    .unwrap();
    let err = build_registry(&config_in(dir.path(), vec![plugin])).unwrap_err();
    assert!(
        format!("{err}").contains("already has a binding for kind"),
        "got: {err}"
    );
}

#[test]
fn sqlite_maintenance_is_explicit_and_preserves_exact_occurrence_times() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE records(ts TEXT NOT NULL,kind TEXT NOT NULL,schema_version INTEGER NOT NULL,payload TEXT NOT NULL);").unwrap();
    let times = [
        "2026-01-01T09:00:00.000000002+09:00",
        "2026-01-01T00:00:00.000000001Z",
    ];
    for time in times {
        conn.execute("INSERT INTO records VALUES (?1,'prompt',1,'{}')", [time])
            .unwrap();
    }
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let reg = load_core().unwrap();
    let env = make_envelope("prompt", Payload::new(), &reg, true).unwrap();
    assert!(
        hatel_core::build_sink(&cfg, &reg)
            .write_record(&env)
            .unwrap_err()
            .to_string()
            .contains("migrate --quiescent")
    );
    assert!(hatel_core::sink::write_readiness(&db).is_err());
    let section =
        hatel_core::query::section(&cfg, reg.kind("prompt").unwrap(), &legacy_query(0, 0, None))
            .unwrap();
    assert_eq!(section.groups.unwrap()[0].count, 2);
    assert_eq!(section.retained_since.as_deref(), Some(times[1]));
    assert_eq!(hatel_core::migration::upgrade_sqlite(&db).unwrap(), 2);
    hatel_core::sink::write_readiness(&db).unwrap();
    assert_eq!(
        hatel_core::sink::database_version(&db).unwrap(),
        Some(hatel_core::sink::DATABASE_VERSION)
    );
    let stored = conn.prepare("SELECT ts,occurred_seconds,occurred_nanos FROM records ORDER BY occurred_seconds,occurred_nanos").unwrap().query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i32>(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    let second = times[0].parse::<jiff::Timestamp>().unwrap().as_second();
    assert_eq!(
        stored,
        vec![(times[1].into(), second, 1), (times[0].into(), second, 2)]
    );
    assert_eq!(hatel_core::migration::upgrade_sqlite(&db).unwrap(), 0);
    hatel_core::build_sink(&cfg, &reg)
        .write_record(&env)
        .unwrap();
    assert_eq!(
        hatel_core::sink::read_records(&cfg, "prompt", None).len(),
        3
    );
}

#[test]
fn sqlite_maintenance_rolls_back_invalid_times_and_refuses_future_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE records(ts TEXT NOT NULL,kind TEXT NOT NULL,schema_version INTEGER NOT NULL,payload TEXT NOT NULL); INSERT INTO records VALUES ('invalid','prompt',1,'{}');").unwrap();
    assert!(
        hatel_core::migration::upgrade_sqlite(&db)
            .unwrap_err()
            .to_string()
            .contains("invalid timestamp")
    );
    let columns = conn
        .prepare("PRAGMA table_info(records)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(columns, vec!["ts", "kind", "schema_version", "payload"]);
    assert_eq!(hatel_core::sink::database_version(&db).unwrap(), Some(0));
    conn.pragma_update(None, "user_version", hatel_core::sink::DATABASE_VERSION + 1)
        .unwrap();
    assert!(
        hatel_core::migration::upgrade_sqlite(&db)
            .unwrap_err()
            .to_string()
            .contains("newer than supported")
    );
    assert!(hatel_core::sink::write_readiness(&db).is_err());
    assert_eq!(
        conn.query_row("SELECT ts FROM records", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "invalid"
    );
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let reg = load_core().unwrap();
    assert!(
        hatel_core::query::aggregate(&cfg, reg.kind("prompt").unwrap(), &legacy_query(0, 0, None))
            .unwrap_err()
            .to_string()
            .contains("newer than supported")
    );
}

#[test]
fn sqlite_window_filter_excludes_old_records() {
    // The SQLite reader pushes the time window into SQL; an out-of-window row is excluded.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let reg = load_core().unwrap();
    // recent record via the real write path
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("telemetry.db");
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let reg = load_core().unwrap();
    let mut event = serde_json::json!({
        "hook_event_name": "UserPromptSubmit", "session_id": "S", "cwd": "/tmp/p", "prompt": "hi"
    });
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);

    let recs = hatel_core::sink::read_records(&cfg, "prompt", None);
    assert_eq!(recs.len(), 1, "record read back from sqlite");
    assert_eq!(
        recs[0].payload.get("prompt_len").and_then(|v| v.as_i64()),
        Some(2)
    );
    let groups = aggregate(&reg, &cfg, "prompt", &query(0, 5, None));
    assert_eq!(
        groups.iter().map(|g| g.count).sum::<i64>(),
        1,
        "report aggregates sqlite records"
    );
}

#[test]
fn a_kind_section_says_how_far_back_its_store_reaches() {
    // The oldest retained occurrence is scoped to the selected contract and project,
    // independent of the query window.
    let reg = load_core().unwrap();
    // A record carrying no `project` at all — a tree outside any repository — sits in the store
    // under no project's reach, the way the aggregate leaves it out of every project's groups.
    let stamped = |ts: &str, project: Option<&str>| {
        let mut payload = serde_json::json!({"session_id": "S", "tool_name": "Bash"});
        if let Some(p) = project {
            payload["project"] = serde_json::Value::String(p.to_string());
        }
        serde_json::json!({"ts": ts, "kind": "tool", "_schema_version": 1, "payload": payload})
            .to_string()
    };
    for sink in [SinkKind::Jsonl, SinkKind::Sqlite] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            sink,
            ..config_in(dir.path(), vec![])
        };
        let section = |cfg: &Config, project: Option<&str>| {
            report::Report::build(&reg, cfg, "30d", &legacy_query(0, 0, project))
                .kinds
                .into_iter()
                .find(|k| k.kind == "tool")
                .unwrap()
        };
        assert_eq!(
            section(&cfg, None).retained_since,
            None,
            "{sink:?}: an empty store reaches nowhere"
        );
        match sink {
            SinkKind::Jsonl => {
                std::fs::create_dir_all(&cfg.ledger_dir).unwrap();
                std::fs::write(
                    cfg.ledger_dir.join("tool.jsonl"),
                    format!(
                        "{}\n{}\n",
                        stamped("2026-02-01T00:00:00Z", None),
                        stamped("2026-03-01T00:00:00Z", Some("p"))
                    ),
                )
                .unwrap();
                std::fs::write(
                    cfg.ledger_dir.join("tool.jsonl.20260101.1"),
                    format!(
                        "{}\n{}\n",
                        stamped("2026-01-01T00:00:00Z", Some("q")),
                        stamped("2026-01-02T00:00:00Z", Some("q"))
                    ),
                )
                .unwrap();
            }
            SinkKind::Sqlite => {
                let mut s = hatel_core::sink::build_sink(&cfg, &build_registry(&cfg).unwrap());
                for (ts, project) in [
                    ("2026-02-01T00:00:00Z", None),
                    ("2026-03-01T00:00:00Z", Some("p")),
                    ("2026-01-01T00:00:00Z", Some("q")),
                ] {
                    s.write_record(
                        &hatel_core::Envelope::from_json_line(&stamped(ts, project)).unwrap(),
                    )
                    .unwrap();
                }
                s.flush().unwrap();
            }
        }
        assert_eq!(
            section(&cfg, None).retained_since.as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "{sink:?}: the oldest record, wherever it is held"
        );
        // Scoped to a project, the reach is that project's own oldest record: another project's
        // archive says nothing about whether this one was being recorded then, and neither does
        // the older record ahead of it in the same file that names no project.
        assert_eq!(
            section(&cfg, Some("p")).retained_since.as_deref(),
            Some("2026-03-01T00:00:00Z"),
            "{sink:?}: the project's own oldest record"
        );
        assert_eq!(
            section(&cfg, Some("r")).retained_since,
            None,
            "{sink:?}: a project the store never saw reaches nowhere"
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
        aggregate(&reg, &cfg, "tool", &legacy_query(0, 3, None)).len(),
        3,
        "capped at 3"
    );
    assert_eq!(
        aggregate(&reg, &cfg, "tool", &legacy_query(0, 0, None)).len(),
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
    line_at(tool, hatel_core::now_epoch())
}

/// A `tool` record stamped `epoch`.
fn line_at(tool: &str, epoch: i64) -> String {
    serde_json::json!({
        "ts": jiff::Timestamp::from_second(epoch).unwrap().to_string(),
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
    let with_plugin = config_in(&cfg.state_dir, vec![example_plugin()]);
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "subagent", &query(0, 0, None));
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|g| g.count == 1));
}

#[test]
fn an_identity_must_name_a_field_of_its_kind() {
    use hatel_core::registry::{KindSpec, KindSpecRaw};
    let raw = |identity: Option<&str>| KindSpecRaw {
        name: "k".into(),
        mode: Default::default(),
        fields: vec!["id".into(), "label".into(), "ms".into()].into(),
        version: 1,
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
    hatel_core::claude::hook::process_event(&mut fresh, &cfg, &reg);
    for usd in [0.40, 0.55] {
        let mut resumed = serde_json::json!({
            "hook_event_name": "SessionStart", "session_id": "S", "cwd": "/tmp/x",
            "source": "resume", "context_tokens": 40_000,
            "seconds_since_last_response": 30, "estimated_cache_write_usd": usd,
            "prompt_cache_likely_expired": false,
        });
        hatel_core::claude::hook::process_event(&mut resumed, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "session", &query(0, 0, None));
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
    hatel_core::claude::hook::process_event(&mut fresh, &cfg, &reg);
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
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("ordered.toml");
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
        let store = tempfile::tempdir().unwrap();
        let mut cfg = config_in(store.path(), vec![plugin.clone()]);
        cfg.sink = sink;
        // One record per file, so the active file holds the newest and the archives the older.
        cfg.rotate_bytes = 1;
        let reg = build_registry(&cfg).unwrap();
        for (job, stage, ms) in records {
            let mut payload = Payload::new();
            payload.insert("job_id".into(), job.into());
            payload.insert("stage".into(), stage.into());
            payload.insert("ms".into(), ms.into());
            let mut out = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
            out.write_record(&make_envelope("ord.job", payload, &reg, true).unwrap())
                .unwrap();
            out.flush().unwrap();
        }
        aggregate(&reg, &cfg, "ord.job", &query(0, 0, None))
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
fn snapshot_queries_select_latest_project_entities_before_filtering_and_grouping() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("snapshot.toml");
    std::fs::write(&plugin, r#"
[[kind]]
name = "state.job"
mode = "snapshot"
identity = "id"
fields = { id = { type = "string", required = true }, status = { type = "string", required = true }, value = { type = "integer", required = true } }
group_key = "status"
measures = ["value"]
"#).unwrap();
    for sink in [SinkKind::Jsonl, SinkKind::Sqlite] {
        let store = tempfile::tempdir().unwrap();
        let mut cfg = config_in(store.path(), vec![plugin.clone()]);
        cfg.sink = sink;
        cfg.rotate_bytes = 1;
        let registry = build_registry(&cfg).unwrap();
        let spec = registry.kind("state.job").unwrap();
        let mut out = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
        for (project, id, status, value, seconds) in [
            ("A", "j1", "open", 2, "00"),
            ("A", "j1", "closed", 7, "02"),
            ("A", "j2", "open", 3, "01"),
            ("A", "j1", "open", 99, "01"),
            ("B", "j1", "open", 4, "03"),
            ("A", "j1", "closed", 8, "02"),
        ] {
            let mut env = make_envelope(
                "state.job",
                serde_json::from_value(serde_json::json!({"id":id,"status":status,"value":value}))
                    .unwrap(),
                &registry,
                true,
            )
            .unwrap();
            env.ts = format!("2026-01-01T00:00:{seconds}Z");
            env.metadata.project = Some(hatel_core::ProjectRef {
                key: project.into(),
                label: "same-label".into(),
            });
            out.write_record(&env).unwrap();
        }
        out.flush().unwrap();
        let all = hatel_core::query::aggregate(&cfg, spec, &query(0, 0, None)).unwrap();
        assert_eq!(all.len(), 2);
        let open = all
            .iter()
            .find(|group| group.key.as_deref() == Some("open"))
            .unwrap();
        assert_eq!((open.count, open.sums[0].sum), (2, 7.0));
        let closed = all
            .iter()
            .find(|group| group.key.as_deref() == Some("closed"))
            .unwrap();
        assert_eq!((closed.count, closed.sums[0].sum), (1, 8.0));
        let filters = vec![("status".into(), "open".into())];
        let filtered = hatel_core::query::aggregate(
            &cfg,
            spec,
            &report::Query {
                filters: &filters,
                ..query(0, 0, Some("A"))
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!((filtered[0].count, filtered[0].sums[0].sum), (1, 3.0));
        let recent =
            hatel_core::query::aggregate(&cfg, spec, &query(1_767_225_603, 0, None)).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!((recent[0].count, recent[0].sums[0].sum), (1, 4.0));
        for value in [1, 1, 2] {
            let mut tied = make_envelope(
                "state.job",
                serde_json::from_value(
                    serde_json::json!({"id":"tied","status":"open","value":value}),
                )
                .unwrap(),
                &registry,
                true,
            )
            .unwrap();
            tied.ts = "2026-01-01T00:00:05Z".into();
            tied.metadata.observed_at = Some("2026-01-01T00:00:06Z".into());
            out.write_record(&tied).unwrap();
            out.flush().unwrap();
            let result = hatel_core::query::aggregate(&cfg, spec, &query(0, 0, None));
            if value == 1 {
                let groups = result.unwrap();
                let open = groups
                    .iter()
                    .find(|group| group.key.as_deref() == Some("open"))
                    .unwrap();
                assert_eq!((open.count, open.sums[0].sum), (3, 8.0));
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("identical occurrence and observation times")
                );
            }
        }
        let mut invalid = make_envelope(
            "state.job",
            serde_json::from_value(serde_json::json!({"id":"bad","status":"open","value":1}))
                .unwrap(),
            &registry,
            true,
        )
        .unwrap();
        invalid.payload.remove("id");
        out.write_record(&invalid).unwrap();
        out.flush().unwrap();
        assert!(
            hatel_core::query::aggregate(&cfg, spec, &query(0, 0, None))
                .unwrap_err()
                .to_string()
                .contains("non-empty string identity")
        );
        let mut unobserved = make_envelope(
            "state.job",
            serde_json::from_value(
                serde_json::json!({"id":"unobserved","status":"open","value":1}),
            )
            .unwrap(),
            &registry,
            true,
        )
        .unwrap();
        unobserved.metadata.observed_at = None;
        unobserved.metadata.project = Some(hatel_core::ProjectRef {
            key: "no-time".into(),
            label: "no-time".into(),
        });
        out.write_record(&unobserved).unwrap();
        out.flush().unwrap();
        assert!(
            hatel_core::query::aggregate(&cfg, spec, &query(0, 0, Some("no-time")))
                .unwrap_err()
                .to_string()
                .contains("valid observation time")
        );
        assert!(hatel_core::query::aggregate(&cfg, spec, &query(0, 0, Some("A"))).is_ok());
    }
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    };
    call("t1", None);
    call("t2", Some("a9"));
    call("t3", Some("a9"));
    let groups = aggregate(
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
        hatel_core::claude::hook::process_event(&mut e, &cfg, &reg);
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
    let groups = aggregate(
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "tool", &query(0, 0, None));
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
    }
    let groups = aggregate(&reg, &cfg, "tool", &query(0, 0, None));
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
    hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("acme");
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
        hatel_core::claude::hook::process_event(&mut event, &cfg, &reg);
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
    hatel_core::claude::hook::process_event(&mut prompt, &cfg, &reg);
    let mut call = serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "T1", "tool_name": "Bash", "tool_use_id": "t1", "duration_ms": 5,
    });
    hatel_core::claude::hook::process_event(&mut call, &cfg, &reg);
    let mut agent = serde_json::json!({
        "hook_event_name": "SubagentStop", "session_id": "S", "cwd": "/tmp/x",
        "prompt_id": "T1", "agent_id": "a1", "agent_type": "general-purpose",
    });
    hatel_core::claude::hook::process_event(&mut agent, &cfg, &reg);
    let turn = |kind: &str| {
        aggregate(
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

#[test]
fn typed_records_reject_missing_and_invalid_values_before_storage() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("typed.toml");
    std::fs::write(
        &plugin,
        r#"
[[kind]]
name = "typed"
version = 2
fields = { name = { type = "string", required = true }, seconds = { type = "number", unit = "s" } }
group_key = "name"
measures = ["seconds"]
"#,
    )
    .unwrap();
    let cfg = config_in(dir.path(), vec![plugin]);
    let reg = build_registry(&cfg).unwrap();
    for payload in [
        serde_json::json!({"seconds": 0}),
        serde_json::json!({"name": "check", "seconds": "0"}),
        serde_json::json!({"name": "check", "seconds": null}),
    ] {
        assert!(
            make_envelope(
                "typed",
                serde_json::from_value(payload).unwrap(),
                &reg,
                true
            )
            .is_err()
        );
    }
    for payload in [
        serde_json::json!({"name": "check"}),
        serde_json::json!({"name": "check", "seconds": 0}),
    ] {
        assert!(
            make_envelope(
                "typed",
                serde_json::from_value(payload).unwrap(),
                &reg,
                true
            )
            .is_ok()
        );
    }
}

#[test]
fn receipts_follow_storage_and_metadata_survives_each_backend() {
    for backend in [SinkKind::Jsonl, SinkKind::Sqlite] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            sink: backend,
            ..config_in(dir.path(), vec![])
        };
        let reg = build_registry(&cfg).unwrap();
        let mut env = make_envelope("prompt", Payload::new(), &reg, true).unwrap();
        env.apply_context(hatel_core::model::RecordContext {
            occurred_at: Some("2026-01-01T00:00:00Z".into()),
            run_id: Some("run".into()),
            ..Default::default()
        })
        .unwrap();
        let receipt = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
            .write_record(&env)
            .unwrap();
        assert!(receipt.record_id.is_some());
        let stored = hatel_core::sink::read_records(&cfg, "prompt", None);
        assert_eq!(stored, vec![env]);
    }
    let dir = tempfile::tempdir().unwrap();
    let not_directory = dir.path().join("file");
    std::fs::write(&not_directory, "occupied").unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        state_dir: not_directory,
        ..config_in(dir.path(), vec![])
    };
    let reg = build_registry(&cfg).unwrap();
    let env = make_envelope("prompt", Payload::new(), &reg, true).unwrap();
    assert!(
        hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
            .write_record(&env)
            .is_err()
    );
}

#[test]
fn jsonl_import_is_atomic_repeatable_and_preserves_identical_legacy_events() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("ledger");
    std::fs::create_dir(&source).unwrap();
    let line = r#"{"ts":"2026-01-01T00:00:00Z","kind":"prompt","payload":{"session_id":"s"}}"#;
    let path = source.join("prompt.jsonl");
    std::fs::write(&path, format!("{line}\n{line}\ninvalid\n")).unwrap();
    let db = dir.path().join("telemetry.db");
    let preflight = hatel_core::migration::check(&config_in(dir.path(), vec![]))
        .unwrap_err()
        .to_string();
    assert!(preflight.contains("prompt.jsonl:3: invalid or unsupported envelope"));
    assert!(!db.exists());
    assert!(hatel_core::migration::import_jsonl(&source, &db).is_err());
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let original = format!("{line}\n{line}\n");
    std::fs::write(&path, &original).unwrap();
    assert_eq!(
        hatel_core::migration::import_jsonl(&source, &db).unwrap(),
        2
    );
    assert_eq!(
        hatel_core::migration::import_jsonl(&source, &db).unwrap(),
        0
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(DISTINCT json_extract(metadata, '$.record_id')) FROM records",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    std::fs::write(&path, format!("{original}{line}\n")).unwrap();
    assert!(hatel_core::migration::import_jsonl(&source, &db).is_err());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn query_reports_zero_missing_invalid_and_duration_buckets_without_coercion() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
        .flush()
        .unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
    for payload in [
        serde_json::json!({"duration_ms": 0}),
        serde_json::json!({"duration_ms": 1000}),
        serde_json::json!({}),
        serde_json::json!({"duration_ms": "1000"}),
    ] {
        conn.execute(
            "INSERT INTO records(ts,kind,schema_version,payload) VALUES (?1,'tool',1,?2)",
            rusqlite::params!["2026-01-01T00:00:00Z", payload.to_string()],
        )
        .unwrap();
    }
    let reg = build_registry(&cfg).unwrap();
    let groups =
        hatel_core::query::aggregate(&cfg, reg.kind("tool").unwrap(), &legacy_query(0, 0, None))
            .unwrap();
    let measure = &groups[0].sums[0];
    assert_eq!(groups[0].key, None);
    assert_eq!(
        (
            measure.valid_count,
            measure.missing_count,
            measure.invalid_count
        ),
        (2, 1, 1)
    );
    assert_eq!(measure.sum, 1000.0);
    assert_eq!(measure.avg, Some(500.0));
    assert_eq!(measure.min, Some(0.0));
    assert_eq!(measure.max, Some(1000.0));
    assert_eq!(measure.buckets[0].count, 1);
    assert_eq!(measure.buckets[2].count, 2);
}

#[test]
fn numeric_aggregation_reports_overflow_and_accepts_the_signed_integer_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
        .flush()
        .unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
    let registry = build_registry(&cfg).unwrap();
    for value in [f64::MAX, f64::MAX] {
        conn.execute("INSERT INTO records(ts,kind,schema_version,payload) VALUES ('2026-01-01T00:00:00Z','tool',1,?1)", [serde_json::json!({"duration_ms":value}).to_string()]).unwrap();
    }
    assert!(
        hatel_core::query::aggregate(
            &cfg,
            registry.kind("tool").unwrap(),
            &legacy_query(0, 0, None)
        )
        .is_err()
    );
    conn.execute("DELETE FROM records", []).unwrap();
    conn.execute("INSERT INTO records(ts,kind,schema_version,payload) VALUES ('2026-01-01T00:00:00Z','tool',1,?1)", [serde_json::json!({"duration_ms":i64::MIN}).to_string()]).unwrap();
    let groups = hatel_core::query::aggregate(
        &cfg,
        registry.kind("tool").unwrap(),
        &legacy_query(0, 0, None),
    )
    .unwrap();
    assert_eq!(groups[0].sums[0].valid_count, 1);
    assert_eq!(groups[0].sums[0].sum, i64::MIN as f64);
}

fn export_config(dir: &Path, policy: &str) -> hatel_core::ExportConfig {
    let path = dir.join("exports.toml");
    std::fs::write(&path, policy).unwrap();
    hatel_core::ExportConfig::from_settings(&hatel_core::Settings::read(&path).unwrap()).unwrap()
}

#[test]
fn selected_sanitized_records_and_pending_delivery_commit_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    cfg.export = Ok(export_config(
        dir.path(),
        "[[export]]\nendpoint='http://company:4318'\nmode='raw'\nsignals=['metrics']\nkinds=['prompt']\nprojects=['company']\n",
    ));
    let reg = build_registry(&cfg).unwrap();
    let mut env = make_envelope("prompt", Payload::new(), &reg, true).unwrap();
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    assert_eq!(sink.write_record(&env).unwrap().delivery.queued, 0);
    env.metadata.record_id = Some("personal".into());
    env.metadata.project = Some(hatel_core::ProjectRef {
        key: "/personal".into(),
        label: "personal".into(),
    });
    assert_eq!(sink.write_record(&env).unwrap().delivery.queued, 0);
    env.metadata.record_id = Some("company".into());
    env.metadata.project = Some(hatel_core::ProjectRef {
        key: "/company".into(),
        label: "company".into(),
    });
    assert_eq!(sink.write_record(&env).unwrap().delivery.queued, 1);
    env.metadata.record_id = Some("projection".into());
    env.metadata.projection = true;
    assert_eq!(sink.write_record(&env).unwrap().delivery.queued, 0);
    drop(sink);
    let states = hatel_core::outbox::states(&dir.path().join("telemetry.db")).unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].records, 1);
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_delivery BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT,'delivery failed'); END;").unwrap();
    env.metadata.projection = false;
    env.metadata.record_id = Some("rolled-back".into());
    assert!(
        hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
            .write_record(&env)
            .is_err()
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM records WHERE json_extract(metadata,'$.record_id')='rolled-back'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    hatel_core::outbox::finish(
        &conn,
        "http://company:4318",
        "company",
        hatel_core::now_epoch(),
        hatel_core::outbox::Outcome::Delivered,
    )
    .unwrap();
    hatel_core::outbox::finish(
        &conn,
        "http://company:4318",
        "company",
        hatel_core::now_epoch(),
        hatel_core::outbox::Outcome::Delivered,
    )
    .unwrap();
    assert_eq!(
        conn.query_row("SELECT pending_records FROM delivery_queues", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn a_full_delivery_queue_keeps_the_local_record_and_exposes_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    cfg.export = Ok(export_config(
        dir.path(),
        "[[export]]\nendpoint='http://company:4318'\nmode='raw'\nsignals=[]\nkinds=['prompt']\nqueue_bytes=1024\n",
    ));
    let reg = build_registry(&cfg).unwrap();
    let env = make_envelope(
        "prompt",
        [("session_id".into(), serde_json::json!("x".repeat(2048)))].into(),
        &reg,
        true,
    )
    .unwrap();
    let receipt = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
        .write_record(&env)
        .unwrap();
    assert_eq!(receipt.delivery.queued, 0);
    assert_eq!(receipt.delivery.not_queued, 1);
    assert_eq!(
        hatel_core::sink::read_records(&cfg, "prompt", None).len(),
        1
    );
    assert_eq!(
        hatel_core::outbox::states(&dir.path().join("telemetry.db")).unwrap()[0].status,
        "capacity"
    );
}

#[test]
fn retention_removes_pending_references_and_releases_queue_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    cfg.export = Ok(export_config(
        dir.path(),
        "[[export]]\nendpoint='http://company:4318'\nmode='raw'\nkinds=['prompt']\n",
    ));
    let reg = build_registry(&cfg).unwrap();
    let mut env = make_envelope("prompt", Payload::new(), &reg, true).unwrap();
    env.ts = "2026-01-01T00:00:00Z".into();
    hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap())
        .write_record(&env)
        .unwrap();
    assert_eq!(
        hatel_core::sink::prune(&cfg, cfg.retention(hatel_core::now_epoch())),
        1
    );
    assert!(
        hatel_core::outbox::states(&dir.path().join("telemetry.db"))
            .unwrap()
            .is_empty()
    );
    let conn = rusqlite::Connection::open(dir.path().join("telemetry.db")).unwrap();
    assert_eq!(
        conn.query_row("SELECT pending_records FROM delivery_queues", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn common_metadata_separates_equal_project_labels_and_filters_runs_without_payload_fields() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let registry = build_registry(&cfg).unwrap();
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    for (key, run) in [("/team-one/repo", "first"), ("/team-two/repo", "second")] {
        let mut record = make_envelope("prompt", Payload::new(), &registry, true).unwrap();
        record.metadata.project = Some(hatel_core::ProjectRef {
            key: key.into(),
            label: "repo".into(),
        });
        record.metadata.run_id = Some(run.into());
        sink.write_record(&record).unwrap();
    }
    let spec = registry.kind("prompt").unwrap();
    let q = report::Query {
        group_by: Some("metadata.project.key"),
        ..query(0, 0, None)
    };
    let result = hatel_core::query::aggregate(&cfg, spec, &q).unwrap();
    assert_eq!(result.len(), 2);
    let filters = [("metadata.run_id".into(), "first".into())];
    let q = report::Query {
        group_by: Some("metadata.project.key"),
        filters: &filters,
        ..query(0, 0, Some("/team-one/repo"))
    };
    let result = hatel_core::query::section(&cfg, spec, &q).unwrap();
    assert_eq!(result.groups.as_ref().unwrap().len(), 1);
    assert_eq!(
        result.groups.as_ref().unwrap()[0].key.as_deref(),
        Some("/team-one/repo")
    );
    assert!(result.retained_since.is_some());
    let filters = [("metadata.schema_id".into(), spec.schema_id.clone())];
    let q = report::Query {
        group_by: Some("metadata.schema_id"),
        filters: &filters,
        ..query(0, 0, None)
    };
    let groups = hatel_core::query::aggregate(&cfg, spec, &q).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].count, 2);
    assert_eq!(groups[0].key.as_deref(), Some(spec.schema_id.as_str()));
    let filters = [("metadata.schema_id".into(), "different-contract".into())];
    assert!(
        hatel_core::query::aggregate(
            &cfg,
            spec,
            &report::Query {
                filters: &filters,
                ..q
            }
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn project_scope_and_filters_keep_canonical_metadata_separate_from_redacted_legacy_payloads() {
    for backend in [SinkKind::Jsonl, SinkKind::Sqlite] {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("private.toml");
        std::fs::write(&plugin,"[[kind]]\nname='private'\nfields=['project']\ngroup_key='project'\nredact=['project']\n").unwrap();
        let cfg = Config {
            sink: backend,
            ..config_in(dir.path(), vec![plugin])
        };
        let registry = build_registry(&cfg).unwrap();
        let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
        for canonical in [true, false] {
            let mut record = make_envelope(
                "private",
                [("project".into(), serde_json::json!("repo"))].into(),
                &registry,
                true,
            )
            .unwrap();
            if canonical {
                record.metadata.project = Some(hatel_core::ProjectRef {
                    key: "/team/repo".into(),
                    label: "repo".into(),
                });
            }
            sink.write_record(&record).unwrap();
        }
        let spec = registry.kind("private").unwrap();
        let result = hatel_core::query::section(&cfg, spec, &query(0, 0, Some("repo"))).unwrap();
        assert_eq!(
            result
                .groups
                .as_ref()
                .unwrap()
                .iter()
                .map(|group| group.count)
                .sum::<i64>(),
            2
        );
        assert!(result.retained_since.is_some());
        for (project, expected) in [("/team/repo", 1), ("repo", 1), ("other", 0)] {
            let filters = [("project".into(), project.into())];
            let groups = hatel_core::query::aggregate(
                &cfg,
                spec,
                &report::Query {
                    filters: &filters,
                    ..query(0, 0, None)
                },
            )
            .unwrap();
            assert_eq!(
                groups.iter().map(|group| group.count).sum::<i64>(),
                expected
            );
        }
    }
}

#[test]
fn period_request_cost_uses_occurrence_records_and_never_a_session_lifetime_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        sink: SinkKind::Sqlite,
        ..config_in(dir.path(), vec![])
    };
    let registry = build_registry(&cfg).unwrap();
    let now = hatel_core::now_epoch();
    let mut sink = hatel_core::build_sink(&cfg, &build_registry(&cfg).unwrap());
    for (request, age, cost) in [("old", 14 * 86400, 99.0), ("recent", 0, 0.02)] {
        let mut env = make_envelope(
            "claude.api_request",
            [
                ("session_id".into(), serde_json::json!("session")),
                ("request_id".into(), serde_json::json!(request)),
                ("cost_usd".into(), serde_json::json!(cost)),
            ]
            .into(),
            &registry,
            true,
        )
        .unwrap();
        env.ts = jiff::Timestamp::from_second(now - age).unwrap().to_string();
        sink.write_record(&env).unwrap();
    }
    let q = report::Query {
        kind: Some("claude.api_request"),
        ..query(now - 7 * 86400, 0, None)
    };
    let report = report::Report::build(&registry, &cfg, "7d", &q);
    assert_eq!(
        report.cost_basis,
        "session_lifetime_totals_selected_by_last_seen"
    );
    assert!(report.cost.is_empty());
    assert_eq!(report.kinds[0].groups[0].sums[0].sum, 0.02);
    assert_eq!(report.kinds[0].groups[0].count, 1);
}
