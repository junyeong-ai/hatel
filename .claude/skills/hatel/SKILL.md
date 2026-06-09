---
name: hatel
description: Set up, diagnose, and query hatel — the local Claude Code telemetry collector. Use when the user wants to wire Claude Code telemetry into settings.json, find out why cost or token data isn't showing up, report on Claude Code cost / token / subagent usage for a project, or add a custom per-project metric.
when_to_use: "Trigger phrases: set up telemetry, wire up the hooks, how much did Claude Code cost, token usage this month, which subagent burns the most tokens, why is cost empty, telemetry doctor, add a custom metric, track deploys in telemetry."
allowed-tools: Bash, Read, Edit
---

# hatel

Two binaries: `hatel` (receiver, reports, doctor, init, emit) and
`hatel-hook` (wired into Claude Code lifecycle events; runs automatically — never
invoke it by hand). The receiver runs locally; nothing leaves the machine.

Always check wiring with `hatel doctor` first when something looks off — it reports
each gap honestly (it never fabricates a missing signal) and exits non-zero when the wiring is
incomplete, so you can gate on it.

## Set up / wire

`init` wires Claude Code's `settings.json` (the telemetry `env` + the lifecycle hooks). It is
idempotent and non-destructive: it appends our hook without touching the user's own hooks, and
never overwrites an `OTEL_*` endpoint they've repointed at a corporate collector.

```bash
hatel init                 # user scope (~/.claude/settings.json — all projects)
hatel init --scope local   # this repo only (.claude/settings.local.json)
hatel init --scope project # committed, shared with the team
hatel init --print         # print the block instead (for managed/org settings)
hatel init --remove        # remove our wiring (leaves the native telemetry env)
hatel doctor               # verify and explain any gaps
```

`doctor` gaps and what they mean:
- **no hook invokes …** → run `init`.
- **BLOCKED by allowManagedHooksOnly** → IT must deploy the hook as a *managed* hook (MDM).
- **OTEL_METRICS_INCLUDE_SESSION_ID=false** → per-session/project attribution is impossible;
  only org/user aggregates remain. There is no fallback — report it as-is.
- **OTEL_EXPORTER_OTLP_PROTOCOL not http/json** → this receiver only decodes `http/json`.

## Analyse cost & usage

Run the receiver to capture native OTel (cost/tokens are push-only — captured only while it
runs); query history with `report`. Use `--format json` whenever you need to parse or compare.

```bash
hatel serve --all                          # live view; leave running to collect
hatel report --window 30d --format json    # machine-readable rollup + cost snapshot
hatel report --project <label> --format json
hatel report --kind <name> --format json    # scope to one Kind (omits the cost snapshot)
hatel report --top 0                        # all groups, not just the top N
hatel kinds --json                          # every registered Kind and its fields
```

Reading a report: each Kind lists groups with a record count and the summed `measures`; the
`cost` array is the latest snapshot per session (`tokens`, `cost_usd`, `active_time_s`, `lines`,
`project`). For "which subagent costs most", the live `serve` view breaks tokens/cost down per
subagent via `agent.name`. `report --project <label>` matches by the project's basename label
and drops Kinds that carry no `project` field — don't present that as zero usage.

## Add a custom per-project metric

A plugin is a TOML schema file (no code, no recompile). Point at it with
`HATEL_PLUGINS=path/to/plugin.toml` (OS path-list separator for several), then
confirm with `hatel kinds`. **Choose the path by where the signal originates, and
keep one writer per Kind** (a Kind written by both paths double-counts):

- A signal the Claude Code lifecycle can observe → a **hook binding** (zero code,
  auto-attributed to the session's project).
- A signal only the project's own tooling knows (a gate decision, a CI rollup, a deploy
  outcome) → **`emit`**.

```toml
# hook-bound: zero-code attribution from a spec branch (the hook reads git_branch only because
# this binding maps from it).
[[kind]]
name = "branch_work"
fields = ["session_id", "project", "spec"]
group_key = "spec"

[[binding]]
event = "SessionEnd"
kind = "branch_work"
map.session_id = { from = "session_id" }
map.spec = { from = "git_branch", capture = "^spec/(.+)$" }
```

Per Kind: `fields` (the allow-list — anything else is dropped before write), `group_key` (what
a report groups by), `measures` (numeric fields a report sums; first is the primary metric),
`redact` (fields hashed before storage). Namespace plugin Kinds (`team.deploy`) so they can't
collide with core's flat names. Field-map transforms: `from` (a list tries each in order),
`capture` (regex group 1), `len`, `present`, `basename`, `const`.

`emit` records a domain signal directly (`key=value` is a string, `key:=value` is JSON):

```bash
hatel emit ci_check check=lint date=2026-06-09 runs:=14000 failures:=3
echo '{"check":"lint","runs":14000}' | hatel emit ci_check
```

`emit` does **not** infer the project from its working directory (the emitter may run anywhere),
so include attribution (`project`, a slug) as fields. A field the Kind doesn't accept is dropped
*and warned to stderr* with the accepted list — surface that to the user rather than ignoring it.
See `plugins/example.toml` for a worked example.
