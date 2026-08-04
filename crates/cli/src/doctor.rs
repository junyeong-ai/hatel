//! `doctor` — verify the Claude Code ↔ collector wiring and report policy gaps honestly. It
//! never guesses or papers over a missing signal: when a managed policy disables `session.id`
//! or blocks hooks, it says so and explains the consequence, rather than inventing a fallback.
//! All settings knowledge is shared with `init` via `claude_settings`. Every check lands in one
//! structured report rendered as the glyphed human text or, with `--json`, as stable
//! machine-readable JSON — the two views cannot disagree because they render the same findings.
//! The exit code is non-zero when a hard requirement fails, so scripts and CI can gate on it;
//! advisory notes don't fail it.

use std::path::Path;

use serde::Serialize;

use hatel_core::{Config, ExportConfig, ExportMode, SessionIndex, Settings, sink};

use crate::claude_settings as cs;

/// A resolved entry from the merged settings `env`: `&(value, source-scope)`.
type EnvEntry<'a> = &'a (String, &'static str);

/// Window for the dormant-binding note: a wired binding with no records while sessions HAVE
/// been starting is worth pointing out (the upstream event may have been renamed, or its payload
/// reshaped — both fail silently otherwise).
const DORMANT_WINDOW_DAYS: i64 = 7;

/// One finding's severity. `Fail` is the only status that flips the exit code — the
/// glyphs ✓ / ✗ / ⚠ / • are its human rendering, `"ok"`-style strings its JSON one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Fail,
    Warn,
    Note,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Fail => "✗",
            Status::Warn => "⚠",
            Status::Note => "•",
        }
    }
}

#[derive(Debug, Serialize)]
struct Finding {
    message: String,
    status: Status,
}

/// One diagnostic section: a stable machine `name` plus the human header it renders
/// under (presentation only — never serialized).
#[derive(Debug, Serialize)]
struct Section {
    name: &'static str,
    #[serde(skip)]
    header: &'static str,
    findings: Vec<Finding>,
}

impl Section {
    fn new(name: &'static str, header: &'static str) -> Self {
        Section {
            name,
            header,
            findings: Vec::new(),
        }
    }
    fn push(&mut self, status: Status, message: impl Into<String>) {
        self.findings.push(Finding {
            message: message.into(),
            status,
        });
    }
    fn ok(&mut self, message: impl Into<String>) {
        self.push(Status::Ok, message);
    }
    fn fail(&mut self, message: impl Into<String>) {
        self.push(Status::Fail, message);
    }
    fn warn(&mut self, message: impl Into<String>) {
        self.push(Status::Warn, message);
    }
    fn note(&mut self, message: impl Into<String>) {
        self.push(Status::Note, message);
    }
}

