//! Shared knowledge of Claude Code's `settings.json` wiring — the single source of truth
//! for the `env` block and the lifecycle hooks that connect Claude Code to this collector.
//! `doctor` reads through it to diagnose; `init` writes through it to wire. Diagnosis and
//! merge use the same scope discovery and the same structural hook check, so they can never
//! disagree about what "wired" means.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The hook binary spawned on every lifecycle event. Doubles as the structural marker that
/// identifies *our* hook inside a settings file, regardless of the absolute path it's wired
/// with.
const HOOK_BIN: &str = "hatel-hook";

/// The lifecycle events this collector binds. Claude Code deliberately does not pass the
/// `OTEL_*` telemetry env to hook subprocesses, which is why hooks carry the project context
/// and domain events the native layer can't — so these are the events worth a hook record.
pub const EVENTS: [&str; 8] = [
    "SessionStart",
    "SessionEnd",
    "PostToolUse",
    "UserPromptSubmit",
    "SubagentStop",
    "InstructionsLoaded",
    "PreCompact",
    "PostCompact",
];

/// The receiver's default bind address — the endpoint `init` wires. Used to tell "pointed at the
/// local receiver" (where `http/json` is mandatory) from "repointed elsewhere" (where it's the
/// remote collector's business).
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4318";

/// Whether an OTLP endpoint points at a loopback (local) receiver — where this collector is the
/// only thing that could be listening, so `http/json` is mandatory. Normalizes the host so an
/// alias (`localhost`, `::1`), a non-default port, or a trailing slash/path doesn't slip past as
/// "remote". Anything non-loopback is treated as a remote collector (protocol is its business).
pub fn is_local_receiver(endpoint: &str) -> bool {
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = if let Some(after) = authority.strip_prefix('[') {
        after.split(']').next().unwrap_or(after) // [::1]:4318 → ::1
    } else {
        authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// The native-telemetry `env` block Claude Code reads at session start. The bool marks a
/// *required* key: if the user already has it set to a different value, native telemetry reaches
/// no collector at all (it's disabled, or routed to a non-OTLP exporter), so `wire` reports that
/// as *blocking* rather than advisory. The endpoint and protocol are advisory — a corporate
/// collector is a legitimate repoint, and `http/json` is required only for *this* receiver, which
/// the user may not be pointing at.
const TELEMETRY_ENV: [(&str, &str, bool); 5] = [
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "1", true),
    ("OTEL_METRICS_EXPORTER", "otlp", true),
    ("OTEL_LOGS_EXPORTER", "otlp", true),
    ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json", false),
    ("OTEL_EXPORTER_OTLP_ENDPOINT", DEFAULT_ENDPOINT, false),
];

/// Settings scopes in precedence order (lowest first); managed wins.
const SCOPES: &[&str] = &["user", "project", "local", "managed"];

/// The absolute hook command to wire: the `hatel-hook` binary sitting beside the
/// running `hatel` executable, so the wiring points at this exact install rather
/// than relying on the hook being on `PATH` when Claude Code spawns it. Carries the platform
/// executable suffix (`.exe` on Windows), since that's the name the release ships and the OS
/// must spawn.
pub fn hook_command() -> String {
    let exe = format!("{HOOK_BIN}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&exe)))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(exe)
}

// ── settings discovery ──

/// A settings file's load state — `Malformed`/`Unreadable` are reported distinctly so a
/// broken file is never silently mistaken for an absent one (which would hide its env and
/// hooks, or let a writer clobber a file it failed to parse).
pub enum Load {
    Absent,
    Unreadable(String),
    Malformed(String),
    Found(Value),
}

impl Load {
    pub fn value(&self) -> Option<&Value> {
        match self {
            Load::Found(v) => Some(v),
            _ => None,
        }
    }
    pub fn label(&self) -> String {
        match self {
            Load::Absent => "absent".to_string(),
            Load::Unreadable(e) => format!("unreadable ({e})"),
            Load::Malformed(e) => format!("malformed ({e})"),
            Load::Found(_) => "found".to_string(),
        }
    }
}

pub struct ScopeFile {
    pub name: &'static str,
    pub path: PathBuf,
    pub load: Load,
}

pub type Env = std::collections::BTreeMap<String, (String, &'static str)>;

pub fn scope_files() -> Vec<ScopeFile> {
    SCOPES
        .iter()
        .filter_map(|&name| scope_path(name).map(|path| ScopeFile { name, load: read_json(&path), path }))
        .collect()
}

/// The file backing a scope. `user` is global (all projects); `project`/`local` are per-repo
/// (committed / per-dev) and anchored at the git worktree root so they resolve to the same file
/// from any subdirectory; `managed` is org-controlled and is never a write target.
pub fn scope_path(name: &str) -> Option<PathBuf> {
    match name {
        "user" => home_dir().map(|h| h.join(".claude/settings.json")),
        "project" => Some(repo_base().join(".claude/settings.json")),
        "local" => Some(repo_base().join(".claude/settings.local.json")),
        "managed" => Some(managed_path()),
        _ => None,
    }
}

/// The repo-relative scopes anchor at the git worktree root (so `--scope project/local` is
/// stable from any subdirectory), falling back to the current directory outside a repo.
fn repo_base() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    hatel_core::project::git_root(&cwd).unwrap_or(cwd)
}

#[cfg(target_os = "macos")]
fn managed_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
}
#[cfg(target_os = "windows")]
fn managed_path() -> PathBuf {
    PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json")
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn managed_path() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.json")
}

pub fn read_json(path: &Path) -> Load {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(v) => Load::Found(v),
            Err(e) => Load::Malformed(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Load::Absent,
        Err(e) => Load::Unreadable(e.to_string()),
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// ── reads (diagnosis) ──

/// Merge each scope's `env` block in precedence order; later scopes override.
pub fn effective_env(files: &[ScopeFile]) -> Env {
    let mut env = Env::new();
    for f in files {
        let Some(obj) = f.load.value().and_then(|v| v.get("env")).and_then(|v| v.as_object()) else {
            continue;
        };
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), (s.to_string(), f.name));
            }
        }
    }
    env
}

