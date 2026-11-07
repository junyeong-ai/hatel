# CLAUDE.md

hatel is a local telemetry ledger with a Claude Code adapter: an OTLP/HTTP receiver (`hatel serve`)
and lifecycle hooks (`hatel-hook`) joined through `session.id`, stored in one SQLite ledger and
queried through `report` and the MCP server. `README.md` (ko) / `README.en.md` (en) describe the
product; this file is for working on the code. Rules that apply to specific paths live in
`.claude/rules/`.

## Build & verify

The toolchain is pinned by `rust-toolchain.toml` (edition 2024, resolver 3). CI denies warnings, so
run its gates with the same `RUSTFLAGS` before calling work done:

```sh
RUSTFLAGS="-D warnings" cargo test --workspace --locked
RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features --workspace --locked -- -D warnings
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo +1.98.0 check --workspace --all-targets --all-features --locked
cargo audit   # after dependency updates
```

- Dependency updates change `Cargo.toml` and `Cargo.lock` together.
- `rust-toolchain.toml` pins the build; `workspace.package.rust-version` is the minimum supported
  version with its own CI check. Raise it only as a deliberate compatibility change.
- Release: pushing a `v*` tag runs `.github/workflows/release.yml`.

## Design constraints

- `crates/hook` depends on `crates/core` only and has no async runtime: it is spawned on every
  lifecycle event, so cold start is a design constraint. `crates/core` stays async-free; the
  receiver, CLI and MCP server (official `rmcp` SDK) live in `crates/cli`.
- Vendor interpretation stays in adapters: `crates/core/src/claude` maps lifecycle hooks and reads
  transcripts, and `crates/cli/src/claude` decodes Claude Code metrics and safe event projections.
  Plugins add Kinds and bindings, not unused native counters. Local decode supports HTTP/JSON sums,
  gauges and selected events; native forwarding can relay bodies it cannot decode; traces return
  501.
- Comments state rationale and constraints, never the history of edits.

## Signals and attribution

- Never fabricate a missing signal: a series without a dimension buckets under `(unattributed)`,
  session-less metrics are dropped, and `doctor` reports gaps instead of inventing fallbacks.
- A project is a repository, never the directory work ran in. A linked worktree attributes to the
  repository it checks out; a tree outside one has no project. Path queries go through
  `project_selector` rather than restating these rules.
- Every identified session start is recorded, with or without a project, so the receiver can tell
  a session that has no project from one whose start it has not seen. Only the second justifies
  holding an egress batch back; collapsing the two restores a fabricated project or a saturated
  export park.
- Repository attributes give a `vcs:<browser-URL>` identity only to a session the index has not
  observed; a known unattributed start is never replaced.
- A lifecycle event marks a turn boundary, not an entity: `SessionStart`, `SubagentStart` and
  `SubagentStop` fire again on resume. Check how often an event fires per entity before binding a
  Kind that counts it. `identity` names the entity a Kind counts, never a deduplication key:
  `subagent` declares `agent_id`, while `session` declares none because `/resume` reuses
  `session_id` and keying on it would erase every resume and the cache spend it pays.
- Native telemetry follows Claude Code's environment policy: project and local `init` write hooks
  only; diagnosis applies repository restrictions and managed destination locks without changing
  user settings; inherited variables describe this process, not another session's launch.

## Cost

- `cost_basis` names the snapshot as lifetime session totals selected by last activity.
  `claude.api_request` answers occurrence-window questions from native request estimates, and a
  missing request cost stays missing. Neither is an invoice. Session cache-write forecasts are
  never added to native costs.
- `CostRow.observed_metrics` names the native metrics seen; an absent metric is not a measured
  zero. `project_key` keeps same-named repositories apart.
- `claude.response` reads billed responses from transcripts, so it survives native telemetry being
  routed elsewhere. A request is recorded once by its id (forks and subagents copy history), a
  token-less entry is not a billed response, and side requests never reach a transcript: it is
  never added to `claude.api_request`. A hook reads only what a transcript gained since its stored
  position, which keeps a long session's turn end as cheap as its first.
- An unreadable cost baseline disables only its persistence until repair and receiver restart; it
  is never rewritten from an empty baseline.

## Schemas, contracts and queries

- A schema describes data; a query asks a question of it. A Kind declares fields, measures and the
  dimension and measure a report defaults to; `--group-by` and `--sort-by` override them per
  query. A new question is a query, not a schema edit.
- Field declarations own types, requiredness and units. Numbers are JSON numbers; a stored value
  its type rejects stays visible as `invalid_count`. Duration buckets are cumulative in the
  field's `s` or `ms` unit.
- Every record carries its Kind's contract fingerprint (`schema_id`); identity is part of it,
  query defaults are not. A record is stored only beside its contract's definition (`contracts`).
