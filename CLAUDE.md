# CLAUDE.md

hatel is a local telemetry record store and query tool with a Claude Code adapter: an OTLP/HTTP receiver (`hatel serve`)
joined with lifecycle-hook records (`hatel-hook`) through `session.id`, stored locally,
queried via `report` / the MCP server. Read `README.md` (ko) / `README.en.md` (en) for the
product; this file is for working on the code.

## Build & verify

The toolchain is pinned by `rust-toolchain.toml`; edition 2024, resolver 3. CI denies all
warnings — run the same gates locally before considering work done:

```sh
RUSTFLAGS="-D warnings" cargo test --workspace --locked
RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features --workspace --locked -- -D warnings
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo +1.98.0 check --workspace --all-targets --all-features --locked
cargo audit                             # after dependency updates
actionlint .github/workflows/*.yml        # after workflow edits
uvx zizmor .github/                       # after workflow edits (security audit)
```

`RUSTFLAGS` is what the workflows set, so omitting it locally lets a warning through that CI
rejects.

Dependency updates keep `Cargo.toml` and `Cargo.lock` together. The pinned build toolchain and
`workspace.package.rust-version` have different roles; preserve the minimum supported version
and its CI check unless a deliberate compatibility change requires raising both.

## Layout

- `crates/core` — async-free library: registry/schema (Kind definitions, `core.toml`),
  settings (`config.toml`) and the resolved runtime config, SQLite/WAL ledger, JSONL compatibility/import, sessions,
  cost snapshot, report aggregation and its render, PII. `claude/` owns lifecycle interpretation.
- `crates/hook` — the hook binary; depends on core only, no async runtime (spawned on
  every lifecycle event — cold-start is a design constraint).
- `crates/cli` — the `hatel` binary: `serve` (receiver), `report`, `init`, `service`,
  `doctor`, `kinds`, `emit`, `emit-batch`, `deliveries`, `migrate`,
  `mcp` (stdio MCP server via the official `rmcp` SDK).
- `.claude/skills/hatel/SKILL.md` — the Claude Code skill; packaged into every release
  archive and installed by `scripts/install.sh`.

## Load-bearing conventions

- **Never fabricate a missing signal.** A series without a dimension buckets under
  `(unattributed)`; session-less metrics are dropped, not guessed; `doctor` reports gaps
  instead of inventing fallbacks. New code follows this or doesn't merge.
- **Cost bases stay explicit.** Session cache-write forecasts are not added to native OTel costs.
  `cost_basis` names the snapshot as lifetime session totals selected by last activity.
  `claude.api_request` records answer occurrence-window questions from native request cost
  estimates; missing request costs stay missing. Neither source is a billing invoice.
  `CostRow.observed_metrics` names the native metrics this build has seen; an absent metric is
  not a measured zero. `project_key` keeps same-named repositories apart.
- **Vendor interpretation stays in adapters.** `crates/core/src/claude` maps lifecycle hooks;
  `crates/cli/src/claude` decodes Claude Code's metrics and safe event projections. Plugins add
  record Kinds and bindings, not unused native counters. Repository attributes provide a
  `vcs:<browser-URL>` identity when the session index has not observed the session; a known
  unattributed start is never replaced. Local decode supports HTTP/JSON sums, gauges and selected
  events; native forwarding can relay opaque bodies. Traces are unsupported and return 501.
  `/healthz` reports these capabilities.
- **Routing is destination-specific.** `signals` selects native OTLP input; `kinds` is an exact
  opt-in allow-list for sanitized local records. Omitted keys preserve metrics+logs and no local
  Kinds. A local record and its delivery rows commit together. Network work belongs to the
  receiver; a durable retry reuses `record_id` and rechecks current policy. Native forwarding
  stays best-effort. Vendor projections set `metadata.projection` so forwarding the original
  signal cannot also export its local projection. Queue counters change in the same transaction
  as delivery state; retention deletes delivery references and releases capacity with records.
  A Kind data-contract fingerprint accompanies each record. A changed contract cancels pending
  records rather than interpreting them under new redaction or field rules.
  Identity is part of that contract; query defaults are not. SQLite records and their validated
  definitions commit together in `contracts`. Imports never invent definitions for old fingerprints.
