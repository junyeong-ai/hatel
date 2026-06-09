//! `init` — wire this collector into Claude Code's `settings.json`. The merge is
//! non-destructive and idempotent (see `claude_settings::wire`): it adds the telemetry `env`
//! and the lifecycle hooks without touching a user's existing hooks or an endpoint they've
//! repointed elsewhere, so it is safe to re-run. `--print` emits the block for managed/org
//! settings instead of writing.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde_json::Value;

use crate::claude_settings as cs;

/// Where to write. `managed`/org settings are org-controlled and print-only, so they are not
/// a target here.
#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    /// `~/.claude/settings.json` — every project (recommended).
    User,
    /// `.claude/settings.json` — committed, shared with the team.
    Project,
    /// `.claude/settings.local.json` — this repo, per-dev.
    Local,
}

impl Scope {
    fn key(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Local => "local",
        }
    }
}

pub fn run(scope: Scope, print: bool, remove: bool) -> i32 {
    let hook_cmd = cs::hook_command();

    if print {
        print!("{}", cs::render_snippet(&hook_cmd));
        return 0;
    }

    let Some(path) = cs::scope_path(scope.key()) else {
        eprintln!("init: cannot resolve the {} settings path (no home directory?)", scope.key());
        return 1;
    };

    if remove {
        return remove_wiring(&path, scope);
    }

    // Start from the existing file, or an empty object if absent. Refuse to write over a file
    // we couldn't parse or that isn't an object — clobbering it would lose the user's settings.
    let mut settings = match cs::read_json(&path) {
        cs::Load::Found(v) if v.is_object() => v,
        cs::Load::Absent => Value::Object(Default::default()),
        cs::Load::Found(_) => {
            eprintln!("init: {} is not a JSON object; refusing to overwrite", path.display());
            return 1;
        }
        cs::Load::Malformed(e) => {
            eprintln!("init: {} is malformed ({e}); fix or remove it first", path.display());
            return 1;
        }
        cs::Load::Unreadable(e) => {
            eprintln!("init: {} is unreadable ({e})", path.display());
            return 1;
        }
    };

    let rep = cs::wire(&mut settings, &hook_cmd);

    // An advisory key the user set differently (e.g. an endpoint repointed at a corporate
    // collector) — telemetry still flows, so note it but don't fail.
    for (k, existing) in &rep.env_conflicts {
        eprintln!("  ⚠ {k} already set to {existing} — left as-is (re-point the receiver or edit by hand)");
    }
    // A required key set so telemetry can't flow (disabled / wrong exporter). We won't overwrite
    // the user's value, but the wiring isn't functional, so this drives a non-zero exit.
    for (k, existing) in &rep.env_blocked {
        eprintln!("  ✗ {k}={existing} keeps native telemetry off — left as-is; set it to the collector value to capture cost/tokens");
    }

    // Structural malformation is different again: refuse rather than persist a half-wired file.
    // This writes nothing (the in-memory value isn't saved on this path).
    if rep.malformed() {
        if rep.env_not_object {
            eprintln!("init: `env` in {} is not an object — fix it first", path.display());
        }
        if rep.hooks_not_object {
            eprintln!("init: `hooks` in {} is not an object — fix it first", path.display());
        }
        for ev in &rep.events_conflicts {
            eprintln!("init: hooks.{ev} in {} is not an array — fix it first", path.display());
        }
        return 1;
    }

    warn_if_hook_missing(&hook_cmd);

    if rep.changed() {
        if let Err(e) = write_atomic(&path, &settings) {
            eprintln!("init: failed to write {}: {e}", path.display());
            return 1;
        }
        println!("wired {} ({} scope)", path.display(), scope.key());
        if !rep.env_added.is_empty() {
            println!("  env:   +{}", rep.env_added.join(", "));
        }
        if !rep.events_added.is_empty() {
            println!("  hooks: +{}", rep.events_added.join(", "));
        }
    } else {
        println!("already wired: {}", path.display());
    }
    println!("verify with `hatel doctor`");

    // The hooks are in place, but a blocked required key means native telemetry won't be
    // captured — report that as failure so an installer or script can act on it.
    i32::from(!rep.env_blocked.is_empty())
}

/// Warn if the hook command we just wired points at a binary that isn't there — a CLI-only
/// install would otherwise leave Claude Code trying to spawn a missing file on every event. Only
/// checked for the absolute path we resolve beside the CLI; a bare name (resolution failed)
/// can't be checked against `PATH` here.
fn warn_if_hook_missing(hook_cmd: &str) {
    let p = Path::new(hook_cmd);
    if p.is_absolute() && !p.exists() {
        eprintln!(
            "  ⚠ the hook binary isn't at {hook_cmd} yet — install hatel-hook beside hatel"
        );
    }
}

/// Remove this collector's wiring from a scope (the inverse of wiring). Strips only our hook
/// entries; leaves the `env` block (Claude Code's native telemetry config) and reports it.
fn remove_wiring(path: &Path, scope: Scope) -> i32 {
    let mut settings = match cs::read_json(path) {
        cs::Load::Found(v) if v.is_object() => v,
        cs::Load::Absent => {
            println!("not wired: {} does not exist", path.display());
            return 0;
        }
        cs::Load::Found(_) => {
            eprintln!("init: {} is not a JSON object", path.display());
            return 1;
        }
        cs::Load::Malformed(e) => {
            eprintln!("init: {} is malformed ({e}); fix or remove it first", path.display());
            return 1;
        }
        cs::Load::Unreadable(e) => {
            eprintln!("init: {} is unreadable ({e})", path.display());
            return 1;
        }
    };

    let rep = cs::unwire(&mut settings);
    if !rep.changed() {
        println!("no hatel hooks in {}", path.display());
        return 0;
    }

    if let Err(e) = write_atomic(path, &settings) {
        eprintln!("init: failed to write {}: {e}", path.display());
        return 1;
    }

    println!("unwired {} ({} scope)", path.display(), scope.key());
    println!("  hooks: -{}", rep.events_cleared.join(", "));
    println!("  env left as-is — Claude Code's native telemetry config; remove by hand if you no longer want it");
    0
}

/// Write JSON atomically: serialize to a temp file beside the target, fsync, then rename over
/// it, so a crash mid-write can never leave a half-written settings file. Pretty-printed with a
/// trailing newline to stay diff- and editor-friendly.
fn write_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    text.push('\n');

    let tmp = tmp_sibling(path);
    {
        // Create the temp private (0o600) from the start, so the full contents never sit at a
        // world-readable default in the window before the final mode is applied.
        let mut f = create_private(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    inherit_mode(&tmp, path)?;
    std::fs::rename(&tmp, path)
}

/// Create (truncating) a file owner-only from the start — `0o600` on Unix. A no-op for mode bits
/// off Unix.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Don't widen permissions when rewriting: copy the existing file's mode onto the temp before it
/// replaces the original, so a user who locked their `settings.json` down (it may hold other
/// secrets) keeps that mode. A new file stays at the `0o600` it was created with — private by
/// default, since it's per-user config. No-op off Unix, where mode bits don't apply.
#[cfg(unix)]
fn inherit_mode(tmp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    if let Ok(meta) = std::fs::metadata(target) {
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn inherit_mode(_tmp: &Path, _target: &Path) -> std::io::Result<()> {
    Ok(())
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".init-tmp-{}", std::process::id()));
    path.with_file_name(name)
}
