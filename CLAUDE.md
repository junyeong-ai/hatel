# CLAUDE.md

hatel is a local telemetry collector for Claude Code: an OTLP/HTTP receiver (`hatel serve`)
joined with lifecycle-hook records (`hatel-hook`) through `session.id`, stored locally,
queried via `report` / the MCP server. Read `README.md` (ko) / `README.en.md` (en) for the
product; this file is for working on the code.

## Build & verify

The toolchain is pinned by `rust-toolchain.toml`; edition 2024, resolver 3. CI denies all
warnings — run the same gates locally before considering work done:

```sh
cargo test --workspace --locked
cargo clippy --all-targets --all-features --workspace --locked -- -D warnings
cargo fmt --all --check
actionlint .github/workflows/*.yml        # after workflow edits
uvx zizmor .github/                       # after workflow edits (security audit)
```

## Layout

- `crates/core` — async-free library: registry/schema (Kind definitions, `core.toml`),
  settings (`config.toml`) and the resolved runtime config, sinks (JSONL/SQLite), sessions,
  cost snapshot, report aggregation and its render, PII.
- `crates/hook` — the hook binary; depends on core only, no async runtime (spawned on
  every lifecycle event — cold-start is a design constraint).
- `crates/cli` — the `hatel` binary: `serve` (receiver), `report`, `init`, `service`,
  `doctor`, `kinds`, `emit`, `mcp` (stdio MCP server via the official `rmcp` SDK).
- `.claude/skills/hatel/SKILL.md` — the Claude Code skill; packaged into every release
  archive and installed by `scripts/install.sh`.

## Load-bearing conventions

- **Never fabricate a missing signal.** A series without a dimension buckets under
  `(unattributed)`; session-less metrics are dropped, not guessed; `doctor` reports gaps
  instead of inventing fallbacks. New code follows this or doesn't merge.
- **A project is a repository, never the directory work ran in.** A linked worktree attributes
  to the repository it checks out; a tree outside one has no project. Every identified session
  start is recorded either way, so the receiver can tell a session that has no project from one
  whose start it has not seen — the first is answered now, the second is the only one worth
  holding egress and tool records back for. Collapsing those two states again silently restores
  a fabricated project, a saturated export park, or both.
- **Fail-open on the local write path** (a write error is a stderr note, never a blocked
  tool call); **fail-closed on egress privacy** (an unattributable batch is not forwarded
  to a filtered destination).
- `#[serde(deny_unknown_fields)]` on every config/schema surface — a misspelled key must
  fail loudly, never silently disable a feature. `config.toml` therefore has exactly one
  typed shape (`settings.rs`) covering every section: a second parser over the same file
  would have to tolerate the sections it doesn't own, and a writer that rebuilt the file
  from its own section alone would drop the rest.
- **A schema describes data; a query asks a question of it.** A Kind declares its fields,
  its measures, and the dimension/measure a report *defaults* to; `--group-by` / `--sort-by`
  override those per query. Answering a new question is a query, not a schema edit.
- **One registry for the write and read paths.** Plugins are registered in `config.toml`,
  so a Kind the hook can record is one `report` can read; `HATEL_PLUGINS` overrides it for a
  single process only. `doctor` names any stored Kind no loaded schema declares.
- **Machine outputs are one shape per question**: `--json` / `--format json` serialize
  keys alphabetically, and the MCP read tools (report / kinds / doctor) return exactly
  the JSON their CLI counterparts print. Changing one means changing both (they share
  the same functions — keep it that way).
- **README samples are real output.** Human-facing CLI output is quoted verbatim in both
  READMEs; changing output text means updating `README.md` and `README.en.md` together
  (they are maintained line-for-line parallel, ko/en).
- `SKILL.md` frontmatter keeps a `version` field tracking the workspace version. It is not
  part of the official skill spec — it exists so an installed skill can be matched to its
  binaries — keep it when editing frontmatter, and bump it with `workspace.package.version`.
- Comments state rationale and constraints ("why", failure modes avoided) — never change
  history or narration of edits.

## Release

Pushing a tag `v*` runs `release.yml`: the test gate runs on all three shipped OSes, every
target builds natively on its own runner (release jobs are deliberately cache-free), the
archives bundle both binaries plus the skill, provenance is attested
(`gh attestation verify <archive> --repo junyeong-ai/hatel`), and the runner-bundled `gh`
publishes the release.