- **Native telemetry follows Claude Code's environment policy.** Project/local init writes hooks
  only. Diagnosis applies repository restrictions and managed destination locks without changing
  user settings; inherited variables describe this process, not another session's launch.
- **A project is a repository, never the directory work ran in.** A linked worktree attributes
  to the repository it checks out; a tree outside one has no project. Every identified session
  start is recorded either way, so the receiver can tell a session that has no project from one
  whose start it has not seen — the first is answered now, the second is the only one worth
  holding an egress batch back for. Collapsing those two states again silently restores
  a fabricated project, a saturated export park, or both.
- **Fail-open at the lifecycle hook boundary** (a write error is a stderr note, never a blocked
  tool call). CLI/MCP writes return a storage receipt or an explicit failure. **Fail-closed on egress privacy** (an unattributable batch is not forwarded
  to a filtered destination).
- `#[serde(deny_unknown_fields)]` on every config/schema surface — a misspelled key must
  fail loudly, never silently disable a feature. `config.toml` therefore has exactly one
  typed shape (`settings.rs`) covering every section: a second parser over the same file
  would have to tolerate the sections it doesn't own, and a writer that rebuilt the file
  from its own section alone would drop the rest. A storage variable's or a `HATEL_*` flag's
  value is held to the same bar: `Config` resolves one it cannot read as unset and returns it
  beside the config, for `Config::load` to refuse and the hook to note.
- **A lifecycle event marks a turn boundary, not an entity.** `SubagentStart`, `SubagentStop` and
  `SessionStart` all fire again when their subject is resumed. Before binding a Kind that claims to
  count something, check how often its event fires per one of that thing. `identity` names the
  entity the Kind exists to count, never a deduplication key: `subagent` declares `agent_id`
  because a spawn is what it counts, while `session` declares none — `/resume` re-enters the same
  `session_id`, so keying on it would erase every resume and the cache spend each one pays.
- **Snapshots describe state rather than repeated events.** A Kind with `mode = "snapshot"`
  requires a non-empty string `identity`. Reports select the latest occurrence per project and
  identity within the requested window before filtering or grouping. Equal occurrences use observation
  time; conflicting states tied on both times fail the section, as do missing retained identities
  or observation times. Legacy records without observation time cannot be reinterpreted as snapshots.
  Event mode remains the default and
  its identity selects the earliest matching record per group. An unchanged entity outside the
  window is absent, so a rolling snapshot report is not a complete current inventory.
- **A schema describes data; a query asks a question of it.** A Kind declares its fields,
  its measures, and the dimension/measure a report *defaults* to; `--group-by` / `--sort-by`
  override those per query. Answering a new question is a query, not a schema edit.
  The common `check` Kind records producer-observed execution outcomes and seconds. Producers
  distinguish a command exit, timeout, spawn error, signal and reuse at their execution boundary;
  missing durations or exit codes stay absent. It implies no defect, flakiness or saved time.
- **Typed records and explicit missingness.** A Kind's field declarations own types, requiredness
  and units. Numbers are JSON numbers; invalid legacy values remain visible as `invalid_count`.
  Reports aggregate inside SQLite with bounded caches, including when reading a legacy JSONL
  store. A field's duration buckets are cumulative and use its declared `s` or `ms` unit.