pub fn managed_hooks_only(files: &[ScopeFile]) -> bool {
    files
        .iter()
        .find(|f| f.name == "managed")
        .and_then(|f| f.load.value())
        .and_then(|v| v.get("allowManagedHooksOnly"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether a scope wires our hook (walking `hooks.<event>[].hooks[].command` structurally,
/// so the marker string in an env value or a disabled block never false-positives).
fn scope_wires_hook(f: &ScopeFile) -> bool {
    f.load
        .value()
        .and_then(|v| v.get("hooks"))
        .and_then(|h| h.as_object())
        .map(|events| events.values().any(event_has_hook))
        .unwrap_or(false)
}

/// Which of the canonical `EVENTS` our hook is wired for, counting only scopes whose hooks
/// Claude Code actually honors (under `allowManagedHooksOnly`, just the managed scope). `doctor`
/// compares this to `EVENTS` so partial coverage — most lifecycle events silently uncaptured — is
/// reported rather than passing as fully wired on the strength of a single event.
pub fn covered_events(files: &[ScopeFile]) -> Vec<&'static str> {
    let managed_only = managed_hooks_only(files);
    EVENTS
        .iter()
        .copied()
        .filter(|ev| {
            files
                .iter()
                .any(|f| (!managed_only || f.name == "managed") && scope_event_has_hook(f, ev))
        })
        .collect()
}

/// Whether a scope wires our hook for one specific event.
fn scope_event_has_hook(f: &ScopeFile, ev: &str) -> bool {
    f.load
        .value()
        .and_then(|v| v.get("hooks"))
        .and_then(|h| h.get(ev))
        .and_then(|e| e.as_array())
        .is_some_and(|groups| event_array_has_hook(groups))
}

/// Whether a blocked (non-managed) scope wires the hook while managed-only is in force.
pub fn hook_wired_but_blocked(files: &[ScopeFile]) -> bool {
    managed_hooks_only(files) && files.iter().any(|f| f.name != "managed" && scope_wires_hook(f))
}

/// Whether one event's value (an array of matcher groups) already invokes our hook.
fn event_has_hook(event: &Value) -> bool {
    event.as_array().is_some_and(|groups| event_array_has_hook(groups))
}

fn event_array_has_hook(groups: &[Value]) -> bool {
    groups.iter().any(group_invokes_our_hook)
}

/// Whether a matcher group runs our hook — any entry in its `hooks` array is ours.
fn group_invokes_our_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|entries| entries.iter().any(entry_is_our_hook))
}

/// Whether a single entry runs *our* hook. Both detection and removal key on this, so they
/// always agree.
fn entry_is_our_hook(entry: &Value) -> bool {
    entry.get("command").and_then(|c| c.as_str()).is_some_and(command_is_our_hook)
}

/// Whether a hook `command` runs our hook binary: its path basename is `hatel-hook`
/// (with an optional, case-insensitive `.exe`). Matched on the basename, not a substring — so an
/// unrelated command that merely contains the name (a wrapper like `hatel-hook-shim`)
/// is never mistaken for ours. The whole string is the path we wire (which may contain spaces, as
/// on Windows `C:\Program Files\…`), so it is not split on whitespace.
fn command_is_our_hook(command: &str) -> bool {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    match base.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("exe") => stem == HOOK_BIN,
        _ => base == HOOK_BIN,
    }
}