- Readers resolve each stored contract against the answering definition (`KindSpec::reads`,
  `catalog::Contracts`): same name, version and mode, and the same identity for a snapshot; every
  declared field keeps type, unit and redaction; every required field was required when written. A
  field only the writer declared is never read, the payload `project` included. Incompatible and
  undefined records are counted per `resolution`, never reinterpreted. Historical query
  registries never record data.
- A pending delivery stays deliverable only under a definition that reads it and still declares
  every field it carries (`KindSpec::exports`).
- A snapshot Kind (`mode = "snapshot"`) requires a non-empty string `identity`. Reports pick the
  latest occurrence per project and identity in the window, under every stored contract, before
  filtering; an entity whose latest state is unreadable is left out. Ties fall to observation
  time; a conflicting tie, or a readable record missing identity or observation time, fails the
  section. A rolling snapshot report is not a complete inventory. Event identity selects the
  earliest readable matching record per group.
- `check` records producer-observed outcomes and seconds. Producers keep command exit, timeout,
  spawn error, signal and reuse distinct; missing durations or exit codes stay absent. It implies
  no defect, flakiness or saved time.
- Numbers need a measurement basis: a measure is read with its valid, missing and invalid counts,
  human output tells unmeasured from zero, and aggregate overflow is a section error. Native metric
  dimensions keep scalar types distinct, compound dimensions are unsupported locally rather than
  turned into empty labels, attribute keys are unique and known identifiers are strings.

## Storage

- One registry and one store serve writers and readers: plugins and `[storage]` live in
  `config.toml`, which the hook, the receiver and `report` read however each started.
  `HATEL_PLUGINS` and the storage variables override them for one process only.
- The ledger is one SQLite database with raw timestamps beside normalized seconds and nanoseconds.
  A record references its Kind, contract and project by id; readers read the `envelopes` view, so
  only the write path knows the tables behind it. Reports aggregate inside SQLite.
- Each ledger layout has its own `DATABASE_VERSION`, read in one statement with the records table
  (`sink::layout`). Hooks never convert data: a store at another layout is refused
  (`sink::write_readiness`), never altered. A layout change takes the next number
  (`each_layout_has_its_own_number`) and a command that converts the layout it replaces in place,
  in one transaction, which the installer runs before it restarts the receiver.
- The receiver reads `config.toml` when it starts and again before it deletes anything, keeping the
  longer retention for each Kind and deleting nothing while the file cannot be read. `/healthz`
  reports the file, the store, the local decode capabilities and what native forwarding delivered,
  failed (with the error's causes) or dropped since start; `doctor` names a receiver on
  another file or store, the splitting variables in its environment and in settings.json `env`,
  and stored Kinds no loaded schema declares.

## Delivery

- `signals` selects native OTLP input per destination; `kinds` is an exact opt-in allow-list of
  sanitized local records. Omitted keys mean metrics and logs, and no local Kinds.
- A local record, its delivery rows and the queue counters commit together; retention deletes
  delivery references and releases capacity with the records. Network work belongs to the
  receiver; a durable retry reuses `record_id` and rechecks current policy. Native forwarding stays
  best-effort, and `metadata.projection` keeps a forwarded native signal from also exporting its
  local projection.
- A storage receipt confirms local storage and reports queue acceptance, never downstream
  delivery. `deliveries` groups by destination, status and error reason. Encoding rejection and an
  exhausted policy are not HTTP attempts; an unsupported OTLP integer or timestamp fails only that
  record. Lost acknowledgements can duplicate delivery; IDs allow downstream deduplication
  without a claim of exactly-once.

## Failure and output surfaces

- Fail open at the lifecycle hook boundary: a write error is a stderr note, never a blocked tool
  call. CLI and MCP writes return a receipt or an explicit failure. Fail closed on egress privacy:
  an unattributable batch is not forwarded to a filtered destination.
- `#[serde(deny_unknown_fields)]` is on every config, schema and MCP argument surface, so a
  misspelled key fails loudly. `config.toml` has exactly one typed shape (`settings.rs`): a second
  parser would have to tolerate sections it does not own, and a writer rebuilding one section
  would drop the rest. A storage variable or `HATEL_*` flag `Config` cannot read resolves as unset
  and is returned beside the config, for `Config::load` to refuse and the hook to note.
- Read failure is not an empty store: reports and Kind inventories carry `storage_error`, and
  reports carry `cost_error`. CLI exit status and MCP `isError` reflect them; invalid arguments and
  unreadable configuration are `isError` results, never protocol errors. `emit_batch` sets
  `isError` only for a whole-call failure, since its per-item results say what was stored.
- Machine outputs are one shape per question: `--json`, `--format json` and receipts are one line
  of alphabetical-key JSON (`machine_json`), and each MCP tool returns exactly what its CLI
  counterpart prints, through the same function. Every list cut at `top_n` states its total. A
  missing group field is `key: null` (`—`), an empty string is `key: ""` (`(empty)`), and an
  exact empty-value filter selects only the latter.