- **One registry and one store for the write and read paths.** Plugins and `[storage]` live in
  `config.toml`, which the hook, the service's receiver and `report` all read however each was
  started, so a Kind the hook can record is one `report` can read, in the store it reads;
  `HATEL_PLUGINS` and the storage variables override them for a single process only. Fresh stores
  default to SQLite; an existing JSONL ledger keeps its backend until `migrate --quiescent`
  imports it and saves the selection. Stop its writers first; the command holds the receiver
  lock, preserves sources, and records an atomic import manifest so retries do not duplicate rows. The receiver
  reads the file when it starts, and again before it deletes anything, keeping the longer retention
  and deleting nothing while the file cannot be read. It reports that file and its store on
  `/healthz`; `doctor` names a receiver that reads another file or writes another store, and the
  splitting variables it can see, in its own environment and in settings.json `env`. The service
  unit therefore carries nothing to configure. `doctor` names any stored Kind no loaded schema
  declares.
- **Delivery outcomes describe actual work.** A storage receipt confirms local storage and
  reports queue acceptance, not downstream delivery. `deliveries` groups by destination, status
  and error reason. Encoding rejection and an exhausted policy are not HTTP attempts. Unsupported
  OTLP integer or timestamp representations fail that record before sending; valid neighbors
  remain deliverable. Retry acknowledgements can be lost, so IDs enable downstream deduplication
  without claiming exactly-once delivery.
- **A unit is hatel's own only while it is byte-equal to a rendering some release wrote.**
  `service` and `service --restart` replace only such a unit and keep any other as a hand edit,
  since replacing it would silently drop what it sets. Changing `launchd_plist` or `systemd_unit`
  therefore moves the rendering it replaces into `earlier_launchd_plists` / `earlier_systemd_units`,
  with a copy in the classification test: the pinned-rendering tests fail on the change but cannot
  check the move, and without it every installed unit reads as hand-edited.
- **Machine outputs are one shape per question**: `--json` / `--format json` serialize
  keys alphabetically, and the MCP read tools (report / kinds / doctor) return exactly
  the JSON their CLI counterparts print. Changing one means changing both (they share
  the same functions — keep it that way). A missing group field is `key: null` (`—` in human
  output); an empty string is `key: ""` (`(empty)`). An exact empty-value filter selects only
  the latter. Keep that distinction in report explanations.
- **Read failure is not an empty store.** Reports and Kind inventories carry `storage_error`;
  reports carry `cost_error` when the native baseline cannot be read. CLI exit status and MCP
  `isError` reflect those failures. An unreadable cost baseline disables only its persistence
  until repair and receiver restart; it must never be rewritten from an empty baseline.
- **Numbers need a measurement basis.** Read a measure with its valid/missing/invalid counts.
  Human reports distinguish no measurement from zero and show the measured fraction for partial
  sums. Native summaries count a metric only when `observed_metrics` explicitly names it; a
  legacy row has unknown coverage. Aggregate overflow is a section error. Native Claude metric
  dimensions keep scalar types distinct; compound dimensions are unsupported locally rather than
  converted to empty labels. Attribute keys are unique and known identifiers are strings.
- **README samples are real output.** Human-facing CLI output is quoted verbatim in both
  READMEs; changing output text means updating `README.md` and `README.en.md` together
  (they are maintained line-for-line parallel, ko/en).
- `SKILL.md` frontmatter keeps a `version` field tracking the workspace version. It is not
  part of the official skill spec — it exists so an installed skill can be matched to its
  binaries — keep it when editing frontmatter, and bump it with `workspace.package.version`.
  The installer and release workflow copy only `SKILL.md`; keep it self-contained unless both
  packaging paths are updated to ship supporting files. Put discovery cues in `description` and
  keep query guidance separate from setup and diagnosis.
- Comments state rationale and constraints ("why", failure modes avoided) — never change
  history or narration of edits.

## Release

Pushing a tag `v*` runs `release.yml`: the test gate runs on all three shipped OSes, every
target builds on a pinned runner image (release jobs are deliberately cache-free), the
archives bundle both binaries plus the skill, provenance is attested
(`gh attestation verify <archive> --repo junyeong-ai/hatel`), and the runner-bundled `gh`
publishes the release.
