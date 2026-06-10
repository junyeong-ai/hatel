# hatel

[![CI](https://github.com/junyeong-ai/hatel/actions/workflows/ci.yml/badge.svg)](https://github.com/junyeong-ai/hatel/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#)

> **English** | **[한국어](README.ko.md)**

A local, zero-infrastructure telemetry collector for Claude Code. It joins two
complementary signal layers that Claude Code already produces — native
OpenTelemetry and lifecycle hooks — into a per-project, per-session, and
per-subagent view, with no dashboards to host and, by default, no data leaving your
machine (opt-in [export](#forwarding-to-other-collectors-export) can tee the enriched
stream to a downstream collector).

- **Native OTel** (push) carries the machine signals: tokens, cost, active time,
  lines, and per-subagent attribution via `agent.name`. It has no project on the
  wire, so it is joined to a project through the session index.
- **Hooks** (event) carry the project context (`cwd`) and the domain events OTel
  can't express: tool calls, prompt sizes, memory loads, subagent stops,
  compactions — and anything a plugin defines.

Two binaries:

| Binary | Role |
|---|---|
| `hatel-hook` | wired into `settings.json` hooks; reads one event on stdin, maps it through the registered bindings, records any matches, exits. No async runtime — ~3 ms cold start. |
| `hatel` | the receiver (`serve`), reports, `init`, `service`, `doctor`, and `kinds`. |

## Install

One line — downloads the prebuilt binaries (receiver + hook), verifies their SHA-256, and
installs the Claude Code skill. No Rust toolchain required:

```sh
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/hatel/main/scripts/install.sh | bash
```

Append `-s -- --wire` to wire Claude Code in the same step (`| bash -s -- --wire`); pin a
release with `HATEL_VERSION=0.1.0`. Remove everything later with the matching
`scripts/uninstall.sh`.

From a clone, the same script builds both binaries from source when no prebuilt release exists
for your platform (or pass `--source` to force it):

```sh
git clone https://github.com/junyeong-ai/hatel && cd hatel
./scripts/install.sh            # prebuilt if available, otherwise a source build
```

Or install straight from git with cargo:

```sh
cargo install --git https://github.com/junyeong-ai/hatel hatel-cli   # the receiver
cargo install --git https://github.com/junyeong-ai/hatel hatel-hook  # the hook
```

## Wire it into Claude Code

`hatel init` does this for you — it merges the telemetry `env` and the lifecycle
hooks into `settings.json`, idempotently and non-destructively (it appends our hook without
touching your own, and never overwrites an endpoint you've repointed at a corporate collector):

```sh
hatel init                 # ~/.claude/settings.json (all projects)
hatel init --scope local   # .claude/settings.local.json (this repo, per-dev)
hatel doctor               # verify the wiring and explain any gaps
hatel init --remove        # cleanly undo (leaves the native telemetry env)
```

Telemetry config for Claude Code itself must live in `settings.json` `env` — that
is the only channel Claude Code reads at session start, and those `OTEL_*` vars are
deliberately **not** passed to hook subprocesses, which is exactly why the two
layers are separate. For an org, paste the equivalent block (printed by
`hatel init --print`) into managed settings. The full shape:

```jsonc
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_METRICS_EXPORTER": "otlp",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:4318"
  },
  "hooks": {
    "SessionStart":      [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "SessionEnd":        [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "PostToolUse":       [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "SubagentStop":      [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "InstructionsLoaded":[{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "PreCompact":        [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "PostCompact":       [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }]
  }
}
```

The `command` is shown as the bare name for readability; `hatel init` (and `--print`) write the
**absolute** path to `hatel-hook` beside `hatel`, so Claude Code can spawn it without relying on
`PATH`. `http/json` is required: this receiver decodes the JSON OTLP encoding so it needs no
protobuf dependency. Putting the block at the **user** level feeds one collector from every
project; the receiver still shows only the current project by default.

Then run the receiver:

```sh
hatel serve            # live view, current project only
hatel serve --all      # every project sharing this collector
```

The receiver is a single-writer daemon: it binds `127.0.0.1:<port>`, so a second
instance on the same port exits and the cost snapshot has exactly one writer. It
always answers `200` — the status reflects that the body was *received*, not whether
this build could decode it, so a raw tee of a body the local view can't read still
succeeds and an OTLP client never retries (a retry would inflate delta counts). An
undecodable body is noted to stderr and surfaced by `doctor` (which detects a wrong
protocol from the settings), never via a status code. It caps request bodies at 64 MB
and recovers a poisoned lock rather than crashing. Durable state is bounded by
retention (`HATEL_RETENTION_DAYS`); the in-memory per-session accumulator is bounded
by the sessions seen since the process started, which a normally-restarting daemon
keeps small.

## Forwarding to other collectors (export)

The receiver can forward what it ingests to one or more downstream OTLP/HTTP
collectors, so you no longer have to choose between hatel and a corporate collector —
hatel sits in front and tees to it. This mirrors an OpenTelemetry Collector pipeline:
the receiver decodes locally and forwards onward; fan-out to Prometheus/Honeycomb/etc.
is the downstream collector's job, so hatel emits **OTLP only**.

Configure destinations in `config.toml` (`$HATEL_CONFIG`, else
`<config-dir>/hatel/config.toml`). Each `[[export]]` is one destination and the
transform applied on the way there:

```toml
[[export]]
endpoint = "http://collector.corp:4318"   # /v1/metrics and /v1/logs are appended
mode = "enriched"                           # raw | enriched
headers = { authorization = "…" }           # e.g. downstream auth (never logged by value)
# timeout_ms = 5000

[[export]]
endpoint = "http://archive:4318"
mode = "raw"
```

- **`raw`** forwards the incoming OTLP byte-verbatim — protocol-agnostic, so it tees a
  `protobuf` body too.
- **`enriched`** injects the `project` label (joined from `session.id`) into each
  datapoint/record, so the downstream backend gains the project-level attribution that
  raw OTel structurally lacks. It needs an `http/json` stream to transform; a datapoint
  whose session is unknown is forwarded unchanged (the label is never fabricated).

A/B is a per-destination transform, not two toggles: the same endpoint with both would
double-count delta metrics, so a duplicate endpoint is rejected at load. Forwarding is
best-effort and fail-open — a slow/unreachable downstream never blocks ingestion (the
queue drops and counts under pressure) and is never retried.

> **Egress is not redacted.** `raw`/`enriched` forward the full OTLP body off the host;
> hatel's allow-list/hashing applies to the hook ledger, **not** to this egress. `doctor`
> prints this as a standing warning whenever export is configured. (Because hatel is
> content-free by construction it carries no prompt/tool bodies, so this is still safer
> than pointing Claude Code's raw OTel at a corporate collector — but it is off-host.)

### Insert hatel in front of an existing collector

Export only forwards what reaches the receiver, so Claude Code's endpoint must point at
hatel. If it currently points at a corporate collector, one command captures it and
repoints Claude Code at hatel — keeping the collector and gaining hatel:

```sh
hatel init --insert                 # capture the current endpoint as an enriched target, repoint CC
hatel init --insert --mode raw      # …forwarding byte-verbatim instead
```

It writes the captured endpoint (and its auth headers) into `config.toml`, repoints the
scope that set the endpoint, and verifies with `doctor`. If the endpoint is **managed-
locked** it can't be repointed — `doctor` says so plainly; only the hook ledger is then
available.

## Claude Code skill

The installer also drops a `hatel` skill into `~/.claude/skills/` (it ships in the
repo at `.claude/skills/hatel/` for anyone who clones, and is version-locked inside
each release archive). With it, Claude Code can wire the telemetry for you, diagnose gaps,
answer "how much did this project cost / which subagent burns the most tokens" from
`report --format json`, and scaffold a custom plugin Kind — all through the same binary
documented here. It loads only when the conversation is about telemetry, so it costs nothing
otherwise.

## Commands

| Command | Purpose |
|---|---|
| `serve [--port 4318] [--all] [--project N]` | OTLP/HTTP receiver + live per-session rollup, with per-subagent token/cost breakdown when subagents run. |
| `report [--window 30d] [--format md\|text\|json] [--project N] [--kind K] [--top K]` | aggregate over a rolling window — per group: record count and the sum of each Kind's `measures` — plus the cost snapshot. `--format json` for dashboards; `--top 0` shows all groups; `--kind K` scopes the rollup to one registered Kind and omits the cost snapshot. |
| `init [--scope user\|project\|local] [--print] [--remove] [--insert [--mode raw\|enriched]]` | wire (or unwire) the telemetry env + lifecycle hooks in `settings.json` — idempotent, non-destructive, atomic. `--print` emits the block for managed settings. `--insert` captures an existing corporate endpoint as an export target and repoints Claude Code at hatel. |
| `service [--remove] [--print]` | install/remove the receiver as a launchd/systemd user service for gap-free collection (runs `serve --all`). `--print` emits the unit. |
| `doctor` | verify the wiring and report policy gaps honestly (see below). |
| `kinds [--json]` | list the registered Kinds (core + plugins). |
| `emit <kind> [key=value...] [--json OBJ]` | record one domain signal for a registered Kind (field pairs, `--json`, or stdin) — the programmatic path for custom metrics. |

## Storage

Both halves of storage go through one abstraction (`HATEL_SINK`):
emitters write via the sink, and `report` reads via the same backend — so the
choice is honest end-to-end (a report consumes SQLite exactly as it does JSONL):

- `jsonl` (default) — one append-only file per Kind under the state dir, rotated at
  10 MB (`HATEL_ROTATE_BYTES`). Git-friendly, greppable, zero dependencies.
- `sqlite` — embedded, WAL, indexed by `(kind, ts)` so windowed reads stay cheap; the
  report window is filtered in SQL rather than scanning all history.

State lives under the XDG state dir (`~/.local/state/hatel`, or the
platform equivalent); override with `HATEL_STATE_DIR`. The session
index (`session_index.jsonl`) and the cost snapshot (`cost_snapshot.jsonl`) are
always written there independent of the sink — the receiver needs the index to
attribute project-less OTel data, and the cost snapshot is a per-session snapshot,
not an event stream.

The **live `serve` view** and the session index keep two same-named repositories
distinct by their git-root path (the project *key*). Stored event records and
`report` carry only the human *label* (basename) — by design, so the path never
leaves the index — so `report --project foo` matches by label and would merge two
different `foo` repos. `report --project` also drops Kinds that carry no `project`
field (an `emit` Kind that didn't include one), with no row rather than an error.

### Environment variables

| Variable | Effect |
|---|---|
| `HATEL_SINK` | `jsonl` (default) / `sqlite` |
| `HATEL_STATE_DIR` | override the state directory |
| `HATEL_CONFIG` | override the `config.toml` path (the `[[export]]` destinations) |
| `HATEL_PLUGINS` | plugin TOML paths, OS path-list separator (`:` Unix, `;` Windows) |
| `HATEL_ROTATE_BYTES` | JSONL rotation threshold (default 10 MB) |
| `HATEL_RETENTION_DAYS` | cost-snapshot retention horizon (default 90, max 100000) |
| `HATEL_DISABLED=1` | turn the hook into a no-op |
| `HATEL_STRICT=1` | error (don't silently drop) on a payload key outside the allow-list |
| `HATEL_TESTING=1` | redirect writes under a `_test/` subdirectory |

These configure the collector itself and are unrelated to Claude Code's `OTEL_*`
telemetry settings, which live in `settings.json`.

## Extending: custom per-project metrics

A plugin is a TOML schema file — no code, no recompile. It contributes Kinds (and
optionally hook bindings) through the same loader the core uses. Point at it with
`HATEL_PLUGINS=path/to/plugin.toml` (OS path-list separator for several). Per Kind:

- `fields` — the single allow-list (anything outside it is dropped before write).
- `group_key` — the field a report groups by.
- `measures` — numeric fields a report **sums** per group (durations, counts, costs);
  the first is the primary metric groups are ranked by. A numeric string sums the same
  as a number, so an `=` vs `:=` slip on `emit` doesn't silently zero a measure.
- `redact` — fields hashed before storage.

A duplicate Kind name is a hard error at startup, so namespace plugin Kinds
(`team.deploy`) to avoid colliding with core's flat names (`tool`, `prompt`). Kind
names are restricted to `[A-Za-z0-9._-]` (they are also ledger filenames).

A custom Kind is filled by one of two paths. **Choose by where the signal
originates**, and keep **one writer per Kind** (a Kind written by both paths
double-counts):

- A signal the Claude Code lifecycle can observe → a **hook binding** (zero code,
  auto-attributed to the session's project).
- A signal that only your project's own logic knows → **`emit`** (your tooling
  records it; the collector cannot observe it).

**1. Hook binding** — for a signal derivable from a Claude Code lifecycle event,
with zero code:

```toml
[[kind]]
name = "team.deploy"
fields = ["session_id", "project", "service", "ok"]
group_key = "service"

[[binding]]
event = "PostToolUse"
kind = "team.deploy"
map.session_id = { from = "session_id" }
map.service   = { from = "tool_name" }
map.ok        = { from = "tool_response", present = true }
```

Field-map transforms: `from` (passthrough; a list `["a","b"]` tries each in order,
tolerating a version-sensitive field name), `capture` (regex group 1), `len`
(string length), `present` (field present → bool), `basename` (final path
component), `const`. A transform that doesn't apply omits the field — never
fabricated. When (and only when) a binding maps from `git_branch`, the hook reads it
from `.git/HEAD` (no subprocess), so a project can derive a spec slug with zero code:
`map.spec_slug = { from = "git_branch", capture = "^spec/(.+)$" }`.

**2. `emit`** — for a domain signal that is *not* a Claude Code event (a spec-gate
decision, a rule-check rollup, a deploy outcome). Your tooling records it directly:

```sh
# field pairs — key=value is a string, key:=value is JSON (numbers, bools, arrays)
hatel emit ci_check check=lint date=2026-06-09 runs:=14000 failures:=3
# or a whole JSON object via --json, or piped on stdin
echo '{"check":"lint","runs":14000}' | hatel emit ci_check
```

The `key=` / `key:=` split keeps types explicit — they are never guessed from the
string. `emit` validates the Kind, applies the same allow-list and redaction, and
writes via the active sink. A field the Kind doesn't accept is dropped (allow-list) but
**warned to stderr with the list of accepted fields**, so a typo surfaces immediately
instead of silently vanishing. An unknown Kind or malformed input exits non-zero (the
caller learns it wasn't recorded); an IO error stays fail-open. It is
language-agnostic — any project, in any language, calls the binary. Run
`hatel kinds` to see every registered Kind and its exact fields.

Unlike a hook, `emit` does **not** infer the project from its working directory: the
emitting process (a scheduler, a CI job) may run anywhere, so guessing would
mis-attribute. For cross-project analytics, include the attribution you want
(`project`, a slug, an org) as fields in the payload — the caller knows them, the
collector can't. `plugins/example.toml` is a worked example: a CI-check rollup recorded
by your own tooling, alongside a zero-code branch-attribution Kind.

## Privacy

- The allow-list is the primary defense; the core ships **no** content-bearing
  fields. Prompts store length, tools store the name — never the text or arguments.
  This mirrors Claude Code's own default-off `OTEL_LOG_USER_PROMPTS` /
  `OTEL_LOG_TOOL_DETAILS`.
- `redact` fields are hashed (BLAKE3, 16 hex chars) before write.
- Event records carry the project **label** only; the absolute git-root path lives
  solely in the local session index.
- Everything stays on your machine. Failures are fail-open: a write error degrades
  to a stderr note and never blocks a tool call.

## Always-on collection (no gaps)

Native OTel is push-only: tokens and cost are captured only while the receiver is running. For
gap-free user-level collection, install it as a background service — `hatel` writes and loads the
unit for you (a launchd LaunchAgent on macOS, a systemd `--user` unit on Linux):

```sh
hatel service           # install + start: runs `serve --all`, kept alive across login/failure
hatel service --remove  # stop and remove it
hatel service --print   # print the unit instead of installing — to inspect or hand to MDM
```

The unit runs the exact binary that installed it, so re-running `hatel service` after a
`cargo install` or `--bin-dir` move repoints it. (`scripts/install.sh --service` does this in the
same step as install.)

## Enterprise / managed settings

The collector never fights managed policy; it adapts:

- **OTel repointed at a corporate collector** — the local hook ledger keeps
  working; the `session.id` join holds wherever the native data lands, so metrics
  query from the corporate backend and join to the local domain ledger by session.
- **`allowManagedHooksOnly`** — user/project hooks are blocked, so IT deploys
  `hatel-hook` as a *managed* hook (the single static binary ships via
  MDM). `doctor` detects this from the file-based managed settings (macOS / Linux /
  Windows paths); it does not read non-file managed sources (Windows registry, MDM
  server drop-ins).
- **`OTEL_METRICS_INCLUDE_SESSION_ID=false`** — per-session attribution becomes
  impossible. `doctor` reports it plainly; org/user aggregates still work. There is
  no guessed fallback — an unavailable signal is reported as unavailable, never
  fabricated.

## Layout

```
crates/core   async-free library: model, registry, schema, pii, sinks, session, hook, report
crates/hook   the lean hook binary (core only)
crates/cli    the receiver, reports, doctor (core + tokio/axum)
plugins/      example declarative plugins
```
