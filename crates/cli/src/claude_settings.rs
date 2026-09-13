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

/// The lifecycle events this collector *can* wire — its vocabulary. Claude Code deliberately does
/// not pass the `OTEL_*` telemetry env to hook subprocesses, which is why hooks carry the project
/// context and domain events the native layer can't. Only the events actually needed are wired
/// (see [`active_events`]): `SessionStart` (for the session→project index) plus every event a
/// loaded Kind binds. The rest stay in the vocabulary so a plugin can bind them — e.g. `PostToolUse`
/// for a tool-driven Kind — without firing the hook on events nothing consumes.
pub const EVENTS: [&str; 10] = [
    "SessionStart",
    "SessionEnd",
    "UserPromptExpansion",
    "PostToolUse",
    "PostToolUseFailure",
    "UserPromptSubmit",
    "SubagentStop",
    "InstructionsLoaded",
    "PreCompact",
    "PostCompact",
];

/// The events to actually wire for a given registry: `SessionStart` (needed for the session index
/// even though no Kind binds it) plus every vocabulary event some loaded Kind binds. This is what
/// keeps the hook off events nothing consumes — no dead `SessionEnd`/`PostCompact` wiring.
pub fn active_events(registry: &hatel_core::Registry) -> Vec<&'static str> {
    EVENTS
        .iter()
        .copied()
        .filter(|ev| *ev == "SessionStart" || !registry.bindings_for(ev).is_empty())
        .collect::<Vec<_>>()
}

/// Events some loaded Kind binds that are *not* in the wireable vocabulary — `init` can't wire
/// them (the hook never fires for them), so the binding would silently collect nothing. `init`/
/// `doctor` surface them loudly instead.
pub fn unwireable_bindings(registry: &hatel_core::Registry) -> Vec<String> {
    unwireable(registry)
}

fn unwireable(registry: &hatel_core::Registry) -> Vec<String> {
    let mut events: Vec<String> = registry
        .bound_events()
        .filter(|e| !EVENTS.contains(e))
        .map(str::to_string)
        .collect();
    events.sort_unstable();
    events.dedup();
    events
}

/// The registry every wiring decision is taken against, built once by the command that needs it
/// and passed down. Resilient (a broken plugin is skipped, never blocking wiring of core
/// telemetry), matching the hook's own load.
pub fn registry_for_wiring(cfg: &hatel_core::Config) -> hatel_core::Registry {
    hatel_core::schema::build_registry_resilient(cfg)
}

/// The receiver's default bind address — the endpoint `init` wires. Used to tell "pointed at the
/// local receiver" (where `http/json` is mandatory) from "repointed elsewhere" (where it's the
/// remote collector's business).
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4318";

