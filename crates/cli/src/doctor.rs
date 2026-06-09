//! `doctor` — verify the Claude Code ↔ collector wiring and report policy gaps honestly. It
//! never guesses or papers over a missing signal: when a managed policy disables `session.id`
//! or blocks hooks, it says so and explains the consequence, rather than inventing a fallback.
//! All settings knowledge is shared with `init` via `claude_settings`. The exit code is non-zero
//! when a hard requirement fails, so scripts and CI can gate on it; advisory notes don't fail it.

use std::path::Path;

use hatel_core::Config;

use crate::claude_settings as cs;

pub fn run() -> i32 {
    let files = cs::scope_files();
    let env = cs::effective_env(&files);
    let mut ok = true;

    println!("hatel doctor\n");

    println!("settings files:");
    for f in &files {
        println!("  {:<8} {:<22} {}", f.name, f.load.label(), f.path.display());
    }
    println!();

    println!("native telemetry (settings.json env):");
    // Must be `otlp` specifically — `console`/`none` parse as healthy but never reach this
    // receiver.
    ok &= check_env(&env, "CLAUDE_CODE_ENABLE_TELEMETRY", Some("1"));
    ok &= check_env(&env, "OTEL_METRICS_EXPORTER", Some("otlp"));
    ok &= check_env(&env, "OTEL_LOGS_EXPORTER", Some("otlp"));
    ok &= check_present(&env, "OTEL_EXPORTER_OTLP_ENDPOINT");
    ok &= advise_protocol(&env);
    advise_session_id(&env);
    println!();

    println!("hooks:");
    ok &= report_hooks(&files);
    println!();

    println!("storage:");
    let cfg = Config::load();
    match writable(&cfg.state_dir) {
        Ok(()) => println!("  ✓ state dir writable: {}", cfg.state_dir.display()),
        Err(e) => {
            println!("  ✗ state dir not writable ({}): {e}", cfg.state_dir.display());
            ok = false;
        }
    }
    println!();

    println!(
        "to wire automatically run `hatel init` — or paste this into managed settings for an org:\n"
    );
    print!("{}", cs::render_snippet(&cs::hook_command()));

    if !ok {
        eprintln!("\ndoctor: the wiring is incomplete (see ✗ above)");
    }
    i32::from(!ok)
}

/// Report hook coverage across the canonical lifecycle events. Full coverage in an honored scope
/// is the requirement; partial coverage is a failure because the uncovered events are silently
/// not captured. Returns whether the requirement is met.
fn report_hooks(files: &[cs::ScopeFile]) -> bool {
    let covered = cs::covered_events(files);
    let total = cs::EVENTS.len();
    let managed_only = cs::managed_hooks_only(files);
    let mut ok = true;

    if covered.len() == total {
        println!("  ✓ all {total} lifecycle events invoke `hatel-hook`");
    } else if !covered.is_empty() {
        // Partial coverage, reported before the "blocked" case so it is never masked.
        let missing: Vec<&str> = cs::EVENTS.iter().copied().filter(|e| !covered.contains(e)).collect();
        print!("  ✗ only {}/{total} events wired — missing {}", covered.len(), missing.join(", "));
        if managed_only {
            println!("; deploy the rest as MANAGED hooks (allowManagedHooksOnly ignores lower scopes)");
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
            println!("  ✗ wired hook `{cmd}` is missing on disk — re-run `hatel init` to repoint it");
            ok = false;
        }
    }
    ok
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

fn check_present(env: &cs::Env, key: &str) -> bool {
    check_env(env, key, None)
}

/// `http/json` is mandatory only when the exporter points at the local receiver (it decodes
/// nothing else); when the endpoint is repointed elsewhere the protocol is the remote collector's
/// business. So this is a hard failure only in the local case — returns whether it passed.
fn advise_protocol(env: &cs::Env) -> bool {
    let local = env.get("OTEL_EXPORTER_OTLP_ENDPOINT").is_some_and(|(v, _)| cs::is_local_receiver(v));
    match env.get("OTEL_EXPORTER_OTLP_PROTOCOL") {
        Some((v, scope)) if v == "http/json" => {
            println!("  ✓ OTEL_EXPORTER_OTLP_PROTOCOL=http/json (from {scope})");
            true
        }
        Some((v, scope)) if local => {
            println!("  ✗ OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); the local receiver only decodes http/json");
            false
        }
        Some((v, scope)) => {
            println!("  ⚠ OTEL_EXPORTER_OTLP_PROTOCOL={v} (from {scope}); http/json is required for the local receiver");
            true
        }
        None if local => {
            println!("  ✗ OTEL_EXPORTER_OTLP_PROTOCOL unset; the local receiver needs http/json");
            false
        }
        None => {
            println!("  ⚠ OTEL_EXPORTER_OTLP_PROTOCOL unset; set it to http/json for the local receiver");
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
