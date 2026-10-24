---
name: hatel
version: 0.23.1
description: Records and queries local telemetry with hatel, including custom domain Kinds, check executions, state snapshots, Claude Code cost and token usage, instruction loads, storage diagnosis, and selective downstream delivery. Use when configuring hatel, investigating missing measurements, or reporting recorded usage and outcomes.
allowed-tools: Bash, Read, Edit
---

# Hatel

Use the installed contract: `hatel --version`, `hatel <command> --help`, and
`hatel kinds --json`. This skill targets 0.23.1. A running receiver or MCP server
keeps the build it started. After upgrading, restart the receiver and reconnect Hatel
through Claude Code's `/mcp` → `hatel` → `Reconnect`; the conversation can stay open.

Choose the relevant operation below. Local domain records need neither Claude Code
nor a receiver. `hatel-hook` runs through lifecycle wiring; do not invoke it manually.

## Query recorded facts

1. Inspect `hatel kinds --json` for loaded schemas, fields, mode, identity and measures.
2. Choose Kind, project, window, filters and grouping. Use JSON for parsing or comparison.
3. Before interpreting totals, check section `error`, excluded `contracts`, `storage_error`,
   `cost_error`, `unreadable_kinds`, measurement counts and retention limits. An empty answer
   does not establish zero activity.

```sh
hatel report --kind check --window 7d --format json
hatel report --kind check --filter outcome=failed --group-by name --top 0
hatel report --kind check --group-by metadata.source --sort-by duration_s
hatel report --kind claude.api_request --window 7d --group-by model --format json
hatel report --kind memory --group-by file_path --format json
hatel report --kind tool --filter tool_name=Skill --filter ok=1 --group-by skill
hatel report --project /absolute/repository/path --format json
```

`--filter` is repeatable exact equality; all filters must match. An empty string differs
from an absent field. `--filter`, `--group-by` and `--sort-by` require `--kind`.
`--top` cuts every list: Kind groups, `cost` sessions and `cost_groups` projects; `group_count`,
`cost_session_count` and `cost_group_count` give totals, and `--top 0` returns all. Filter hashed
payload fields by their original value.
`--project` accepts canonical key or display label; labels may combine equal-named repos.
`project` grouping and filtering prefer the common key, falling back to the payload's `project`,
when the Kind declares that field, for records without one. Use `metadata.project.key` to address that common field explicitly.

A section answers under the current contract plus stored contracts it can read: same name, version
and mode, and for a snapshot the same identity; every field it declares keeps type, unit and
redaction; every field it requires was required when written.
`contracts[].resolution` is `selected`, `compatible`, `incompatible` or `undefined`; only the first two
are aggregated and covered by `retained_since`. State excluded counts with the answer. Inspect
`stored_contracts` in `kinds --json`; `report --kind KIND --contract SCHEMA_ID` uses its saved
definition, including retired Kinds. Records without a known definition are counted, never
interpreted: never infer their identities, units or semantics.

Report interpretation:

- `sum` requires its `valid_count`; `missing_count` and `invalid_count` name what was
  excluded. Human `—` means unmeasured, `7 (1/3)` a sum over one measured record of three.
  A measured zero remains zero. Numeric strings are not coerced into measures.
  Aggregates use f64; integers above 2^53 can lose precision. Record identifiers as strings.
- `retained_since` is the oldest retained record, not proof of continuous collection.
  `unreadable_kinds` means some stored records have no loaded schema.
- Event mode counts records, or the earliest matching identity per group when declared.
  ID-less event records count individually. This is not storage deduplication.
- Snapshot mode selects the latest occurrence per project and identity inside the window,
  under every contract, before filtering/grouping; an entity whose latest state is unreadable
  is left out. Equal occurrences use observation time; conflicting ties fail. Unchanged
  entities outside the window are absent: no complete-inventory claim.
- Native `cost` is lifetime session totals selected by last activity, as `cost_basis`
  states. Use `claude.api_request` for occurrence-window request cost estimates.
  Neither is an invoice. `observed_metrics` absent means measurement coverage is unknown.
- Session cache-write estimates forecast a later request; never add them to native costs.
  Never subtract active time from wall time to invent idle time or infer saved time/ROI.
- Subagent labels can include internal agents. A missing memory-load announcement does
  not establish that the file was absent from context. Tool duration ends at return,
  including early acceptance of a background command.

MCP server `hatel` (`claude mcp add hatel -- hatel mcp`): `hatel:report`, `hatel:kinds`,
`hatel:doctor` and `hatel:deliveries` return the JSON their CLI commands print; `hatel:emit` and
`hatel:emit_batch` share recording validation and return storage receipts or ordered per-item
results. A failed call returns `isError` with a message naming what to correct.

## Record or extend a domain signal

Reuse `check` for producer-observed command executions: required `name`, `outcome`,
optional `suite`, `duration_s`, actual `exit_code` or `signal`. Keep timeout, spawn error,
signal and reuse distinct at the producer. Do not infer a verdict from magic exit codes,
stdout, tool names or elapsed-time thresholds. An absent measurement stays absent.

```sh
hatel emit check name=lint suite=quality outcome=passed duration_s:=1.25 --receipt
printf '%s\n' '[{"kind":"check","payload":{"name":"lint","outcome":"passed"},"context":{"source":"quality-tool","run_id":"run-123"}}]' | hatel emit-batch
```

