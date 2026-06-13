//! `doctor` — verify the Claude Code ↔ collector wiring and report policy gaps honestly. It
//! never guesses or papers over a missing signal: when a managed policy disables `session.id`
//! or blocks hooks, it says so and explains the consequence, rather than inventing a fallback.
//! All settings knowledge is shared with `init` via `claude_settings`. The exit code is non-zero
//! when a hard requirement fails, so scripts and CI can gate on it; advisory notes don't fail it.

use std::path::Path;

use hatel_core::{Config, ExportConfig, ExportMode, SessionIndex};

use crate::claude_settings as cs;

/// A resolved entry from the merged settings `env`: `&(value, source-scope)`.
type EnvEntry<'a> = &'a (String, &'static str);

/// Window for the dormant-binding note: a wired binding with no records while sessions HAVE
/// been starting is worth pointing out (the upstream event may have been renamed, or its payload
/// reshaped — both fail silently otherwise).
const DORMANT_WINDOW_DAYS: i64 = 7;

pub fn run() -> i32 {
    let files = cs::scope_files();
    let env = cs::effective_env(&files);
    // The events worth wiring depend on which Kinds are loaded (a plugin may bind more), so derive
    // them from the registry rather than the full vocabulary — coverage is judged against these.
    let events = cs::active_events_default();
    let cfg = Config::load();
    let mut ok = true;

    println!("hatel doctor\n");

    println!("settings files:");
    for f in &files {
        println!(
            "  {:<8} {:<22} {}",
            f.name,
            f.load.label(),
            f.path.display()
        );
    }
    println!();

    println!("native telemetry (settings.json env):");
    // Must be `otlp` specifically — `console`/`none` parse as healthy but never reach this
    // receiver.
    ok &= check_env(&env, "CLAUDE_CODE_ENABLE_TELEMETRY", Some("1"));
    ok &= check_env(&env, "OTEL_METRICS_EXPORTER", Some("otlp"));
    ok &= check_env(&env, "OTEL_LOGS_EXPORTER", Some("otlp"));
    ok &= check_endpoint_present(&env);
    ok &= advise_protocol(&env);
    advise_session_id(&env);
    println!();

    println!("hooks:");
    ok &= report_hooks(&files, &events);
    advise_dormant_bindings(&files, &events, &cfg);
    println!();

    println!("storage:");
    match writable(&cfg.state_dir) {
        Ok(()) => println!("  ✓ state dir writable: {}", cfg.state_dir.display()),
        Err(e) => {
            println!(
                "  ✗ state dir not writable ({}): {e}",
                cfg.state_dir.display()
            );
            ok = false;
        }
    }
    println!();

    ok &= report_export(&env);

    println!(
        "to wire automatically run `hatel init` — or paste this into managed settings for an org:\n"
    );
    print!("{}", cs::render_snippet(&cs::hook_command(), &events));

    if !ok {
        eprintln!("\ndoctor: the wiring is incomplete (see ✗ above)");
    }
    i32::from(!ok)
}

/// Report hook coverage across the canonical lifecycle events. Full coverage in an honored scope
/// is the requirement; partial coverage is a failure because the uncovered events are silently
/// not captured. Returns whether the requirement is met.
fn report_hooks(files: &[cs::ScopeFile], events: &[&'static str]) -> bool {
    let covered = cs::covered_events(files, events);
    let total = events.len();
    let managed_only = cs::managed_hooks_only(files);
    let mut ok = true;

    if covered.len() == total {
        println!("  ✓ all {total} lifecycle events invoke `hatel-hook`");
    } else if !covered.is_empty() {
        // Partial coverage, reported before the "blocked" case so it is never masked.
        let missing: Vec<&str> = events
            .iter()
            .copied()
            .filter(|e| !covered.contains(e))
            .collect();
        print!(
            "  ✗ only {}/{total} events wired — missing {}",
            covered.len(),
            missing.join(", ")
        );
        if managed_only {
            println!(
                "; deploy the rest as MANAGED hooks (allowManagedHooksOnly ignores lower scopes)"
            );
        } else {
            println!("; re-run `hatel init`");
        }
        ok = false;
    } else if cs::hook_wired_but_blocked(files) {
        println!(
            "  ✗ a hook invokes `hatel-hook` but is BLOCKED by allowManagedHooksOnly — \
             deploy it as a MANAGED hook (IT/MDM) or no events are captured"
        );
        ok = false;
    } else {
        println!("  ✗ no hook invokes `hatel-hook` — events are not captured");
        if managed_only {
            println!("    (allowManagedHooksOnly is set: the hook must be in the managed scope)");
        } else {
            println!("    wire it with `hatel init`");
        }
        ok = false;
    }

    // A wired hook whose absolute path no longer resolves silently stops collection, while the
    // basename-based coverage check above still counts it — so a moved/reinstalled binary doesn't
    // pass as healthy. (`hatel init` repoints it.)
    for cmd in cs::wired_hook_commands(files) {
        let p = Path::new(&cmd);
        if p.is_absolute() && !p.exists() {
            println!(
                "  ✗ wired hook `{cmd}` is missing on disk — re-run `hatel init` to repoint it"
            );
            ok = false;
        }
    }
    // A plugin can bind any event string, but `init` only wires the events hatel knows how to wire;
    // a binding outside that set never fires, so the plugin's Kind would silently collect nothing.
    for ev in cs::unwireable_bindings() {
        println!(
            "  ✗ a plugin binds `{ev}`, which hatel does not wire — that binding never fires \
             (the event isn't in the supported set; remove it or use a supported event)"
        );
        ok = false;
    }
    ok
}

/// Informational only, never a failure: a wired, hook-bound Kind that produced no records in the
/// recent window, while sessions HAVE been starting (the index advanced). Both readings are
/// stated because both are real — a rare event (PreCompact can stay quiet for weeks) and a
/// silently dead binding (Claude Code renamed the event or reshaped its payload) look identical
/// from here; the point is that the silence is *visible* where an operator already looks.
/// Grouped per Kind (records carry no event provenance, so Kind-level is the honest granularity
/// when one Kind is bound to several events) and gated on session recency — "no records" carries
/// no signal when nothing has been running.
fn advise_dormant_bindings(files: &[cs::ScopeFile], events: &[&'static str], cfg: &Config) {
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
            println!(
                "  • {} → {kind}: wired, but no records in the last {DORMANT_WINDOW_DAYS}d — \
                 either the event hasn't fired, or its payload no longer matches the binding",
                evs.join(", ")
            );
        }
    }
}

/// Report the configured egress destinations and — decisively — whether the OTel stream actually
/// reaches this receiver. Export forwards *from* the receiver, so a configured export does nothing
/// if the endpoint bypasses hatel; that, and an invalid config file, are hard failures. The
/// egress-privacy and enriched-protocol notes are advisory. Prints nothing when no export is
/// configured. Returns whether the export wiring is functional.
fn report_export(env: &cs::Env) -> bool {
    let export = match ExportConfig::load() {
        Ok(c) => c,
        Err(e) => {
            println!("export:");
            println!("  ✗ export config is invalid ({e}) — `serve` will refuse to start");
            println!();
            return false;
        }
    };
    if export.targets.is_empty() {
        return true; // nothing configured — no section, no failure
    }

    println!("export:");
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
        println!(
            "  • {} ({}{}{})",
            t.endpoint,
            t.mode.as_str(),
            filter,
            headers
        );
    }
    // Egress is the one place data leaves the host, and hatel does not redact the forwarded body.
    println!("  ⚠ egress forwards the raw OTLP stream off this host — hatel does not redact it");
    // The config writer sets owner-only permissions (0o600) on Unix; Windows has no mode-bit
    // equivalent here, so a config holding auth headers deserves an honest heads-up.
    if cfg!(windows) && export.targets.iter().any(|t| !t.headers.is_empty()) {
        println!(
            "  ⚠ this export config carries auth headers, and on Windows hatel cannot restrict \
             the config file's permissions — protect it with file ACLs"
        );
    }

    let mut ok = true;

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
            println!(
                "  ⚠ enriched/filtered export needs http/json input — OTEL_EXPORTER_OTLP_PROTOCOL is {} → those targets forward nothing",
                proto.unwrap_or("unset")
            );
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
        ok &= report_route(None, metrics); // one effective endpoint for both signals (the common case)
    } else {
        ok &= report_route(Some("metrics"), metrics);
        ok &= report_route(Some("logs"), logs);
    }
    println!();
    ok
}