/// Whether a matcher group's `hooks` array is present and empty — i.e. removing our entries left
/// it inert, so the group itself should be pruned.
fn group_hooks_is_empty(group: &Value) -> bool {
    matches!(group.get("hooks"), Some(Value::Array(a)) if a.is_empty())
}

// ── snippet ──

/// The paste-ready settings block — the same `env` + `hooks` that `wire` merges, rendered for
/// managed/org settings where automated writing isn't appropriate. Values go through `{:?}`,
/// which JSON-escapes them, so a Windows hook path with backslashes stays valid JSON.
pub fn render_snippet(hook_cmd: &str) -> String {
    let env = TELEMETRY_ENV
        .iter()
        .map(|(k, v, _)| format!("    {k:?}: {v:?}"))
        .collect::<Vec<_>>()
        .join(",\n");
    let hooks = EVENTS
        .iter()
        .map(|e| {
            format!("    {e:?}: [{{ \"hooks\": [{{ \"type\": \"command\", \"command\": {hook_cmd:?} }}] }}]")
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n  \"env\": {{\n{env}\n  }},\n  \"hooks\": {{\n{hooks}\n  }}\n}}\n")
}

// ── writes (init) ──

/// What `wire` changed, or refused to touch — graded by severity so the caller can act:
/// `env_conflicts` (a key the user set differently, e.g. a repointed endpoint) is advisory and
/// legitimate; `env_blocked` (a *required* key set so telemetry can't flow) means the result
/// isn't functional; `malformed()` means the file is structurally broken where we needed to
/// merge. All three are left untouched — `wire` never overwrites a user's value.
#[derive(Default)]
pub struct WireReport {
    pub env_added: Vec<&'static str>,
    pub env_conflicts: Vec<(&'static str, String)>,
    pub env_blocked: Vec<(&'static str, String)>,
    pub env_not_object: bool,
    pub events_added: Vec<&'static str>,
    pub events_present: Vec<&'static str>,
    pub events_conflicts: Vec<&'static str>,
    pub hooks_not_object: bool,
}

impl WireReport {
    /// Whether the merge changed anything that must be persisted.
    pub fn changed(&self) -> bool {
        !self.env_added.is_empty() || !self.events_added.is_empty()
    }
    /// Whether the file is structurally broken where we needed to merge — an `env` or `hooks`
    /// that isn't an object, or an event whose value isn't an array. Unlike a repointed
    /// endpoint (advisory), this means we can't safely wire, so the caller should refuse rather
    /// than persist a half-wired file.
    pub fn malformed(&self) -> bool {
        self.env_not_object || self.hooks_not_object || !self.events_conflicts.is_empty()
    }
}

/// Idempotently merge the telemetry `env` and the lifecycle hooks into a settings object.
/// Non-destructive by construction: env keys are only *added* when absent (an existing value
/// that differs — e.g. an endpoint repointed at a corporate collector — is reported, never
/// overwritten), and each event's hook is *appended* only when our hook isn't already present,
/// so a user's own hooks survive and a second run is a no-op.
pub fn wire(settings: &mut Value, hook_cmd: &str) -> WireReport {
    let mut rep = WireReport::default();
    let Some(obj) = settings.as_object_mut() else {
        rep.env_not_object = true;
        rep.hooks_not_object = true;
        return rep;
    };

    match obj.entry("env").or_insert_with(|| json!({})) {
        Value::Object(env) => {
            for (k, v, required) in TELEMETRY_ENV {
                match env.get(k) {
                    Some(Value::String(existing)) if existing == v => {}
                    // A required key set to a different value means native telemetry won't flow
                    // (disabled / wrong exporter) — block; an advisory key (endpoint, protocol)
                    // that differs is a legitimate choice — note it.
                    Some(existing) if required => rep.env_blocked.push((k, existing.to_string())),
                    Some(existing) => rep.env_conflicts.push((k, existing.to_string())),
                    None => {
                        env.insert(k.to_string(), json!(v));
                        rep.env_added.push(k);
                    }
                }
            }
        }
        _ => rep.env_not_object = true,
    }

    // The protocol key is advisory in general, but mandatory when the (effective) endpoint is the
    // local receiver — promote a mismatch there from advisory to blocking, so `init` and `doctor`
    // agree that telemetry won't actually reach the receiver.
    let endpoint_is_local = obj
        .get("env")
        .and_then(|e| e.get("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .and_then(Value::as_str)
        .is_some_and(is_local_receiver);
    if endpoint_is_local
        && let Some(i) = rep.env_conflicts.iter().position(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
    {
        rep.env_blocked.push(rep.env_conflicts.remove(i));
    }

    match obj.entry("hooks").or_insert_with(|| json!({})) {
        Value::Object(hooks) => {
            for ev in EVENTS {
                match hooks.entry(ev).or_insert_with(|| Value::Array(vec![])) {
                    Value::Array(groups) => {
                        if event_array_has_hook(groups) {
                            rep.events_present.push(ev);
                        } else {
                            groups.push(hook_group(hook_cmd));
                            rep.events_added.push(ev);
                        }
                    }
                    _ => rep.events_conflicts.push(ev),
                }
            }
        }
        _ => rep.hooks_not_object = true,
    }

    rep
}

fn hook_group(hook_cmd: &str) -> Value {
    json!({ "hooks": [{ "type": "command", "command": hook_cmd }] })
}

/// What `unwire` removed.
#[derive(Default)]
pub struct UnwireReport {
    pub events_cleared: Vec<&'static str>,
}

impl UnwireReport {
    pub fn changed(&self) -> bool {
        !self.events_cleared.is_empty()
    }
}

/// Remove this collector's hook from every event, the inverse of `wire`. It strips only our hook
/// *entries* (a user's own hook — even one co-located in the same group — survives), then prunes
/// any group we emptied, any event left with no groups, and finally an empty `hooks` object, so
/// no cruft is left. The `env` block is left untouched: those are Claude Code's native telemetry
/// settings, not exclusively ours, so — like `wire` refusing to overwrite a repointed endpoint —
/// `unwire` won't guess whether they should go. The caller reports that.
pub fn unwire(settings: &mut Value) -> UnwireReport {
    let mut rep = UnwireReport::default();
    let Some(obj) = settings.as_object_mut() else {
        return rep;
    };
    let Some(Value::Object(hooks)) = obj.get_mut("hooks") else {
        return rep;
    };

    for ev in EVENTS {
        let (removed, empty) = match hooks.get_mut(ev) {
            Some(Value::Array(groups)) => {
                // Remove our hook *entries*, so a user's command co-located in the same group
                // survives; then drop any group whose hooks array we emptied.
                let mut removed = false;
                for group in groups.iter_mut() {
                    if let Some(Value::Array(entries)) = group.get_mut("hooks") {
                        let before = entries.len();
                        entries.retain(|e| !entry_is_our_hook(e));
                        removed |= entries.len() != before;
                    }
                }
                groups.retain(|g| !group_hooks_is_empty(g));
                (removed, groups.is_empty())
            }
            _ => (false, false),
        };
        if removed {
            rep.events_cleared.push(ev);
        }
        if empty {
            hooks.remove(ev);
        }
    }

    if hooks.is_empty() {
        obj.remove("hooks");
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "/usr/local/bin/hatel-hook";

    #[test]
    fn wire_empty_adds_env_and_all_events() {
        let mut s = json!({});
        let rep = wire(&mut s, CMD);
        assert_eq!(rep.env_added.len(), TELEMETRY_ENV.len());
        assert_eq!(rep.events_added.len(), EVENTS.len());
        assert!(rep.changed());
        assert!(
            !rep.malformed() && rep.env_conflicts.is_empty() && rep.env_blocked.is_empty(),
            "a clean wire has no conflicts"
        );
        for ev in EVENTS {
            assert!(event_has_hook(&s["hooks"][ev]), "{ev} wired");
        }
        assert_eq!(s["env"]["OTEL_EXPORTER_OTLP_PROTOCOL"], "http/json");
    }

    #[test]
    fn wire_is_idempotent_byte_for_byte() {
        let mut s = json!({});
        wire(&mut s, CMD);
        let first = s.clone();
        let rep = wire(&mut s, CMD);
        assert!(!rep.changed(), "second run changes nothing");
        assert_eq!(rep.events_present.len(), EVENTS.len());
        assert_eq!(s, first, "second run leaves the value identical");
    }

    #[test]
    fn wire_appends_to_an_existing_user_hook() {
        let mut s = json!({
            "hooks": {
                "PostToolUse": [{ "hooks": [{ "type": "command", "command": "my-own-tool" }] }]
            }
        });
        let rep = wire(&mut s, CMD);
        let groups = s["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "our hook is appended, theirs kept");
        assert_eq!(groups[0]["hooks"][0]["command"], "my-own-tool");
        assert!(rep.events_added.contains(&"PostToolUse"));
    }

    #[test]
    fn wire_does_not_overwrite_a_repointed_endpoint() {
        let mut s = json!({ "env": { "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318" } });
        let rep = wire(&mut s, CMD);
        assert_eq!(s["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://corp:4318");
        assert!(rep.env_conflicts.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(!rep.malformed(), "a repointed endpoint is advisory, not malformed");
        assert!(rep.env_blocked.is_empty(), "a repointed endpoint is advisory, not blocking");
        // the non-conflicting keys are still added
        assert_eq!(s["env"]["OTEL_METRICS_EXPORTER"], "otlp");
    }

    #[test]
    fn wire_flags_disabled_telemetry_as_blocked_not_advisory() {
        let mut s = json!({ "env": {
            "CLAUDE_CODE_ENABLE_TELEMETRY": "0",
            "OTEL_METRICS_EXPORTER": "none",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318"
        }});
        let rep = wire(&mut s, CMD);
        // required keys set to non-collector values block native telemetry...
        assert!(rep.env_blocked.iter().any(|(k, _)| *k == "CLAUDE_CODE_ENABLE_TELEMETRY"));
        assert!(rep.env_blocked.iter().any(|(k, _)| *k == "OTEL_METRICS_EXPORTER"));
        // ...while a repointed endpoint stays advisory, not blocking
        assert!(rep.env_conflicts.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(!rep.malformed());
        // nothing the user set is overwritten
        assert_eq!(s["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"], "0");
    }

    #[test]
    fn is_local_receiver_normalizes_host() {
        assert!(is_local_receiver("http://127.0.0.1:4318"));
        assert!(is_local_receiver("http://127.0.0.1:4318/")); // trailing slash
        assert!(is_local_receiver("http://localhost:4318")); // alias
        assert!(is_local_receiver("http://127.0.0.1:9999")); // any local port
        assert!(is_local_receiver("http://[::1]:4318")); // ipv6 loopback
        assert!(!is_local_receiver("http://corp-collector:4318"));
        assert!(!is_local_receiver("https://otel.example.com"));
    }

    #[test]
    fn wire_blocks_wrong_protocol_against_a_local_endpoint() {
        // grpc with the (default-added) local endpoint can't reach the http/json receiver — block,
        // so `init` agrees with `doctor` instead of exiting 0.
        let mut s = json!({ "env": { "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc" } });
        let rep = wire(&mut s, CMD);
        assert!(rep.env_blocked.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"));
        assert!(!rep.env_conflicts.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"));
    }

    #[test]
    fn wire_keeps_wrong_protocol_advisory_against_a_remote_endpoint() {
        let mut s = json!({ "env": {
            "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318"
        }});
        let rep = wire(&mut s, CMD);
        assert!(rep.env_conflicts.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"));
        assert!(!rep.env_blocked.iter().any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"));
    }

    #[test]
    fn wire_leaves_a_non_array_event_untouched() {
        let mut s = json!({ "hooks": { "PostToolUse": "oops" } });
        let rep = wire(&mut s, CMD);
        assert_eq!(s["hooks"]["PostToolUse"], "oops");
        assert!(rep.events_conflicts.contains(&"PostToolUse"));
        assert!(rep.malformed(), "a non-array event is structural malformation");
        // other events still wire
        assert!(rep.events_added.contains(&"SessionStart"));
    }

    #[test]
    fn unwire_removes_only_our_hooks_and_prunes() {
        let mut s = json!({});
        wire(&mut s, CMD);
        let rep = unwire(&mut s);
        assert!(rep.changed());
        assert_eq!(rep.events_cleared.len(), EVENTS.len());
        assert!(s.get("hooks").is_none(), "an emptied hooks object is pruned");
        assert!(s["env"].is_object(), "env is Claude Code's config — left intact");
    }

    #[test]
    fn unwire_keeps_a_users_own_hook() {
        let mut s = json!({
            "hooks": { "PostToolUse": [{ "hooks": [{ "type": "command", "command": "my-own-tool" }] }] }
        });
        wire(&mut s, CMD);
        let rep = unwire(&mut s);
        let groups = s["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "only our hook is removed");
        assert_eq!(groups[0]["hooks"][0]["command"], "my-own-tool");
        assert!(rep.events_cleared.contains(&"PostToolUse"));
    }

    #[test]
    fn unwire_removes_only_our_entry_from_a_shared_group() {
        // A (contrived) group holding both the user's command and ours in one hooks array.
        let mut s = json!({
            "hooks": { "PostToolUse": [
                { "hooks": [
                    { "type": "command", "command": "my-own-tool" },
                    { "type": "command", "command": CMD }
                ] }
            ] }
        });
        let rep = unwire(&mut s);
        assert!(rep.events_cleared.contains(&"PostToolUse"));
        let entries = s["hooks"]["PostToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "only our entry is removed, the user's survives");
        assert_eq!(entries[0]["command"], "my-own-tool");
    }

    #[test]
    fn unwire_on_an_unwired_file_is_a_noop() {
        let mut s = json!({ "env": { "FOO": "bar" } });
        let rep = unwire(&mut s);
        assert!(!rep.changed());
        assert_eq!(s, json!({ "env": { "FOO": "bar" } }));
    }

    #[test]
    fn wire_then_unwire_round_trips() {
        let mut s = json!({ "permissions": { "allow": ["Bash"] } });
        wire(&mut s, CMD);
        unwire(&mut s);
        // back to just the user's unrelated settings plus the env block wire added
        assert!(s.get("hooks").is_none());
        assert_eq!(s["permissions"]["allow"][0], "Bash");
    }

    #[test]
    fn snippet_is_valid_json_with_a_windows_path() {
        let snippet = render_snippet(r"C:\Program Files\ht\hatel-hook.exe");
        let parsed: Value = serde_json::from_str(&snippet).expect("snippet parses as JSON");
        assert!(event_has_hook(&parsed["hooks"]["SessionStart"]));
    }

    #[test]
    fn command_matching_is_precise_not_substring() {
        assert!(command_is_our_hook("/usr/local/bin/hatel-hook"));
        assert!(command_is_our_hook("hatel-hook"));
        assert!(command_is_our_hook(r"C:\ht\hatel-hook.exe"));
        assert!(command_is_our_hook("hatel-hook.EXE"));
        // a command that merely contains the name is NOT ours
        assert!(!command_is_our_hook("/usr/local/bin/hatel-hook-shim"));
        assert!(!command_is_our_hook("my-hatel-hook"));
    }

    fn one_scope(v: Value) -> Vec<ScopeFile> {
        vec![ScopeFile { name: "user", path: std::path::PathBuf::from("x"), load: Load::Found(v) }]
    }

    #[test]
    fn covered_events_reports_partial_wiring() {
        let files = one_scope(json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }],
                "PostToolUse":  [{ "hooks": [{ "type": "command", "command": CMD }] }]
            }
        }));
        let covered = covered_events(&files);
        assert_eq!(covered.len(), 2);
        assert!(covered.contains(&"SessionStart") && covered.contains(&"PostToolUse"));
        assert!(!covered.contains(&"SessionEnd"));
    }

    #[test]
    fn covered_events_is_complete_after_wire() {
        let mut s = json!({});
        wire(&mut s, CMD);
        assert_eq!(covered_events(&one_scope(s)).len(), EVENTS.len());
    }

    fn scope(name: &'static str, v: Value) -> ScopeFile {
        ScopeFile { name, path: "x".into(), load: Load::Found(v) }
    }

    #[test]
    fn managed_only_does_not_count_blocked_user_hooks() {
        let files = vec![
            scope("user", json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }] } })),
            scope("managed", json!({ "allowManagedHooksOnly": true })),
        ];
        // the user hook is configured but blocked, so it covers nothing and is flagged distinctly
        assert!(covered_events(&files).is_empty());
        assert!(hook_wired_but_blocked(&files));
    }

    #[test]
    fn managed_only_counts_managed_wiring() {
        let mut managed = json!({ "allowManagedHooksOnly": true });
        wire(&mut managed, CMD);
        let files = vec![scope("managed", managed)];
        assert_eq!(covered_events(&files).len(), EVENTS.len());
        assert!(!hook_wired_but_blocked(&files));
    }
}
