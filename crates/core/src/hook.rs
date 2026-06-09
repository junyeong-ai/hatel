//! The hook entrypoint. Reads one Claude Code lifecycle event as stdin JSON, maps
//! it to records through the registry's declarative bindings, and writes them via
//! the active sink. It ALWAYS returns 0 — telemetry never blocks a tool call — and
//! every failure degrades to a stderr notice (an honest gap, never a fabricated value).

use crate::registry::Registry;
use crate::schema::build_registry_resilient;
use crate::session::SessionIndex;
use crate::sink::build_sink;
use crate::{Config, Payload, ProjectRef, make_envelope, project, resolve_project};

pub fn run_hook(stdin: &str) -> i32 {
    let cfg = Config::load();
    if cfg.disabled {
        return 0;
    }
    let mut event: serde_json::Value =
        serde_json::from_str(stdin).unwrap_or(serde_json::Value::Null);
    // Resilient build: a misconfigured plugin is skipped (logged) so it can never
    // silence core collection; the strict error still surfaces via `kinds` / `doctor`.
    let registry = build_registry_resilient(&cfg);
    process_event(&mut event, &cfg, &registry);
    0
}

/// Process one already-parsed event against an explicit config and registry. This
/// is the testable seam: it performs no I/O configuration of its own, so tests can
/// drive it with a temp-dir config and a hand-built registry.
pub fn process_event(event: &mut serde_json::Value, cfg: &Config, registry: &Registry) {
    let event_name = string_field(event, "hook_event_name");
    if event_name.is_empty() {
        return;
    }
    let cwd = string_field(event, "cwd");
    let session_id = string_field(event, "session_id");
    let project = resolve_project(&cwd);

    let bindings = registry.bindings_for(&event_name);

    // The git branch is a dimension neither OTel nor the hook stdin carries; inject
    // it so a field map can derive e.g. a spec slug from `spec/<slug>`. Read it only
    // when a binding for this event actually maps from it — the common events
    // (tool/prompt/…) never touch the filesystem for a field they don't use.
    let needs_branch = bindings
        .iter()
        .any(|b| b.map.values().any(|m| m.references("git_branch")));
    if needs_branch
        && let Some(obj) = event.as_object_mut()
        && !obj.contains_key("git_branch")
        && let Some(branch) = project::git_branch(&cwd)
    {
        obj.insert("git_branch".to_string(), serde_json::Value::from(branch));
    }

    record_session(cfg, &event_name, &session_id, &project);

    if bindings.is_empty() {
        return;
    }
    let mut sink = build_sink(cfg);
    for binding in bindings {
        let mut payload = Payload::new();
        for (field, mapping) in &binding.map {
            if let Some(value) = mapping.apply(event) {
                payload.insert(field.clone(), value);
            }
        }
        // The hook owns project attribution (from cwd); inject it only into Kinds that
        // declare `project`, so a Kind that opts out isn't rejected under strict mode.
        if registry.kind(&binding.kind).is_some_and(|s| s.fields.contains("project")) {
            payload.insert(
                "project".to_string(),
                serde_json::Value::from(project.label.clone()),
            );
        }
        match make_envelope(&binding.kind, payload, registry, cfg.strict) {
            Ok(envelope) => sink.write_record(&envelope),
            Err(e) => eprintln!("hatel hook: {e}"),
        }
    }
    sink.flush();
}

/// The session → project join goes to the sink-independent index (not a Kind), so
/// the receiver can attribute project-less OTel data regardless of the sink. Only the
/// session start establishes it; that is all the receiver needs.
fn record_session(cfg: &Config, event_name: &str, session_id: &str, project: &ProjectRef) {
    if event_name == "SessionStart" && !session_id.is_empty() {
        SessionIndex::new(cfg.state_dir.clone()).record(session_id, project);
    }
}

fn string_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}