#[derive(Debug, Serialize)]
struct SettingsFileInfo {
    load: String,
    name: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct Report {
    ok: bool,
    sections: Vec<Section>,
    settings_files: Vec<SettingsFileInfo>,
    /// The managed-settings paste block — the fix `init` would apply, carried in the
    /// report so a machine consumer gets the remedy alongside the findings.
    snippet: String,
}

/// The full report as a JSON value — the `--json` payload, shared with the MCP tool.
pub(crate) fn report_value() -> serde_json::Value {
    serde_json::to_value(build_report()).unwrap_or_default()
}

pub fn run(json: bool) -> i32 {
    let report = build_report();
    if json {
        // Through `to_value` so object keys serialize alphabetically, like every other
        // machine output.
        let value = serde_json::to_value(&report).unwrap_or_default();
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        print!("{}", render_human(&report));
        if !report.ok {
            eprintln!("\ndoctor: the wiring is incomplete (see ✗ above)");
        }
    }
    i32::from(!report.ok)
}

fn build_report() -> Report {
    let files = cs::scope_files();
    let env = cs::effective_env(&files);
    // The events worth wiring depend on which Kinds are loaded (a plugin may bind more), so derive
    // them from the registry rather than the full vocabulary — coverage is judged against these.
    let events = cs::active_events_default();
    let settings = Settings::load();
    let cfg = Config::load_resilient();

    let settings_files = files
        .iter()
        .map(|f| SettingsFileInfo {
            load: f.load.label().to_string(),
            name: f.name.to_string(),
            path: f.path.display().to_string(),
        })
        .collect();

    let mut native = Section::new("native_telemetry", "native telemetry (settings.json env):");
    // Must be `otlp` specifically — `console`/`none` parse as healthy but never reach this
    // receiver.
    check_env(&mut native, &env, "CLAUDE_CODE_ENABLE_TELEMETRY", Some("1"));
    check_env(&mut native, &env, "OTEL_METRICS_EXPORTER", Some("otlp"));
    check_env(&mut native, &env, "OTEL_LOGS_EXPORTER", Some("otlp"));
    check_endpoint_present(&mut native, &env);
    advise_protocol(&mut native, &env);
    advise_session_id(&mut native, &env);

    let mut hooks = Section::new("hooks", "hooks:");
    report_hooks(&mut hooks, &files, &events);
    advise_dormant_bindings(&mut hooks, &files, &events, &cfg);

    let mut storage = Section::new("storage", "storage:");
    match writable(&cfg.state_dir) {
        Ok(()) => storage.ok(format!("state dir writable: {}", cfg.state_dir.display())),
        Err(e) => storage.fail(format!(
            "state dir not writable ({}): {e}",
            cfg.state_dir.display()
        )),
    }
    report_registry(&mut storage, &settings, &cfg);

    let mut sections = vec![native, hooks, storage];
    if let Some(export) = report_export(&env) {
        sections.push(export);
    }

    let ok = sections
        .iter()
        .flat_map(|s| &s.findings)
        .all(|f| f.status != Status::Fail);

    Report {
        ok,
        sections,
        settings_files,
        snippet: cs::render_snippet(&cs::hook_command(), &events),
    }
}

fn render_human(r: &Report) -> String {
    let mut out = String::from("hatel doctor\n\n");
    out.push_str("settings files:\n");
    for f in &r.settings_files {
        out.push_str(&format!("  {:<8} {:<22} {}\n", f.name, f.load, f.path));
    }
    out.push('\n');
    for s in &r.sections {
        out.push_str(s.header);
        out.push('\n');
        for f in &s.findings {
            out.push_str(&format!("  {} {}\n", f.status.glyph(), f.message));
        }
        out.push('\n');
    }
    out.push_str(
        "to wire automatically run `hatel init` — or paste this into managed settings for an org:\n\n",
    );
    out.push_str(&r.snippet);
    out
}

/// Report hook coverage across the canonical lifecycle events. Full coverage in an honored scope
/// is the requirement; partial coverage is a failure because the uncovered events are silently
/// not captured.
fn report_hooks(sec: &mut Section, files: &[cs::ScopeFile], events: &[&'static str]) {
    let covered = cs::covered_events(files, events);
    let total = events.len();
    let managed_only = cs::managed_hooks_only(files);

    if covered.len() == total {
        sec.ok(format!("all {total} lifecycle events invoke `hatel-hook`"));
    } else if !covered.is_empty() {
        // Partial coverage, reported before the "blocked" case so it is never masked.
        let missing: Vec<&str> = events
            .iter()
            .copied()
            .filter(|e| !covered.contains(e))
            .collect();
        let remedy = if managed_only {
            "; deploy the rest as MANAGED hooks (allowManagedHooksOnly ignores lower scopes)"
        } else {
            "; re-run `hatel init`"
        };
        sec.fail(format!(
            "only {}/{total} events wired — missing {}{remedy}",
            covered.len(),
            missing.join(", ")
        ));
    } else if cs::hook_wired_but_blocked(files) {
        sec.fail(
            "a hook invokes `hatel-hook` but is BLOCKED by allowManagedHooksOnly — \
             deploy it as a MANAGED hook (IT/MDM) or no events are captured",
        );
    } else {
        let remedy = if managed_only {
            "\n    (allowManagedHooksOnly is set: the hook must be in the managed scope)"
        } else {
            "\n    wire it with `hatel init`"
        };
        sec.fail(format!(
            "no hook invokes `hatel-hook` — events are not captured{remedy}"
        ));
    }

    // A wired hook whose absolute path no longer resolves silently stops collection, while the
    // basename-based coverage check above still counts it — so a moved/reinstalled binary doesn't
    // pass as healthy. (`hatel init` repoints it.)
    for cmd in cs::wired_hook_commands(files) {
        let p = Path::new(&cmd);
        if p.is_absolute() && !p.exists() {
            sec.fail(format!(
                "wired hook `{cmd}` is missing on disk — re-run `hatel init` to repoint it"
            ));
        }
    }
    // A plugin can bind any event string, but `init` only wires the events hatel knows how to wire;
    // a binding outside that set never fires, so the plugin's Kind would silently collect nothing.
    for ev in cs::unwireable_bindings() {
        sec.fail(format!(
            "a plugin binds `{ev}`, which hatel does not wire — that binding never fires \
             (the event isn't in the supported set; remove it or use a supported event)"
        ));
    }
}

/// Informational only, never a failure: a wired, hook-bound Kind that produced no records in the
/// recent window, while sessions HAVE been starting (the index advanced). Both readings are
/// stated because both are real — a rare event (PreCompact can stay quiet for weeks) and a
/// silently dead binding (Claude Code renamed the event or reshaped its payload) look identical
/// from here; the point is that the silence is *visible* where an operator already looks.
/// Grouped per Kind (records carry no event provenance, so Kind-level is the honest granularity
/// when one Kind is bound to several events) and gated on session recency — "no records" carries
/// no signal when nothing has been running.
/// Compare what the store holds against what the registry can read. A Kind whose records are on
/// disk but whose schema is not loaded is invisible to every query — the collection worked and
/// the reporting cannot see it — which no other check would surface, because nothing about the
/// wiring is wrong.
fn report_registry(sec: &mut Section, settings: &hatel_core::Result<Settings>, cfg: &Config) {
    let config_path = Settings::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no config directory)".to_string());
    if let Err(e) = settings {
        sec.fail(e.to_string());
        return;
    }
    // The strict build is what every read path performs, so its verdict is the one that decides
    // whether the configured schemas are usable; the resilient build then still supplies a
    // registry for the comparison below, which stays informative even with one plugin broken.
    match hatel_core::schema::build_registry(cfg) {
        Ok(_) => match cfg.plugins.len() {
            0 => sec.note(format!("no plugin schemas configured ({config_path})")),
            n => sec.ok(format!("{n} plugin schema(s) from {config_path}")),
        },
        Err(e) => sec.fail(format!("plugin schema in {config_path}: {e}")),
    }
    let registry = hatel_core::schema::build_registry_resilient(cfg);
    match sink::stored_kinds(cfg) {
        Err(e) => sec.fail(format!("cannot enumerate stored Kinds: {e}")),
        Ok(stored) => {
            let unreadable: Vec<String> = stored
                .into_iter()
                .filter(|k| registry.kind(k).is_none())
                .collect();
            if !unreadable.is_empty() {
                sec.warn(format!(
                    "stored but unreadable — no loaded schema declares {}; add the plugin that \
                     defines them to `plugins` in {config_path}, or their records stay uncountable",
                    unreadable.join(", ")
                ));
            }
        }
    }
}

fn advise_dormant_bindings(
    sec: &mut Section,
    files: &[cs::ScopeFile],
    events: &[&'static str],
    cfg: &Config,
) {
    let since = hatel_core::now_epoch() - DORMANT_WINDOW_DAYS * 86_400;
    let index_recent = SessionIndex::new(cfg.state_dir.clone())
        .newest_mtime()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| d.as_secs() as i64 >= since);
    if !index_recent {
        return;
    }
    // Resilient build, like the wiring derivation: a broken plugin must not silence doctor.
    let registry = hatel_core::schema::build_registry_resilient(cfg);
    let mut bound_kinds: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for ev in cs::covered_events(files, events) {
        for binding in registry.bindings_for(ev) {
            bound_kinds
                .entry(binding.kind.as_str())
                .or_default()
                .push(ev);
        }
    }
    for (kind, evs) in bound_kinds {
        let active = hatel_core::sink::read_records(cfg, kind, Some(since))
            .iter()
            .any(|r| hatel_core::ts_epoch(&r.ts).is_some_and(|t| t >= since));
        if !active {
            sec.note(format!(
                "{} → {kind}: wired, but no records in the last {DORMANT_WINDOW_DAYS}d — \
                 either the event hasn't fired, or its payload no longer matches the binding",
                evs.join(", ")
            ));
        }
    }
}

/// Report the configured egress destinations and — decisively — whether the OTel stream actually
/// reaches this receiver. Export forwards *from* the receiver, so a configured export does nothing
/// if the endpoint bypasses hatel; that, and an invalid config file, are hard failures. The
/// egress-privacy and enriched-protocol notes are advisory. Returns `None` when no export is
/// configured — no section, no failure.
fn report_export(env: &cs::Env) -> Option<Section> {
    let mut sec = Section::new("export", "export:");
    let export = match ExportConfig::load() {
        Ok(c) => c,
        Err(e) => {
            sec.fail(format!(
                "export config is invalid ({e}) — `serve` will refuse to start"
            ));
            return Some(sec);
        }
    };
    if export.targets.is_empty() {
        return None;
    }

    for t in &export.targets {
        let headers = match t.headers.len() {
            0 => String::new(),
            // The values may be secrets (auth tokens) — only the count is shown, never the value.
            n => format!(", {n} header(s)"),
        };
        let filter = t
            .filter
            .describe()
            .map(|d| format!(", {d}"))
            .unwrap_or_default();
        sec.note(format!(
            "{} ({}{}{})",
            t.endpoint,
            t.mode.as_str(),
            filter,
            headers
        ));
    }
    // Egress is the one place data leaves the host, and hatel does not redact the forwarded body.
    sec.warn("egress forwards the raw OTLP stream off this host — hatel does not redact it");
    // The config writer sets owner-only permissions (0o600) on Unix; Windows has no mode-bit
    // equivalent here, so a config holding auth headers deserves an honest heads-up.
    if cfg!(windows) && export.targets.iter().any(|t| !t.headers.is_empty()) {
        sec.warn(
            "this export config carries auth headers, and on Windows hatel cannot restrict \
             the config file's permissions — protect it with file ACLs",
        );
    }

    // Both enriching and filtering read the JSON body (to inject, or to resolve a batch's project
    // from its session.id); a non-JSON protocol makes an enriched target skip and a filtered target
    // fail closed (forward nothing). Flag the protocol that would do so.
    let needs_json = export
        .targets
        .iter()
        .any(|t| t.mode == ExportMode::Enriched || t.filter.is_filtered());
    if needs_json {
        let proto = env
            .get("OTEL_EXPORTER_OTLP_PROTOCOL")
            .map(|(v, _)| v.as_str());
        if proto != Some("http/json") {
            sec.warn(format!(
                "enriched/filtered export needs http/json input — OTEL_EXPORTER_OTLP_PROTOCOL is {} → those targets forward nothing",
                proto.unwrap_or("unset")
            ));
        }
    }

    // The decisive check: export forwards from this receiver, so the OTel stream must reach it.
    // A signal-specific endpoint (metrics/logs) overrides the general one, so check each — a
    // per-signal override could send one signal past hatel while the general endpoint looks clean.
    let (metrics, logs) = effective_otlp_endpoints(env);
    let same_destination = match (metrics, logs) {
        (Some((m, _)), Some((l, _))) => {
            hatel_core::export::normalize_endpoint(m) == hatel_core::export::normalize_endpoint(l)
        }
        (None, None) => true,
        _ => false,
    };
    if same_destination {
        report_route(&mut sec, None, metrics); // one effective endpoint for both signals (the common case)
    } else {
        report_route(&mut sec, Some("metrics"), metrics);
        report_route(&mut sec, Some("logs"), logs);
    }
    Some(sec)
}

/// Report whether one OTLP route reaches this receiver. `signal` labels a per-signal route
/// (metrics/logs) or `None` for the single shared endpoint. A managed-locked endpoint is unfixable
/// here; a user/project/local one is fixable via `init --insert`.
fn report_route(sec: &mut Section, signal: Option<&str>, endpoint: Option<EnvEntry<'_>>) {
    let at = signal.map(|s| format!("{s} ")).unwrap_or_default();
    match endpoint {
        Some((endpoint, _)) if cs::is_local_receiver(endpoint) => {
            sec.ok(format!(
                "{at}OTel is routed through this receiver — export has a stream to forward"
            ));
        }
        Some((endpoint, scope)) if *scope == "managed" => {
            sec.fail(format!(
                "{at}endpoint is managed-locked to {endpoint} — OTel can't be routed through hatel, so export forwards nothing (only the hook ledger is available)"
            ));
        }
        Some((endpoint, scope)) => {
            sec.fail(format!(
                "{at}OTel goes directly to {endpoint} (from {scope}), bypassing this receiver — export forwards nothing; run `hatel init --insert` to route it through hatel"
            ));
        }
        // Unset is the native section's call (it requires an explicit endpoint); here it's only
        // advisory, since an unset endpoint falls back to the OTel default rather than a definite
        // bypass — don't double-fail it.
        None => {
            sec.warn(format!(
                "{at}OTLP endpoint is unset — run `hatel init` to point it explicitly at this receiver"
            ));
        }
    }
}

/// Push a ✓/✗ finding for an env key (a hard requirement).
fn check_env(sec: &mut Section, env: &cs::Env, key: &str, want: Option<&str>) {
    match env.get(key) {
        Some((val, scope)) if want.is_none_or(|w| w == val) => {
            sec.ok(format!("{key}={val} (from {scope})"));
        }
        Some((val, scope)) => {
            sec.fail(format!(
                "{key}={val} (from {scope}); expected {}",
                want.unwrap()
            ));
        }
        None => {
            sec.fail(format!("{key} unset"));
        }
    }
}

/// At least one OTLP endpoint must be set. The general `OTEL_EXPORTER_OTLP_ENDPOINT` is the
/// canonical setup (`hatel init` writes it); a per-signal override (`…_METRICS_ENDPOINT` /
/// `…_LOGS_ENDPOINT`) is honored too rather than mis-reported as unset.
fn check_endpoint_present(sec: &mut Section, env: &cs::Env) {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    ] {
        if let Some((val, scope)) = env.get(key) {
            sec.ok(format!("{key}={val} (from {scope})"));
            return;
        }
    }
    sec.fail(
        "no OTLP endpoint set (OTEL_EXPORTER_OTLP_ENDPOINT, or a per-signal …_METRICS_ENDPOINT / …_LOGS_ENDPOINT)",
    );
}

