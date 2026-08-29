---
name: hatel
version: 0.7.0
description: Set up, diagnose, and query hatel — the local Claude Code telemetry collector. Use when the user wants to wire Claude Code telemetry into settings.json, find out why cost or token data isn't showing up, report on Claude Code cost / token / subagent usage for a project, or add a custom per-project metric.
when_to_use: "Trigger phrases: set up telemetry, wire up the hooks, how much did Claude Code cost, token usage this month, which subagent burns the most tokens, why is cost empty, telemetry doctor, add a custom metric, track deploys in telemetry."
allowed-tools: Bash, Read, Edit
---

# hatel

Two binaries: `hatel` (the receiver — `serve`, `report`, `init`, `service`, `doctor`, `kinds`,
`emit`, `mcp`) and `hatel-hook` (wired into Claude Code lifecycle events; runs automatically —
never invoke it by hand). The receiver runs locally; by default nothing leaves the machine
(opt-in export tees downstream — see Forwarding).

Always check wiring with `hatel doctor` first when something looks off — it reports
each gap honestly (it never fabricates a missing signal) and exits non-zero when the wiring is
incomplete, so you can gate on it. `hatel doctor --json` returns the same findings as stable
JSON (top-level `ok`, per-section `findings` with `status` `ok`/`fail`/`warn`/`note`) — prefer
it when you need to branch on a specific gap.