Run in the correct repository directory: CLI/MCP derive `metadata.project` from actual
Git identity; linked worktrees share the repository, and outside Git no project is invented.
Common context accepts `occurred_at` (RFC 3339), `source`, `run_id`, `correlation_id`.
`key=value` is a string; `key:=value` is JSON. A whole object can use `--json` or stdin.

For domain-specific facts, add a namespaced schema TOML to the operator's existing
`plugins` list in `config.toml`, preserving other entries. Relative plugin paths resolve
against that file. Never replace `HATEL_PLUGINS` inside a project integration.

```toml
[[kind]]
name = "team.deploy"
version = 1
fields = { service = { type = "string", required = true }, outcome = { type = "string", required = true }, duration_s = { type = "number", unit = "s" } }
group_key = "service"
measures = ["duration_s"]
```

Typed fields support `string`, `number`, `integer`, `boolean`, `array`, `object`, `any`,
requiredness and units. `redact` hashes selected payload fields before storage.
For state, add `mode = "snapshot"` and `identity` naming a required non-empty string field.
A data contract's opaque `schema_id` changes with field, unit, identity, redaction or mode changes.
Adding an optional field, removing a field or making one optional keeps earlier records readable;
a type, unit or redaction change, a newly required field or a changed snapshot identity does not.
Pending deliveries cancel under an unreadable contract and when they carry a removed field. Semantic changes require an explicit Kind/version and history decision.

Use hook bindings only for facts present in lifecycle input. Producers own domain meaning.
Keep one writer for each fact. Check `hatel kinds --json` and exercise a real receipt/query
in an isolated store when changing a schema; do not write test records to the user's store.

Single writes exit 0 stored, 1 when configuration, registry or store is unusable, 2 for rejected
input. Batches exit 0 all stored, 1 when some items were not stored, 2 when the call failed before
any item (invalid input, unusable configuration or registry); only 0 and 1 print per-item
results, and the whole batch is not atomic. `hatel:emit_batch` sets `isError` only
for a whole-call failure, so resend only the items not `stored`. CLI stdin cap is 8 MiB; an MCP
message above 16 MiB is dropped without a reply; batches allow 4096 items; sanitized payloads
allow 256 KiB. A receipt confirms local storage. Inspect warnings and `delivery` enqueue outcomes; neither confirms downstream receipt.
Detached best-effort producers cannot claim acknowledged storage or exactly-once recording.

## Configure local storage or delivery

Find the actual path with `hatel doctor --local --json`. Shared settings belong in
`config.toml` (`$HATEL_CONFIG`, otherwise the platform's `hatel/config.toml`). Per-process
storage or plugin overrides can split writers from readers and services.
The store is one SQLite/WAL database (`telemetry.db`) in the state directory. A store at a
schema this build did not create refuses reads and writes and is never altered; `doctor --local`
and `/healthz` report it, and moving `telemetry.db` aside starts a new store. Restart the receiver after configuration changes. Do not restart a live
service just to test an integration.

Each destination is `[[export]]`. Native metrics only, with no logs or local Kinds:

```toml
[[export]]
endpoint = "https://collector.example.com"
mode = "raw"
signals = ["metrics"]
kinds = []
```

To include selected local records, set `kinds = ["check", "team.deploy"]`; native selection
is independent. Omitted `signals` means metrics+logs, omitted `kinds` means no local records.
`signals=[]` can send domain records alone. `projects` or `exclude_projects` match key/label;
unresolved attribution fails closed for filtered targets. Duplicate endpoints are rejected.

Native forwarding is best-effort and carries the full body without local payload redaction.
Local record delivery commits an outbox row with the record. The receiver
retries the same ID under current policy, bounded by capacity, age, attempts and retention.
Queue refusal does not undo local storage. Inspect `hatel deliveries --json`: grouped by
destination/status/error reason. Failed delivery differs from failed storage; pre-send
encoding refusal is not an HTTP attempt. Lost acknowledgements can duplicate delivery.
Local projections are not exported again with their native source.

## Diagnose Claude Code wiring

Use `hatel doctor --json`; for ordinary local records use `--local` instead. Report findings
with their actual `status` and scope. Full diagnosis returning nonzero means wiring is
incomplete, not that every local recording path is broken.

`hatel init` merges user wiring without replacing existing hooks or corporate endpoints.
Project/local scopes wire hooks only. Native enablement/destinations belong in the launch
shell, user settings or managed settings. `init --print` supplies a managed-settings block;
`init --insert` captures a corporate destination before routing through Hatel. Inspect its
signals/Kinds. Respect managed endpoint and hook locks.

The receiver locally supports uncompressed HTTP/JSON sums, gauges and selected safe events.
It drops session-less metrics; compound metric dimensions are explicitly unsupported locally.
Duplicate keys and invalid attribute types are errors, not default values. Traces return 501.
Hook-observed project attribution takes precedence; explicit VCS browser URLs can identify
unseen sessions, without guessing equality with local paths. Cost needs a running receiver.
`/healthz` names build, configuration, store and capabilities.

Doctor cannot inspect another session's launch environment. Claude Code strips `OTEL_*`
from child processes; the tool environment alone does not prove the session's destination.
For a configuration/store/build mismatch, resolve the named path or restart the appropriate
receiver. `hatel service --print` inspects service wiring; preserve hand-edited units.
Logs: `~/Library/Logs/hatel/serve.log` on macOS, `journalctl --user -u hatel` on Linux.

The store and its reports can contain paths and identifiers. Hashing is pseudonymization, not
guaranteed anonymity; never paste raw records into external services without considering their fields.