/// Report whether one OTLP route reaches this receiver. `signal` labels a per-signal route
/// (metrics/logs) or `None` for the single shared endpoint. A managed-locked endpoint is unfixable
/// here; a user/project/local one is fixable via `init --insert`. Returns whether the route is
/// functional for export.
fn report_route(signal: Option<&str>, endpoint: Option<EnvEntry<'_>>) -> bool {
    let at = signal.map(|s| format!("{s} ")).unwrap_or_default();
    match endpoint {
        Some((endpoint, _)) if cs::is_local_receiver(endpoint) => {
            println!(
                "  ✓ {at}OTel is routed through this receiver — export has a stream to forward"
            );
            true
        }
        Some((endpoint, scope)) if *scope == "managed" => {
            println!(
                "  ✗ {at}endpoint is managed-locked to {endpoint} — OTel can't be routed through hatel, so export forwards nothing (only the hook ledger is available)"
            );
            false
        }
        Some((endpoint, scope)) => {
            println!(
                "  ✗ {at}OTel goes directly to {endpoint} (from {scope}), bypassing this receiver — export forwards nothing; run `hatel init --insert` to route it through hatel"
            );
            false
        }
        // Unset is the native section's call (it requires an explicit endpoint); here it's only
        // advisory, since an unset endpoint falls back to the OTel default rather than a definite
        // bypass — don't double-fail it.
        None => {
            println!(
                "  ⚠ {at}OTLP endpoint is unset — run `hatel init` to point it explicitly at this receiver"
            );
            true
        }
    }
}