`hatel mcp` serves `report` / `kinds` / `doctor` / `emit` as typed MCP tools over stdio
(`claude mcp add hatel -- hatel mcp`); the read tools return exactly the JSON their CLI
counterparts print (`--format json` / `--json`), and `emit` answers with its outcome as
text — so everything below applies to both surfaces.

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
hatel init --insert        # keep a corporate endpoint AND route through hatel (see Forwarding)
hatel doctor               # verify and explain any gaps
```

`doctor` gaps and what they mean:
- **no hook invokes …** → run `init`.
- **BLOCKED by allowManagedHooksOnly** → IT must deploy the hook as a *managed* hook (MDM).
- **OTEL_METRICS_INCLUDE_SESSION_ID=false** → per-session/project attribution is impossible;
  hatel drops session-less metrics rather than guess (org/user aggregation only survives at a
  downstream collector you forward the raw stream to). There is no fallback — report it as-is.
- **OTEL_EXPORTER_OTLP_PROTOCOL not http/json** → this receiver only decodes `http/json`.
- **export forwards nothing / endpoint bypasses this receiver** → export only forwards what
  reaches hatel; the OTel endpoint must point at hatel. Run `hatel init --insert` (or, if the
  endpoint is managed-locked, only the hook ledger is available — report it as-is).

## Forward to other collectors (export)

The receiver can tee what it ingests to downstream OTLP/HTTP collectors, so you keep a corporate
collector *and* gain hatel. Destinations live in `config.toml` (`$HATEL_CONFIG`, else
`<config-dir>/hatel/config.toml`), one `[[export]]` per destination:

```toml
[[export]]
endpoint = "http://collector.corp:4318"   # /v1/metrics, /v1/logs appended
mode = "enriched"                           # raw (byte-verbatim) | enriched (injects `project`)
headers = { authorization = "…" }           # never logged by value
```

- `raw` forwards verbatim (protocol-agnostic, tees protobuf too); `enriched` injects the
  `project` label so the downstream backend gains attribution raw OTel lacks (needs `http/json`).
- A duplicate endpoint is rejected (it would double-count); forwarding is best-effort and never
  retried; **egress is NOT redacted** (it forwards the full OTLP body off-host — `doctor` warns).
- `hatel init --insert [--mode raw|enriched]` captures an existing corporate endpoint as a
  target and repoints Claude Code at hatel in one step (this is how export becomes usable when the
  endpoint already points elsewhere). Always finish with `hatel doctor`.

## Analyse cost & usage

Run the receiver to capture native OTel (cost/tokens are push-only — captured only while it
runs); query history with `report`. Use `--format json` whenever you need to parse or compare.

```bash
hatel serve --all                          # live view; leave running to collect
hatel report --window 30d --format json    # machine-readable rollup + cost snapshot
hatel report --project <label> --format json
hatel report --kind <name> --format json    # scope to one Kind (omits the cost snapshot)
hatel report --kind <name> --filter field=value   # only records matching every --filter
hatel report --kind <name> --group-by <field>     # a dimension other than the Kind's default
hatel report --kind <name> --sort-by <measure>    # rank by a measure other than the first
hatel report --top 0                        # all groups, not just the top N
hatel kinds --json                          # queryable Kinds + any the ledger holds unreadable
```

Reading a report: each Kind lists groups with a record count and the summed `measures`; the
`cost` array is the latest snapshot per session (`session_id`, `project`, `tokens`, `cost_usd`,
`active_time_s`, `lines`, `ts`, plus three breakdowns). Answer the budget questions from those
breakdowns: `by_agent` (tokens/cost per subagent — "which subagent costs most"), `by_model`
(the model mix — Opus vs Haiku spend), and `tokens_by_type` (`input`/`output`/`cacheRead`/
`cacheCreation` — compute the cache-hit ratio as `cacheRead / total`). A series missing the
dimension lands in `(unattributed)` — report it as such, never guess. Sessions recorded before
the breakdowns existed show `{}` (not recorded — say so rather than treating it as zero).
`report --project <label>` matches by the project's basename label. A Kind that carries no
`project` field records none, so a project scope cannot select it: its `project_scope` reads
`unsupported` and it renders as a note, not an empty table — read that as "not applicable",
never as zero usage. `unreadable_kinds` — on every report, and on `kinds --json`, whose payload
is `{"kinds": [...], "unreadable_kinds": …}` — is non-null when the ledger holds Kinds no loaded
schema declares: the answer covered less than was collected. Report that gap with the names and
the surface it carries, never the totals alone.

Each Kind section names the axes it was computed on (`group_by`, and `sort_by` — `null` means
groups rank by record count). `--group-by` and `--sort-by` (both need `--kind`) change the
question without touching the schema: the Kind declares the defaults, the query overrides them.
Name a field outside the Kind's allow-list, or a measure it does not declare, and it is a loud
error rather than an empty answer.

`--filter` (repeatable, needs `--kind`) matches a field exactly by the rendering the group-key
column shows; a redacted field is matched by its *original* value (the query is hashed exactly
as the ledger stored it). A field outside the Kind's allow-list is a loud error, never an empty
report. Retention is governed by `HATEL_RETENTION_DAYS` (default 90 days): the receiver prunes
older ledger archives and cost rows, so a `--window` beyond the horizon shows only what is
retained — say so rather than presenting it as low usage.

## Add a custom per-project metric

A plugin is a TOML schema file (no code, no recompile). List it under `plugins` in
`config.toml` (`$HATEL_CONFIG`, else `<config-dir>/hatel/config.toml`; relative paths resolve
against that file's directory), then confirm with `hatel kinds`. Configuring it there is what
makes the write and read paths agree — `HATEL_PLUGINS` overrides the list for one process only,
so a Kind registered that way is invisible to any command run without it. `doctor`, `kinds`,
`report`, and the error from `--kind <that name>` all name such a Kind and the surface where its
schema would be listed — listing it there is the fix. **Choose the path by where the signal
originates, and keep one writer per Kind** (a Kind written by both paths double-counts; a
receiver-sourced Kind like `tool` is refused by `emit` outright, exit 2):

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
hatel emit ci_check check=lint date=2026-06-09 runs:=14000 failures:=3 project=acme-api
echo '{"check":"lint","runs":14000,"project":"acme-api"}' | hatel emit ci_check
```

`emit` does **not** infer the project from its working directory (the emitter may run anywhere),
so include attribution (`project`, a slug) as fields. A field the Kind doesn't accept is dropped
*and warned to stderr* with the accepted list (under `HATEL_STRICT=1` it is an error and nothing
is written) — surface that to the user rather than ignoring it.
See `plugins/example.toml` for a worked example.