/// The effective OTLP endpoint per signal: a per-signal override (`…_METRICS_ENDPOINT` /
/// `…_LOGS_ENDPOINT`) wins over the general `OTEL_EXPORTER_OTLP_ENDPOINT`. Returned as
/// `(metrics, logs)` so the protocol check and the export bypass check both reason per signal,
/// not just on the general endpoint.
fn effective_otlp_endpoints(env: &cs::Env) -> (Option<EnvEntry<'_>>, Option<EnvEntry<'_>>) {
    let general = env.get("OTEL_EXPORTER_OTLP_ENDPOINT");
    let metrics = env.get("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").or(general);
    let logs = env.get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").or(general);
    (metrics, logs)
}

/// `http/json` is mandatory only when the exporter points at the local receiver (it decodes
/// nothing else); when the endpoint is repointed elsewhere the protocol is the remote
/// collector's business. So this is a hard failure only in the local case.
fn advise_protocol(sec: &mut Section, env: &cs::Env) {
    // `http/json` is mandatory if EITHER effective signal endpoint reaches the local receiver — a
    // per-signal override could route one signal to hatel even when the general endpoint is remote.
    let (metrics, logs) = effective_otlp_endpoints(env);
    let local = [metrics, logs]
        .into_iter()
        .flatten()
        .any(|(v, _)| cs::is_local_receiver(v));
    match env.get("OTEL_EXPORTER_OTLP_PROTOCOL") {
        Some((v, scope)) if v == "http/json" => {
            sec.ok(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL=http/json (from {scope})"
            ));
        }
        Some((v, scope)) if local => {
            sec.fail(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); the local receiver only decodes http/json"
            ));
        }
        Some((v, scope)) => {
            sec.warn(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); http/json is required for the local receiver"
            ));
        }
        None if local => {
            sec.fail("OTEL_EXPORTER_OTLP_PROTOCOL unset; the local receiver needs http/json");
        }
        None => {
            sec.warn(
                "OTEL_EXPORTER_OTLP_PROTOCOL unset; set it to http/json for the local receiver",
            );
        }
    }
}