/// Print a ✓/✗ for an env key and return whether it passed (a hard requirement).
fn check_env(env: &cs::Env, key: &str, want: Option<&str>) -> bool {
    match env.get(key) {
        Some((val, scope)) if want.is_none_or(|w| w == val) => {
            println!("  ✓ {key}={val} (from {scope})");
            true
        }
        Some((val, scope)) => {
            println!("  ✗ {key}={val} (from {scope}); expected {}", want.unwrap());
            false
        }
        None => {
            println!("  ✗ {key} unset");
            false
        }
    }
}

/// At least one OTLP endpoint must be set. The general `OTEL_EXPORTER_OTLP_ENDPOINT` is the
/// canonical setup (`hatel init` writes it); a per-signal override (`…_METRICS_ENDPOINT` /
/// `…_LOGS_ENDPOINT`) is honored too rather than mis-reported as unset.
fn check_endpoint_present(env: &cs::Env) -> bool {
    for key in [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    ] {
        if let Some((val, scope)) = env.get(key) {
            println!("  ✓ {key}={val} (from {scope})");
            return true;
        }
    }
    println!(
        "  ✗ no OTLP endpoint set (OTEL_EXPORTER_OTLP_ENDPOINT, or a per-signal …_METRICS_ENDPOINT / …_LOGS_ENDPOINT)"
    );
    false
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
/// collector's business. So this is a hard failure only in the local case — returns whether
/// it passed.
fn advise_protocol(env: &cs::Env) -> bool {
    // `http/json` is mandatory if EITHER effective signal endpoint reaches the local receiver — a
    // per-signal override could route one signal to hatel even when the general endpoint is remote.
    let (metrics, logs) = effective_otlp_endpoints(env);
    let local = [metrics, logs]
        .into_iter()
        .flatten()
        .any(|(v, _)| cs::is_local_receiver(v));
    match env.get("OTEL_EXPORTER_OTLP_PROTOCOL") {
        Some((v, scope)) if v == "http/json" => {
            println!("  ✓ OTEL_EXPORTER_OTLP_PROTOCOL=http/json (from {scope})");
            true
        }
        Some((v, scope)) if local => {
            println!(
                "  ✗ OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); the local receiver only decodes http/json"
            );
            false
        }
        Some((v, scope)) => {
            println!(
                "  ⚠ OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); http/json is required for the local receiver"
            );
            true
        }
        None if local => {
            println!("  ✗ OTEL_EXPORTER_OTLP_PROTOCOL unset; the local receiver needs http/json");
            false
        }
        None => {
            println!(
                "  ⚠ OTEL_EXPORTER_OTLP_PROTOCOL unset; set it to http/json for the local receiver"
            );
            true
        }
    }
}

fn advise_session_id(env: &cs::Env) {
    match env.get("OTEL_METRICS_INCLUDE_SESSION_ID") {
        Some((v, scope)) if v == "false" => println!(
            "  ⚠ OTEL_METRICS_INCLUDE_SESSION_ID=false (from {scope}): per-session/project \
             attribution is impossible. Only org/user aggregates remain."
        ),
        _ => println!("  ✓ session.id included in metrics (default on)"),
    }
}

fn writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".doctor_write_probe");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)
}