/// Whether an OTLP endpoint points at a loopback (local) receiver — where this collector is the
/// only thing that could be listening, so `http/json` is mandatory. Normalizes the host so an
/// alias (`localhost`, `::1`), a non-default port, or a trailing slash/path doesn't slip past as
/// "remote". Anything non-loopback is treated as a remote collector (protocol is its business).
pub fn is_local_receiver(endpoint: &str) -> bool {
    // The receiver is plaintext HTTP, so an `https://` endpoint — even on loopback — is not it
    // (Claude Code would TLS-handshake a server that speaks none). Only plain `http://` (or a
    // scheme-less authority) on a loopback host counts.
    if endpoint.starts_with("https://") {
        return false;
    }
    let rest = endpoint.strip_prefix("http://").unwrap_or(endpoint);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = if let Some(after) = authority.strip_prefix('[') {
        after.split(']').next().unwrap_or(after) // [::1]:4318 → ::1
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(authority)
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
        .filter_map(|&name| {
            scope_path(name).map(|path| ScopeFile {
                name,
                load: read_json(&path),
                path,
            })
        })
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
        let Some(obj) = f
            .load
            .value()
            .and_then(|v| v.get("env"))
            .and_then(|v| v.as_object())
        else {
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

/// How one lifecycle event is wired, across the scopes Claude Code honors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wiring {
    /// No entry of ours — the event is not captured.
    Missing,
    /// Ours, but at least one entry runs synchronously: Claude Code waits for the hook every time
    /// the event fires. Records still arrive, so this is a cost, not a gap.
    Blocking,
    /// Every entry of ours runs without the session waiting on it.
    Concurrent,
}

/// How each of `events` is wired, counting only scopes whose hooks Claude Code actually honors
/// (under `allowManagedHooksOnly`, just the managed scope). One traversal answers both questions
/// asked of the wiring — whether the event is captured, and whether capturing it delays the
/// session — so the two can never drift apart. Partial coverage is therefore reported rather than
/// passing as fully wired on the strength of a single event.
pub fn event_wiring(files: &[ScopeFile], events: &[&'static str]) -> Vec<(&'static str, Wiring)> {
    let managed_only = managed_hooks_only(files);
    events
        .iter()
        .copied()
        .map(|ev| {
            let ours = files
                .iter()
                .filter(|f| !managed_only || f.name == "managed")
                .flat_map(|f| scope_event_entries(f, ev))
                .filter(|e| entry_is_our_hook(e));
            let mut wired = false;
            let mut blocking = false;
            for entry in ours {
                wired = true;
                // A second scope wiring the same event asynchronously does not undo the wait the
                // blocking one imposes, so any blocking entry decides the event.
                blocking |= !entry_is_async(entry);
            }
            let w = match (wired, blocking) {
                (false, _) => Wiring::Missing,
                (true, true) => Wiring::Blocking,
                (true, false) => Wiring::Concurrent,
            };
            (ev, w)
        })
        .collect()
}

/// The events of `wiring` that are captured at all, whatever it costs to capture them.
pub fn covered(wiring: &[(&'static str, Wiring)]) -> Vec<&'static str> {
    wiring
        .iter()
        .filter(|(_, w)| *w != Wiring::Missing)
        .map(|(ev, _)| *ev)
        .collect()
}

/// Every hook entry configured for one event in one scope, groups flattened — the granularity at
/// which both "is it ours" and "does it block" are decided.
fn scope_event_entries<'a>(f: &'a ScopeFile, ev: &str) -> impl Iterator<Item = &'a Value> {
    f.load
        .value()
        .and_then(|v| v.get("hooks"))
        .and_then(|h| h.get(ev))
        .and_then(|e| e.as_array())
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
        .flatten()
}

/// Whether a blocked (non-managed) scope wires the hook while managed-only is in force.
pub fn hook_wired_but_blocked(files: &[ScopeFile]) -> bool {
    managed_hooks_only(files)
        && files
            .iter()
            .any(|f| f.name != "managed" && scope_wires_hook(f))
}

/// The distinct commands our hook is wired with across honored scopes — so `doctor` can check
/// they still resolve on disk. A binary moved or reinstalled elsewhere leaves a stale absolute
/// path that the basename-based coverage check still counts as "wired" but that no longer runs.
pub fn wired_hook_commands(files: &[ScopeFile]) -> Vec<String> {
    let managed_only = managed_hooks_only(files);
    let mut cmds = Vec::new();
    for f in files {
        if managed_only && f.name != "managed" {
            continue;
        }
        let Some(events) = f
            .load
            .value()
            .and_then(|v| v.get("hooks"))
            .and_then(|h| h.as_object())
        else {
            continue;
        };
        for groups in events.values() {
            let Some(groups) = groups.as_array() else {
                continue;
            };
            for g in groups {
                let Some(entries) = g.get("hooks").and_then(|h| h.as_array()) else {
                    continue;
                };
                for c in entries
                    .iter()
                    .filter_map(|e| e.get("command").and_then(|c| c.as_str()))
                {
                    if command_is_our_hook(c) && !cmds.iter().any(|x| x == c) {
                        cmds.push(c.to_string());
                    }
                }
            }
        }
    }
    cmds
}

/// What a wired hook answers when asked which build it is. The hook and this binary ship in one
/// archive and compile one schema, so a build other than this one writes records in a shape the
/// queries here do not describe.
#[derive(Debug)]
pub enum HookBuild {
    Version(String),
    /// It ran cleanly and named no version — every build before `--version` existed answers this
    /// way.
    Unreported,
    /// No usable answer: it could not be started here, exited with a failure, or did not answer in
    /// time. Claude Code runs a hook through a shell, which starts what this cannot, and a hook
    /// event is nothing like `--version`, so this says the build is unknown and never that nothing
    /// is collected.
    Unverified(String),
}

/// How much of a hook's answer is read. A build name is one short line, and a deadline bounds time
/// rather than memory, so anything longer is not the answer this asks for.
const HOOK_ANSWER_BYTES: u64 = 256;

/// How long `doctor` waits for a hook to name its build. A diagnostic that waits on a stalled
/// binary never reports anything, and a hook answering `--version` does no work at all.
const HOOK_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Ask the hook wired at `command` which build it is. Stdin is closed so a build that predates
/// `--version` reads an empty event and records nothing instead of waiting for one.
pub fn wired_hook_build(command: &str) -> HookBuild {
    probe_hook_build(command, HOOK_PROBE_DEADLINE)
}

fn probe_hook_build(command: &str, deadline: std::time::Duration) -> HookBuild {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = match Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return HookBuild::Unverified(e.to_string()),
    };
    let started = std::time::Instant::now();
    // Stdout is drained while the hook runs, so a large write cannot stall it, and received under
    // the same deadline, so a process it leaves holding the pipe delays no answer. The thread
    // itself ends when that process closes the pipe, which in a long-lived server holds one
    // thread until it does.
    let out = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.take(HOOK_ANSWER_BYTES + 1).read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let no_answer =
        || HookBuild::Unverified(format!("no answer within {}s", deadline.as_secs_f32()));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            // The kill reaches the hook, not whatever it may have spawned; nothing hatel ships
            // spawns anything, and reaching further would need a process group per platform.
            Ok(None) => {
                // Reaping waits on a process that is ending; a kill that did not land leaves one
                // that is not, and waiting on it would outlast the deadline it just broke.
                if child.kill().is_ok() {
                    let _ = child.wait();
                }
                return no_answer();
            }
            Err(e) => return HookBuild::Unverified(e.to_string()),
        }
    };
    let Ok(stdout) = rx.recv_timeout(deadline.saturating_sub(started.elapsed())) else {
        return no_answer();
    };
    // The size of the answer is judged before the exit it ended in: a hook that writes past what is
    // read ends on the closed pipe, and that end is this probe's doing rather than its own.
    if stdout.len() as u64 > HOOK_ANSWER_BYTES {
        return HookBuild::Unverified(format!("answered with more than {HOOK_ANSWER_BYTES} bytes"));
    }
    if !status.success() {
        return HookBuild::Unverified(status.to_string());
    }
    stdout
        .trim()
        .strip_prefix(HOOK_BIN)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or(HookBuild::Unreported, |v| HookBuild::Version(v.to_string()))
}

/// Whether one event's value (an array of matcher groups) already invokes our hook.
fn event_has_hook(event: &Value) -> bool {
    event
        .as_array()
        .is_some_and(|groups| event_array_has_hook(groups))
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
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(command_is_our_hook)
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

/// Whether an entry runs without the session waiting on it. Claude Code reads a missing `async`
/// as false, so an entry written before asynchronous wiring reads as blocking — which is what it
/// is. This is the one part of the wiring's shape that holds regardless of which install owns it,
/// so it is what `doctor` judges; `wire` adds the path on top, because it writes for one install.
fn entry_is_async(entry: &Value) -> bool {
    entry.get("async").and_then(Value::as_bool) == Some(true)
}

/// Whether one entry is the wiring this version writes — the command and the `async` flag both.
/// An entry of ours that differs in either is stale (the binary moved, or it predates asynchronous
/// wiring) and is replaced rather than left in place.
fn entry_is_current(entry: &Value, command: &str) -> bool {
    entry.get("command").and_then(|c| c.as_str()) == Some(command) && entry_is_async(entry)
}

/// Whether a group holds the current wiring.
fn group_is_current(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|entries| entries.iter().any(|e| entry_is_current(e, command)))
}

/// Whether a matcher group's `hooks` array is present and empty — i.e. removing our entries left
/// it inert, so the group itself should be pruned.
fn group_hooks_is_empty(group: &Value) -> bool {
    matches!(group.get("hooks"), Some(Value::Array(a)) if a.is_empty())
}

/// Remove our hook *entries* from every group of one event — at entry granularity, so a user's
/// command co-located in the same group always survives. `keep_current` keeps our entry at the
/// given path (repointing only a *stale* one), which is what `wire` wants for an event it still
/// owns; `None` removes every entry of ours (`unwire`, and an event `wire` no longer owns). Returns
/// whether anything was removed; the caller prunes emptied groups if it chooses to mutate.
fn strip_our_entries(groups: &mut [Value], keep_current: Option<&str>) -> bool {
    let mut removed = false;
    for group in groups.iter_mut() {
        if let Some(Value::Array(entries)) = group.get_mut("hooks") {
            let before = entries.len();
            entries.retain(|e| {
                !entry_is_our_hook(e) || keep_current.is_some_and(|cmd| entry_is_current(e, cmd))
            });
            removed |= entries.len() != before;
        }
    }
    removed
}

// ── snippet ──

/// The paste-ready settings block — the same `env` + `hooks` that `wire` merges, rendered for
/// managed/org settings where automated writing isn't appropriate. Values go through `{:?}`,
/// which JSON-escapes them, so a Windows hook path with backslashes stays valid JSON.
pub fn render_snippet(hook_cmd: &str, events: &[&'static str]) -> String {
    let env = TELEMETRY_ENV
        .iter()
        .map(|(k, v, _)| format!("    {k:?}: {v:?}"))
        .collect::<Vec<_>>()
        .join(",\n");
    // The group is serialized from the same builder `wire` uses, so a snippet pasted into managed
    // settings is the wiring `init` would have written rather than a second spelling of it.
    let group = serde_json::to_string(&hook_group(hook_cmd)).unwrap_or_default();
    let hooks = events
        .iter()
        .map(|e| format!("    {e:?}: [{group}]"))
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
    /// Events whose stale wiring was pruned because no loaded Kind binds them anymore (e.g. after
    /// `tool` moved to native OTel) — so a re-run converges to exactly the active set.
    pub events_cleared: Vec<&'static str>,
    pub events_conflicts: Vec<&'static str>,
    pub hooks_not_object: bool,
}

impl WireReport {
    /// Whether the merge changed anything that must be persisted.
    pub fn changed(&self) -> bool {
        !self.env_added.is_empty()
            || !self.events_added.is_empty()
            || !self.events_cleared.is_empty()
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
pub fn wire(settings: &mut Value, hook_cmd: &str, events: &[&'static str]) -> WireReport {
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

    // The protocol key is advisory in general, but mandatory when an effective endpoint is the
    // local receiver — promote a mismatch there from advisory to blocking, so `init` and `doctor`
    // agree that telemetry won't actually reach the receiver. "Effective" is resolved exactly as
    // `doctor`'s `effective_otlp_endpoints` does: a per-signal `…_METRICS_ENDPOINT` /
    // `…_LOGS_ENDPOINT` *overrides* (shadows) the general `OTEL_EXPORTER_OTLP_ENDPOINT` for that
    // signal — so a per-signal route to the receiver is caught, and a per-signal override away from
    // a local general endpoint is honored (not falsely seen as local).
    let env_str = |k: &str| {
        obj.get("env")
            .and_then(|e| e.get(k))
            .and_then(Value::as_str)
    };
    let general = env_str("OTEL_EXPORTER_OTLP_ENDPOINT");
    let effective_metrics = env_str("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").or(general);
    let effective_logs = env_str("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").or(general);
    let endpoint_is_local = [effective_metrics, effective_logs]
        .into_iter()
        .flatten()
        .any(is_local_receiver);
    if endpoint_is_local
        && let Some(i) = rep
            .env_conflicts
            .iter()
            .position(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
    {
        rep.env_blocked.push(rep.env_conflicts.remove(i));
    }

    match obj.entry("hooks").or_insert_with(|| json!({})) {
        Value::Object(hooks) => {
            // Walk the whole vocabulary so a re-run converges to *exactly* the active set: ensure
            // our hook on active events, and strip it from inactive ones — otherwise an upgrade that
            // drops an event (a Kind losing its binding) would leave the hook firing on it for no
            // record, which `event_wiring` (scoped to the active set) couldn't even see.
            let mut pruned_empty: Vec<&'static str> = Vec::new();
            for ev in EVENTS {
                let active = events.contains(&ev);
                // Don't materialise an entry for an inactive event that has none — only prune ones
                // that already exist.
                if !active && !hooks.contains_key(ev) {
                    continue;
                }
                match hooks.entry(ev).or_insert_with(|| Value::Array(vec![])) {
                    Value::Array(groups) => {
                        // For an active event keep our current-path entry (repointing only a stale
                        // one); for an inactive event remove every entry of ours.
                        let removed = strip_our_entries(groups, active.then_some(hook_cmd));

                        if !active {
                            // Inactive event: touch it only when we actually stripped our hook. If
                            // there was nothing of ours, the event is left exactly as the user had
                            // it — no group is dropped, no key is deleted, nothing is reported.
                            if removed {
                                groups.retain(|g| !group_hooks_is_empty(g));
                                rep.events_cleared.push(ev);
                                if groups.is_empty() {
                                    pruned_empty.push(ev);
                                }
                            }
                        } else {
                            // Active event: drop any group our strip emptied, then ensure our hook.
                            groups.retain(|g| !group_hooks_is_empty(g));
                            if groups.iter().any(|g| group_is_current(g, hook_cmd)) {
                                // Already the current wiring; a repoint (stale removed) is a change.
                                if removed {
                                    rep.events_added.push(ev);
                                } else {
                                    rep.events_present.push(ev);
                                }
                            } else {
                                groups.push(hook_group(hook_cmd));
                                rep.events_added.push(ev);
                            }
                        }
                    }
                    _ if active => rep.events_conflicts.push(ev),
                    _ => {}
                }
            }
            // Remove only the keys our pruning emptied, so no `"PostToolUse": []` cruft is left
            // (mirrors `unwire`) — a key we didn't empty, or a user's own, is untouched.
            for ev in pruned_empty {
                hooks.remove(ev);
            }
        }
        _ => rep.hooks_not_object = true,
    }

    rep
}

/// Telemetry never delays the session it observes: the hook is wired asynchronously, so a tool call
/// returns without waiting for the record to be written. hatel exits 0 and prints nothing on the
/// happy path either way, so nothing that reaches the operator is given up for it — a collection
/// gap is surfaced by `doctor`, which is where it is looked for.
fn hook_group(hook_cmd: &str) -> Value {
    json!({ "hooks": [{ "type": "command", "command": hook_cmd, "async": true }] })
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
                // Remove every entry of ours (a user's co-located command survives), then drop any
                // group we emptied.
                let removed = strip_our_entries(groups, None);
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
        let rep = wire(&mut s, CMD, &EVENTS);
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
        wire(&mut s, CMD, &EVENTS);
        let first = s.clone();
        let rep = wire(&mut s, CMD, &EVENTS);
        assert!(!rep.changed(), "second run changes nothing");
        assert_eq!(rep.events_present.len(), EVENTS.len());
        assert_eq!(s, first, "second run leaves the value identical");
    }

    #[test]
    fn wire_appends_to_an_existing_user_hook() {
        let mut s = json!({
            "hooks": {
                "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "my-own-tool" }] }]
            }
        });
        let rep = wire(&mut s, CMD, &EVENTS);
        let groups = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "our hook is appended, theirs kept");
        assert_eq!(groups[0]["hooks"][0]["command"], "my-own-tool");
        assert!(rep.events_added.contains(&"UserPromptSubmit"));
    }

    #[test]
    fn wire_repoints_a_stale_hook_path() {
        // a hook of ours wired at an old path (binary moved) is repointed, not duplicated
        let mut s = json!({ "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "/old/bin/hatel-hook" }] }]
        }});
        let rep = wire(&mut s, CMD, &EVENTS);
        let groups = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(
            groups.len(),
            1,
            "stale group is repointed in place, not added alongside"
        );
        assert_eq!(groups[0]["hooks"][0]["command"], CMD);
        assert!(rep.events_added.contains(&"UserPromptSubmit"));
    }

    #[test]
    fn the_printed_snippet_is_the_wiring_init_writes() {
        // Two spellings of the hook group drift apart silently — a snippet pasted into managed
        // settings must be the same wiring `wire` produces, asynchronous flag included.
        let snippet = render_snippet(CMD, &["SessionStart"]);
        let group = serde_json::to_string(&hook_group(CMD)).unwrap();
        assert!(
            snippet.contains(&group),
            "snippet must embed the built group, got: {snippet}"
        );
    }

    #[test]
    fn wire_upgrades_a_synchronous_entry_of_ours() {
        // An entry written before the wiring became asynchronous runs the right command but blocks
        // the event it observes. It is ours, so it is replaced in place rather than left or doubled.
        let mut s = json!({ "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": CMD }] }]
        }});
        let rep = wire(&mut s, CMD, &EVENTS);
        let groups = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "replaced in place, not added alongside");
        assert_eq!(groups[0]["hooks"][0]["command"], CMD);
        assert_eq!(groups[0]["hooks"][0]["async"], true);
        assert!(rep.events_added.contains(&"UserPromptSubmit"));
    }

    #[test]
    fn wire_repoints_stale_but_keeps_the_users_hook() {
        let mut s = json!({ "hooks": { "UserPromptSubmit": [
            { "hooks": [{ "type": "command", "command": "my-own-tool" }] },
            { "hooks": [{ "type": "command", "command": "/old/hatel-hook" }] }
        ] }});
        wire(&mut s, CMD, &EVENTS);
        let cmds: Vec<String> = s["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["hooks"][0]["command"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().any(|c| c == "my-own-tool"), "user hook kept");
        assert!(
            cmds.iter().any(|c| c == CMD),
            "our hook repointed to current"
        );
        assert!(
            !cmds.iter().any(|c| c == "/old/hatel-hook"),
            "stale path gone"
        );
    }

    // Helper: every command string across all groups of an event.
    fn event_commands(s: &Value, ev: &str) -> Vec<String> {
        s["hooks"][ev]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|e| e["command"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn wire_strips_a_stale_entry_colocated_with_a_user_entry() {
        // contrived: the user's command and our STALE hook share one group's hooks array — entry-
        // level stripping must drop only the stale one, never the user's.
        let mut s = json!({ "hooks": { "UserPromptSubmit": [{ "hooks": [
            { "type": "command", "command": "my-own-tool" },
            { "type": "command", "command": "/old/hatel-hook" }
        ] }] }});
        wire(&mut s, CMD, &EVENTS);
        let cmds = event_commands(&s, "UserPromptSubmit");
        assert!(
            cmds.iter().any(|c| c == "my-own-tool"),
            "user entry survives"
        );
        assert!(cmds.iter().any(|c| c == CMD), "current added");
        assert!(
            !cmds.iter().any(|c| c == "/old/hatel-hook"),
            "stale entry removed"
        );
    }

    #[test]
    fn wire_strips_a_stale_entry_colocated_with_the_current_one() {
        let mut s = json!({ "hooks": { "UserPromptSubmit": [{ "hooks": [
            { "type": "command", "command": CMD },
            { "type": "command", "command": "/old/hatel-hook" }
        ] }] }});
        wire(&mut s, CMD, &EVENTS);
        let cmds = event_commands(&s, "UserPromptSubmit");
        assert_eq!(
            cmds,
            vec![CMD.to_string()],
            "stale entry stripped, current kept once"
        );
    }

    #[test]
    fn wired_hook_commands_collects_distinct_ours_only() {
        let v = json!({ "hooks": {
            "SessionStart": [{ "hooks": [{ "type": "command", "command": "/x/hatel-hook" }] }],
            "UserPromptSubmit": [{ "hooks": [
                { "type": "command", "command": "/x/hatel-hook" },
                { "type": "command", "command": "my-tool" }
            ] }]
        }});
        assert_eq!(
            wired_hook_commands(&one_scope(v)),
            vec!["/x/hatel-hook".to_string()]
        );
    }

    #[test]
    fn wire_does_not_overwrite_a_repointed_endpoint() {
        let mut s = json!({ "env": { "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318" } });
        let rep = wire(&mut s, CMD, &EVENTS);
        assert_eq!(s["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://corp:4318");
        assert!(
            rep.env_conflicts
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_ENDPOINT")
        );
        assert!(
            !rep.malformed(),
            "a repointed endpoint is advisory, not malformed"
        );
        assert!(
            rep.env_blocked.is_empty(),
            "a repointed endpoint is advisory, not blocking"
        );
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
        let rep = wire(&mut s, CMD, &EVENTS);
        // required keys set to non-collector values block native telemetry...
        assert!(
            rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "CLAUDE_CODE_ENABLE_TELEMETRY")
        );
        assert!(
            rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "OTEL_METRICS_EXPORTER")
        );
        // ...while a repointed endpoint stays advisory, not blocking
        assert!(
            rep.env_conflicts
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_ENDPOINT")
        );
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
        // https on loopback is NOT our receiver — serve speaks plaintext HTTP only.
        assert!(!is_local_receiver("https://127.0.0.1:4318"));
        assert!(!is_local_receiver("https://localhost:4318"));
    }

    #[test]
    fn wire_blocks_wrong_protocol_against_a_local_endpoint() {
        // grpc with the (default-added) local endpoint can't reach the http/json receiver — block,
        // so `init` agrees with `doctor` instead of exiting 0.
        let mut s = json!({ "env": { "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc" } });
        let rep = wire(&mut s, CMD, &EVENTS);
        assert!(
            rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
        );
        assert!(
            !rep.env_conflicts
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
        );
    }

    #[test]
    fn wire_keeps_wrong_protocol_advisory_against_a_remote_endpoint() {
        let mut s = json!({ "env": {
            "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318"
        }});
        let rep = wire(&mut s, CMD, &EVENTS);
        assert!(
            rep.env_conflicts
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
        );
        assert!(
            !rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
        );
    }

    #[test]
    fn wire_blocks_wrong_protocol_against_a_per_signal_local_endpoint() {
        // The general endpoint is remote, but a per-signal metrics endpoint routes to the local
        // receiver. `doctor` resolves endpoints per signal, so `wire` must too — the grpc protocol
        // can't reach the http/json receiver, so it is blocking, not advisory.
        let mut s = json!({ "env": {
            "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://corp:4318",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT": "http://127.0.0.1:4318"
        }});
        let rep = wire(&mut s, CMD, &EVENTS);
        assert!(
            rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"),
            "a per-signal local endpoint must promote the protocol mismatch to blocking"
        );
    }

    #[test]
    fn wire_keeps_protocol_advisory_when_per_signal_overrides_shadow_a_local_general() {
        // A per-signal endpoint *overrides* the general one for its signal (doctor's resolution): a
        // local general endpoint fully shadowed by remote metrics+logs overrides reaches the
        // receiver for neither signal, so the protocol stays advisory — `wire` must agree with
        // `doctor`, not over-block on the raw (shadowed) general key.
        let mut s = json!({ "env": {
            "OTEL_EXPORTER_OTLP_PROTOCOL": "grpc",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:4318",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT": "http://corp:4318",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT": "http://corp:4318"
        }});
        let rep = wire(&mut s, CMD, &EVENTS);
        assert!(
            !rep.env_blocked
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL"),
            "both signals overridden to remote → protocol is advisory, not blocking"
        );
        assert!(
            rep.env_conflicts
                .iter()
                .any(|(k, _)| *k == "OTEL_EXPORTER_OTLP_PROTOCOL")
        );
    }

    #[test]
    fn wire_leaves_a_non_array_event_untouched() {
        let mut s = json!({ "hooks": { "UserPromptSubmit": "oops" } });
        let rep = wire(&mut s, CMD, &EVENTS);
        assert_eq!(s["hooks"]["UserPromptSubmit"], "oops");
        assert!(rep.events_conflicts.contains(&"UserPromptSubmit"));
        assert!(
            rep.malformed(),
            "a non-array event is structural malformation"
        );
        // other events still wire
        assert!(rep.events_added.contains(&"SessionStart"));
    }

    #[test]
    fn unwire_removes_only_our_hooks_and_prunes() {
        let mut s = json!({});
        wire(&mut s, CMD, &EVENTS);
        let rep = unwire(&mut s);
        assert!(rep.changed());
        assert_eq!(rep.events_cleared.len(), EVENTS.len());
        assert!(
            s.get("hooks").is_none(),
            "an emptied hooks object is pruned"
        );
        assert!(
            s["env"].is_object(),
            "env is Claude Code's config — left intact"
        );
    }

    #[test]
    fn unwire_keeps_a_users_own_hook() {
        let mut s = json!({
            "hooks": { "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "my-own-tool" }] }] }
        });
        wire(&mut s, CMD, &EVENTS);
        let rep = unwire(&mut s);
        let groups = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "only our hook is removed");
        assert_eq!(groups[0]["hooks"][0]["command"], "my-own-tool");
        assert!(rep.events_cleared.contains(&"UserPromptSubmit"));
    }

    #[test]
    fn unwire_removes_only_our_entry_from_a_shared_group() {
        // A (contrived) group holding both the user's command and ours in one hooks array.
        let mut s = json!({
            "hooks": { "UserPromptSubmit": [
                { "hooks": [
                    { "type": "command", "command": "my-own-tool" },
                    { "type": "command", "command": CMD }
                ] }
            ] }
        });
        let rep = unwire(&mut s);
        assert!(rep.events_cleared.contains(&"UserPromptSubmit"));
        let entries = s["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "only our entry is removed, the user's survives"
        );
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
        wire(&mut s, CMD, &EVENTS);
        unwire(&mut s);
        // back to just the user's unrelated settings plus the env block wire added
        assert!(s.get("hooks").is_none());
        assert_eq!(s["permissions"]["allow"][0], "Bash");
    }

    #[test]
    fn snippet_is_valid_json_with_a_windows_path() {
        let snippet = render_snippet(r"C:\Program Files\ht\hatel-hook.exe", &EVENTS);
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

    #[cfg(unix)]
    fn probe_script(body: &str, deadline: std::time::Duration) -> HookBuild {
        use std::os::unix::fs::PermissionsExt;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ht-probe-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(HOOK_BIN);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let build = probe_hook_build(path.to_str().unwrap(), deadline);
        std::fs::remove_dir_all(&dir).ok();
        build
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_build_is_read_only_from_a_clean_and_timely_exit() {
        let answer = |body: &str| probe_script(body, HOOK_PROBE_DEADLINE);
        let got = answer("echo 'hatel-hook 9.9.9'");
        assert!(
            matches!(&got, HookBuild::Version(v) if v == "9.9.9"),
            "{got:?}"
        );
        let got = answer("cat >/dev/null");
        assert!(matches!(got, HookBuild::Unreported), "{got:?}");
        // A flood is not an answer, and it is never held whole to find that out.
        let got = answer("head -c 200000 /dev/zero | tr '\\0' x");
        assert!(
            matches!(&got, HookBuild::Unverified(r) if r.contains("more than")),
            "{got:?}"
        );
        for body in ["echo 'hatel-hook 9.9.9'; exit 3", "kill -9 $$"] {
            let got = answer(body);
            assert!(matches!(got, HookBuild::Unverified(_)), "{body}: {got:?}");
        }
        let silent = std::time::Duration::from_millis(300);
        for body in ["exec sleep 30", "sleep 3 & echo 'hatel-hook 9.9.9'"] {
            let got = probe_script(body, silent);
            assert!(
                matches!(&got, HookBuild::Unverified(r) if r.contains("no answer")),
                "{body}: {got:?}"
            );
        }
    }

    fn one_scope(v: Value) -> Vec<ScopeFile> {
        vec![ScopeFile {
            name: "user",
            path: std::path::PathBuf::from("x"),
            load: Load::Found(v),
        }]
    }

    #[test]
    fn partial_wiring_leaves_the_rest_missing() {
        let files = one_scope(json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }],
                "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": CMD }] }]
            }
        }));
        let covered = covered(&event_wiring(&files, &EVENTS));
        assert_eq!(covered.len(), 2);
        assert!(covered.contains(&"SessionStart") && covered.contains(&"UserPromptSubmit"));
        assert!(!covered.contains(&"SessionEnd"));
    }

    #[test]
    fn wire_leaves_every_event_covered_and_none_blocking() {
        let mut s = json!({});
        wire(&mut s, CMD, &EVENTS);
        let wiring = event_wiring(&one_scope(s), &EVENTS);
        assert_eq!(covered(&wiring).len(), EVENTS.len());
        assert!(wiring.iter().all(|(_, w)| *w == Wiring::Concurrent));
    }

    /// Wiring written before `async` existed still collects everything, so coverage alone reports
    /// it as healthy — the state every upgraded install is in until `init` runs again.
    #[test]
    fn wiring_that_predates_async_is_covered_but_blocking() {
        let files = one_scope(json!({
            "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }] }
        }));
        let wiring = event_wiring(&files, &EVENTS);
        assert_eq!(covered(&wiring), vec!["SessionStart"]);
        assert_eq!(
            wiring
                .iter()
                .find(|(ev, _)| *ev == "SessionStart")
                .unwrap()
                .1,
            Wiring::Blocking
        );
    }

    /// Two scopes can wire the same event, and both hooks run — so an asynchronous entry beside a
    /// blocking one does not spare the session the wait the blocking one imposes.
    #[test]
    fn a_blocking_entry_decides_an_event_another_scope_wires_asynchronously() {
        let files = vec![
            scope(
                "user",
                json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }] } }),
            ),
            scope(
                "project",
                json!({ "hooks": { "SessionStart": [hook_group(CMD)] } }),
            ),
        ];
        assert_eq!(
            event_wiring(&files, &["SessionStart"]),
            vec![("SessionStart", Wiring::Blocking)]
        );
    }

    /// Only our own entries are judged: a user's synchronous hook sharing the event is their
    /// business, and reading it as ours would send them to `hatel init` for something it cannot fix.
    #[test]
    fn a_foreign_synchronous_hook_does_not_make_our_wiring_blocking() {
        let files = one_scope(json!({
            "hooks": { "SessionStart": [
                { "hooks": [{ "type": "command", "command": "/usr/local/bin/their-hook" }] },
                hook_group(CMD)
            ] }
        }));
        assert_eq!(
            event_wiring(&files, &["SessionStart"]),
            vec![("SessionStart", Wiring::Concurrent)]
        );
    }

    fn scope(name: &'static str, v: Value) -> ScopeFile {
        ScopeFile {
            name,
            path: "x".into(),
            load: Load::Found(v),
        }
    }

    #[test]
    fn managed_only_does_not_count_blocked_user_hooks() {
        let files = vec![
            scope(
                "user",
                json!({ "hooks": { "SessionStart": [{ "hooks": [{ "type": "command", "command": CMD }] }] } }),
            ),
            scope("managed", json!({ "allowManagedHooksOnly": true })),
        ];
        // the user hook is configured but blocked, so it covers nothing and is flagged distinctly
        assert!(covered(&event_wiring(&files, &EVENTS)).is_empty());
        assert!(hook_wired_but_blocked(&files));
    }

    #[test]
    fn managed_only_counts_managed_wiring() {
        let mut managed = json!({ "allowManagedHooksOnly": true });
        wire(&mut managed, CMD, &EVENTS);
        let files = vec![scope("managed", managed)];
        assert_eq!(covered(&event_wiring(&files, &EVENTS)).len(), EVENTS.len());
        assert!(!hook_wired_but_blocked(&files));
    }

    #[test]
    fn rewiring_a_shrunken_active_set_prunes_stale_events() {
        // An upgrade that drops an event from the active set (e.g. `tool` → native OTel) must, on
        // the next wire, strip the now-stale wiring rather than leave the hook firing for nothing.
        let mut s = json!({});
        wire(&mut s, CMD, &EVENTS); // previously: all 8 events wired
        assert!(s["hooks"].get("PostToolUse").is_some());

        // Now wire a shrunken active set (no PostToolUse).
        let active: Vec<&str> = EVENTS
            .iter()
            .copied()
            .filter(|e| *e != "PostToolUse")
            .collect();
        let rep = wire(&mut s, CMD, &active);
        assert!(
            rep.events_cleared.contains(&"PostToolUse"),
            "stale event reported as cleared"
        );
        assert!(
            s["hooks"].get("PostToolUse").is_none(),
            "stale event key pruned, no empty cruft"
        );
        // The active events remain wired, and a user hook elsewhere would have survived.
        assert!(group_is_current(&s["hooks"]["SessionStart"][0], CMD));
    }

    #[test]
    fn pruning_leaves_an_inactive_event_we_dont_own_untouched() {
        // An inactive event carrying only the user's structure (here an empty group, and a real
        // user hook) must be left byte-for-byte as-is — wire never drops a group or deletes a key
        // it didn't put a hook in.
        let mut s = json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [] },
                    { "hooks": [{ "type": "command", "command": "user-tool" }] }
                ]
            }
        });
        let before = s.clone();
        let active: Vec<&str> = EVENTS
            .iter()
            .copied()
            .filter(|e| *e != "PostToolUse")
            .collect();
        let rep = wire(&mut s, CMD, &active);
        assert!(
            !rep.events_cleared.contains(&"PostToolUse"),
            "nothing of ours to clear"
        );
        assert_eq!(
            s["hooks"]["PostToolUse"], before["hooks"]["PostToolUse"],
            "an event we don't own is untouched (no group dropped, no key deleted)"
        );
    }

    #[test]
    fn pruning_keeps_a_users_own_hook_on_a_deactivated_event() {
        // If a deactivated event also carries the user's own hook, only ours is stripped.
        let mut s = json!({
            "hooks": { "PostToolUse": [{ "hooks": [
                { "type": "command", "command": CMD },
                { "type": "command", "command": "user-tool" }
            ] }] }
        });
        let active: Vec<&str> = EVENTS
            .iter()
            .copied()
            .filter(|e| *e != "PostToolUse")
            .collect();
        wire(&mut s, CMD, &active);
        let cmds = event_commands(&s, "PostToolUse");
        assert!(!cmds.iter().any(|c| c == CMD), "our hook stripped");
        assert!(cmds.iter().any(|c| c == "user-tool"), "user's hook kept");
    }

    #[test]
    fn active_events_are_session_start_plus_bound_events_only() {
        let reg = hatel_core::schema::load_core().unwrap();
        let active = active_events(&reg);
        // SessionStart (for the index) plus exactly the events a core Kind binds.
        for ev in [
            "SessionStart",
            "UserPromptSubmit",
            "SubagentStop",
            "InstructionsLoaded",
            "PreCompact",
            "PostToolUse",
            "PostToolUseFailure",
            "UserPromptExpansion",
        ] {
            assert!(active.contains(&ev), "{ev} should be wired");
        }
        // Vocabulary events nothing binds are NOT wired, so the hook never fires for no record.
        for ev in ["SessionEnd", "PostCompact"] {
            assert!(
                !active.contains(&ev),
                "{ev} should not be wired (nothing binds it)"
            );
        }
    }

    #[test]
    fn unwireable_flags_only_out_of_vocabulary_bound_events() {
        use hatel_core::registry::{HookBinding, KindSpec, KindSpecRaw};
        let mut reg = hatel_core::Registry::new();
        reg.add_kind(
            KindSpec::from_raw(KindSpecRaw {
                name: "x".into(),
                fields: vec!["session_id".into()],
                group_key: "session_id".into(),
                redact: vec![],
                measures: vec![],
                identity: None,
            })
            .unwrap(),
        )
        .unwrap();
        // One in-vocabulary event and one outside it, both bound by the same plugin Kind.
        for ev in ["PostToolUse", "PreToolUse"] {
            reg.bind(HookBinding {
                event: ev.into(),
                kind: "x".into(),
                map: Default::default(),
            })
            .unwrap();
        }
        // `PostToolUse` is wireable (in EVENTS); `PreToolUse` is not, so only it is flagged.
        assert_eq!(unwireable(&reg), vec!["PreToolUse".to_string()]);
    }
}