fn advise_session_id(sec: &mut Section, env: &cs::Env) {
    match env.get("OTEL_METRICS_INCLUDE_SESSION_ID") {
        Some((v, scope)) if v == "false" => sec.warn(format!(
            "OTEL_METRICS_INCLUDE_SESSION_ID=false (from {scope}): per-session/project \
             attribution is impossible — hatel drops these session-less metrics rather than guess."
        )),
        _ => sec.ok("session.id included in metrics (default on)"),
    }
}

fn writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".doctor_write_probe");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(status: Status) -> Report {
        let mut sec = Section::new("native_telemetry", "native telemetry (settings.json env):");
        sec.push(status, "CLAUDE_CODE_ENABLE_TELEMETRY=1 (from user)");
        let ok = status != Status::Fail;
        Report {
            ok,
            sections: vec![sec],
            settings_files: vec![SettingsFileInfo {
                load: "loaded".into(),
                name: "user".into(),
                path: "/tmp/settings.json".into(),
            }],
            snippet: "{}\n".into(),
        }
    }

    #[test]
    fn only_fail_findings_flip_ok() {
        for status in [Status::Ok, Status::Warn, Status::Note] {
            assert!(report_with(status).ok, "{status:?} is advisory");
        }
        assert!(!report_with(Status::Fail).ok, "a ✗ fails the report");
    }

    #[test]
    fn json_serializes_statuses_as_stable_names_and_skips_presentation() {
        let value = serde_json::to_value(report_with(Status::Fail)).unwrap();
        let finding = &value["sections"][0]["findings"][0];
        assert_eq!(finding["status"], "fail", "snake_case status, not a glyph");
        assert_eq!(value["sections"][0]["name"], "native_telemetry");
        assert!(
            value["sections"][0].get("header").is_none(),
            "the human header is presentation, not data"
        );
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn human_rendering_glyphs_each_status_and_keeps_the_section_layout() {
        let text = render_human(&report_with(Status::Fail));
        assert!(text.starts_with("hatel doctor\n\n"));
        assert!(text.contains("settings files:\n  user     loaded"));
        assert!(
            text.contains("native telemetry (settings.json env):\n  ✗ "),
            "got: {text}"
        );
        assert!(text.contains("to wire automatically run `hatel init`"));
    }
}
