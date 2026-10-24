//! `hatel` — the collector binary: OTLP receiver, reports, doctor, registry
//! introspection, and the MCP server (the same surfaces, served as typed tools).

mod claude;
mod claude_settings;
mod doctor;
mod export;
mod init;
mod mcp;
mod receiver;
mod serve;
mod service;
mod throttle;

use clap::{Parser, Subcommand, ValueEnum};

use hatel_core::schema::{Inventory, UnreadableKinds, build_query_registry, build_registry};
use hatel_core::{Config, Payload, Registry, render, report};

#[derive(Parser)]
#[command(
    name = "hatel",
    version,
    about = "Local telemetry records, reports and Claude Code observability"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the OTLP/HTTP receiver and the live per-session rollup.
    Serve {
        #[arg(long, default_value_t = 4318)]
        port: u16,
        /// Show only this project's sessions (defaults to the receiver's cwd).
        #[arg(long, conflicts_with = "all")]
        project: Option<String>,
        /// Show every project's sessions.
        #[arg(long)]
        all: bool,
        /// Wait for the state dir's lock and the port while another process holds them, rather
        /// than exit (how the service runs the receiver).
        #[arg(long)]
        wait: bool,
    },
    /// Aggregate the ledger over a rolling window.
    Report {
        /// Rolling window in days, e.g. `30d` (days only).
        #[arg(long, default_value = "30d")]
        window: String,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        /// Restrict to one project by canonical key or label; default is all projects.
        #[arg(long)]
        project: Option<String>,
        /// Restrict to one registered Kind (e.g. `check`); default is every Kind.
        /// Scopes the rollup to that Kind and drops the native cost section.
        #[arg(long)]
        kind: Option<String>,
        /// Answer under this stored schema ID instead of the Kind's current contract.
        #[arg(long, requires = "kind")]
        contract: Option<String>,
        /// Items per list: each Kind's groups and the cost section's sessions and projects (0 = all).
        #[arg(long, default_value_t = report::TOP_N)]
        top: usize,
        /// Group by this field instead of the Kind's declared `group_key` (`--group-by outcome`).
        /// Requires `--kind`, and the field must be in that Kind's allow-list — the schema says
        /// what a Kind records, this says which of it the question is about.
        #[arg(long, value_name = "field", requires = "kind")]
        group_by: Option<String>,
        /// Rank groups by this measure instead of the Kind's first declared one
        /// (`--sort-by violations`). Requires `--kind`, and the measure must be one the Kind
        /// declares.
        #[arg(long, value_name = "measure", requires = "kind")]
        sort_by: Option<String>,
        /// Restrict to records whose field equals a value (`--filter spec=auth`; repeatable —
        /// every filter must match). Values compare against the same rendering the group-key
        /// column shows; a redacted field is matched by its original value (the query is
        /// hashed exactly as the ledger stored it). Requires `--kind`, and the field must be
        /// in that Kind's allow-list (a field outside it never reaches the ledger, so it
        /// could only match nothing).
        #[arg(long, value_name = "field=value", requires = "kind")]
        filter: Vec<String>,
    },
    /// Wire this collector into Claude Code's settings.json — idempotent and
    /// non-destructive (adds the telemetry env and lifecycle hooks without touching
    /// existing hooks or a repointed endpoint), so it is safe to re-run.
    Init {
        /// Which settings file to write: `user` (all projects), `project` (committed),
        /// or `local` (this repo, per-dev). Org/managed settings are print-only.
        #[arg(long, value_enum, default_value_t = init::Scope::User)]
        scope: init::Scope,
        /// Print the settings block instead of writing it (for managed/org settings).
        #[arg(long, conflicts_with = "remove")]
        print: bool,
        /// Remove this collector's wiring from the scope (leaves the native telemetry env).
        #[arg(long)]
        remove: bool,
        /// Insert hatel in front of an existing corporate OTLP endpoint: capture it as a
        /// `config.toml` export target and repoint Claude Code at the local receiver — so you keep
        /// the corporate collector and gain hatel. Operates on whichever scope sets the endpoint.
        #[arg(long, conflicts_with_all = ["remove", "print"])]
        insert: bool,
        /// The transform for the captured endpoint when `--insert`ing (default: enriched, so the
        /// corporate collector gains the project label too).
        #[arg(long, value_enum, default_value_t = init::InsertMode::Enriched, requires = "insert")]
        mode: init::InsertMode,
    },
    /// Install, restart or remove the receiver as a background user service (launchd on macOS,
    /// systemd --user on Linux) for gap-free collection — the unit runs `serve --all --wait`.
    Service {
        /// Remove the service instead of installing it.
        #[arg(long, conflicts_with_all = ["print", "restart"])]
        remove: bool,
        /// Print the unit file instead of installing it.
        #[arg(long, conflicts_with = "restart")]
        print: bool,
        /// Restart the installed service so it runs the binary now on disk, first rewriting a unit
        /// an earlier release wrote; a service that is missing or stopped is not started.
        #[arg(long)]
        restart: bool,
    },
    /// Verify the settings.json wiring and report policy gaps.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Inspect local recording, schemas and deliveries without Claude Code wiring checks.
        #[arg(long)]
        local: bool,
    },
    /// Serve `report` / `kinds` / `doctor` / `emit` as typed MCP tools over stdio —
    /// the programmatic surface for agents (`claude mcp add hatel -- hatel mcp`).
    /// The read tools return exactly the JSON their CLI counterparts print.
    Mcp,
    /// List the registered Kinds (core + plugins).
    Kinds {
        #[arg(long)]
        json: bool,
    },
    /// Record one domain signal for a registered Kind — the programmatic path for
    /// project metrics that aren't derived from a Claude Code hook (a gate decision,
    /// a check rollup, a deploy outcome). The payload is allow-list-filtered and
    /// redacted like any other record.
    ///
    /// Give fields as `key=value` (string) or `key:=value` (JSON — for numbers,
    /// bools, arrays), or pass a whole JSON object via `--json` or on stdin.
    Emit {
        /// A registered Kind name (e.g. `check`).
        kind: String,
        /// `key=value` (string) or `key:=value` (JSON) field pairs.
        #[arg(value_name = "key=value")]
        fields: Vec<String>,
        /// A full JSON object payload (instead of field pairs or stdin).
        #[arg(long, conflicts_with = "fields")]
        json: Option<String>,
        /// Print a JSON storage receipt.
        #[arg(long)]
        receipt: bool,
        /// Common record context as JSON (occurred_at, source, run_id, correlation_id).
        #[arg(long)]
        context: Option<String>,
    },
    /// Record a JSON array of {kind, payload, context?} from stdin; returns one result per item.
    EmitBatch,
    /// Show durable local-record delivery states.
    Deliveries {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Md,
    Text,
    Json,
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let command = Cli::parse().command;
    if !matches!(command, Command::Serve { .. } | Command::Mcp) {
        end_quietly_on_closed_stdout();
    }
    match command {
        Command::Serve {
            port,
            project,
            all,
            wait,
        } => serve::run(port, project, all, wait),
        Command::Report {
            window,
            format,
            project,
            kind,
            contract,
            top,
            group_by,
            sort_by,
            filter,
        } => report_cmd(
            &window,
            format,
            top,
            project.as_deref(),
            kind.as_deref(),
            contract.as_deref(),
            group_by.as_deref(),
            sort_by.as_deref(),
            &filter,
        ),
        Command::Init {
            scope,
            print,
            remove,
            insert,
            mode,
        } => {
            if insert {
                init::insert(mode)
            } else {
                init::run(scope, print, remove)
            }
        }
        Command::Service {
            remove,
            print,
            restart,
        } => service::run(if remove {
            service::Action::Remove
        } else if print {
            service::Action::Print
        } else if restart {
            service::Action::Restart
        } else {
            service::Action::Install
        }),
        Command::Doctor { json, local } => doctor::run(json, local),
        Command::Mcp => mcp::run(),
        Command::Kinds { json } => kinds_cmd(json),
        Command::Emit {
            kind,
            fields,
            json,
            receipt,
            context,
        } => emit_cmd(&kind, fields, json, receipt, context),
        Command::EmitBatch => emit_batch_cmd(),
        Command::Deliveries { json } => deliveries_cmd(json),
    }
}

/// A one-shot command is usually read through a pipe, and a reader that stops early (`| head`)
/// must end it the way it ends any Unix tool, not turn the next write into a panic. Rust ignores
/// SIGPIPE by default; the receiver and the MCP server keep that, so a client that disconnects is
/// an error they handle rather than a signal that kills them.
#[cfg(unix)]
fn end_quietly_on_closed_stdout() {
    // SAFETY: restores the default disposition of one signal; no Rust invariant depends on it.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn end_quietly_on_closed_stdout() {}

/// Why an emit was refused: `Registry` is an environment problem (the registry itself
/// failed to load); `Rejected` is a caller problem (unknown Kind,
/// malformed input, or — in strict mode — disallowed keys). The CLI maps them to exit
/// codes 1 / 2, MCP to internal / invalid-params errors.
pub(crate) enum EmitError {
    Registry(String),
    Rejected(String),
}

pub(crate) fn emit_record(
    kind: &str,
    payload: impl FnOnce() -> Result<Payload, String>,
    context: hatel_core::model::RecordContext,
) -> Result<hatel_core::record::Receipt, EmitError> {
    let cfg = Config::load().map_err(|e| EmitError::Registry(e.to_string()))?;
    let reg = build_registry(&cfg).map_err(|e| EmitError::Registry(e.to_string()))?;
    if reg.kind(kind).is_none() {
        return Err(EmitError::Rejected(unknown_kind(kind, &reg, &cfg)));
    }
    let input = hatel_core::record::RecordInput {
        kind: kind.into(),
        payload: payload().map_err(EmitError::Rejected)?,
        context,
    };
    match hatel_core::record::Writer::new(&cfg, &reg).write(input) {
        hatel_core::record::RecordResult::Stored { receipt } => Ok(receipt),
        hatel_core::record::RecordResult::Rejected { error } => Err(EmitError::Rejected(error)),
        hatel_core::record::RecordResult::Failed { error } => Err(EmitError::Registry(error)),
    }
}

fn emit_cmd(
    kind: &str,
    fields: Vec<String>,
    json: Option<String>,
    receipt: bool,
    context: Option<String>,
) -> i32 {
    let context = match context.map(|s| serde_json::from_str(&s)).transpose() {
        Ok(c) => c.unwrap_or_default(),
        Err(e) => {
            eprintln!("emit: invalid context: {e}");
            return 2;
        }
    };
    match emit_record(kind, || build_payload(fields, json), context) {
        Ok(result) => {
            if let Some(w) = &result.warning {
                eprintln!("emit: {w}");
            }
            if result.delivery.not_queued > 0 {
                eprintln!(
                    "emit: stored locally; {} delivery queue(s) at capacity",
                    result.delivery.not_queued
                );
            }
            if receipt {
                print!("{}", machine_json(&result));
            }
            0
        }
        Err(EmitError::Registry(e)) => {
            eprintln!("emit: {e}");
            1
        }
        Err(EmitError::Rejected(e)) => {
            eprintln!("emit: {e}");
            2
        }
    }
}

pub(crate) fn emit_batch(
    items: Vec<serde_json::Value>,
) -> Result<Vec<hatel_core::record::RecordResult>, String> {
    if items.len() > 4096 {
        return Err("batch exceeds 4096 records".into());
    }
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let reg = build_registry(&cfg).map_err(|e| e.to_string())?;
    let mut writer = hatel_core::record::Writer::new(&cfg, &reg);
    Ok(items
        .into_iter()
        .map(|item| match serde_json::from_value(item) {
            Ok(input) => writer.write(input),
            Err(e) => hatel_core::record::RecordResult::Rejected {
                error: e.to_string(),
            },
        })
        .collect())
}

const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

fn read_input(reader: impl std::io::Read) -> Result<String, String> {
    use std::io::Read as _;
    let mut input = String::new();
    reader
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())?;
    if input.len() > MAX_INPUT_BYTES {
        return Err("input exceeds 8 MiB".into());
    }
    Ok(input)
}

fn emit_batch_cmd() -> i32 {
    let input = match read_input(std::io::stdin()) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("emit-batch: {error}");
            return 2;
        }
    };
    let result = serde_json::from_str::<Vec<serde_json::Value>>(&input)
        .map_err(|e| e.to_string())
        .and_then(emit_batch);
    match result {
        Ok(results) => {
            let failed = results
                .iter()
                .any(|r| !matches!(r, hatel_core::record::RecordResult::Stored { .. }));
            print!("{}", machine_json(&results));
            if failed { 1 } else { 0 }
        }
        Err(e) => {
            eprintln!("emit-batch: {e}");
            2
        }
    }
}

pub(crate) fn delivery_value() -> Result<serde_json::Value, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    hatel_core::outbox::states(&cfg.database())
        .map_err(|e| e.to_string())
        .and_then(|states| serde_json::to_value(states).map_err(|e| e.to_string()))
}

fn deliveries_cmd(json: bool) -> i32 {
    match delivery_value() {
        Ok(value) => {
            if json {
                print!("{}", machine_json(&value));
            } else if let Some(states) = value.as_array() {
                for state in states {
                    println!(
                        "{} — {}: {} records, {} bytes",
                        state["destination"].as_str().unwrap_or(""),
                        state["status"].as_str().unwrap_or(""),
                        state["records"],
                        state["bytes"]
                    );
                }
            }
            0
        }
        Err(error) => {
            eprintln!("deliveries: {error}");
            1
        }
    }
}

/// Build the payload from, in precedence: an explicit `--json` object, else
/// `key=value` / `key:=json` field pairs, else a JSON object on stdin.
fn build_payload(fields: Vec<String>, json: Option<String>) -> Result<Payload, String> {
    if let Some(j) = json {
        return parse_json_object(&j);
    }
    if !fields.is_empty() {
        return parse_pairs(&fields);
    }
    read_input(std::io::stdin()).and_then(|input| parse_json_object(&input))
}

fn parse_json_object(s: &str) -> Result<Payload, String> {
    let value: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "payload must be a JSON object".to_string())?;
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `key=value` → a string; `key:=value` → `value` parsed as JSON (numbers, bools,
/// arrays). The `:=` is explicit, so types are never guessed from the string.
fn parse_pairs(fields: &[String]) -> Result<Payload, String> {
    let mut payload = Payload::new();
    for f in fields {
        let eq = f
            .find('=')
            .ok_or_else(|| format!("field {f:?} must be key=value or key:=json"))?;
        let typed = eq > 0 && f.as_bytes()[eq - 1] == b':';
        let key = f[..if typed { eq - 1 } else { eq }].to_string();
        if key.is_empty() {
            return Err(format!("field {f:?} has an empty key"));
        }
        let raw = &f[eq + 1..];
        let value = if typed {
            serde_json::from_str(raw)
                .map_err(|e| format!("field {key:?}: invalid JSON value {raw:?}: {e}"))?
        } else {
            serde_json::Value::String(raw.to_string())
        };
        payload.insert(key, value);
    }
    Ok(payload)
}

#[allow(clippy::too_many_arguments)]
fn report_cmd(
    window: &str,
    format: Format,
    top: usize,
    project: Option<&str>,
    kind: Option<&str>,
    contract: Option<&str>,
    group_by: Option<&str>,
    sort_by: Option<&str>,
    filter: &[String],
) -> i32 {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("report: {e}");
            return 1;
        }
    };
    let reg = match build_query_registry(&cfg, kind, contract) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("report: {e}");
            return 1;
        }
    };
    let filters = match parse_query(&reg, &cfg, kind, group_by, sort_by, filter) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("report: {e}");
            return 2;
        }
    };
    let Some(window_secs) = report::parse_window(window) else {
        eprintln!("report: invalid window {window:?} (expected e.g. 30d — days only)");
        return 2;
    };
    // One query for the whole report — every format and the cost section share its single
    // `since`, so they can't disagree on a record in the last second.
    let q = report::Query {
        since: hatel_core::now_epoch().saturating_sub(window_secs),
        top_n: top,
        project,
        kind,
        contract,
        group_by,
        sort_by,
        filters: &filters,
    };
    let report = report::Report::build(&reg, &cfg, window, &q);
    let failed = report.failed();
    match format {
        Format::Json => print!("{}", machine_json(&report)),
        Format::Md => print!("{}", render::format_markdown(&report)),
        Format::Text => print!("{}", render::format_text(&report)),
    }
    i32::from(failed)
}

/// Why a Kind name did not resolve. A name the store already holds records for is not a typo but
/// an unloaded schema, so it is answered with the surface that would load it — the registered list
/// alone would send someone reading this to `kinds`, which knows no more than the list does.
fn unknown_kind(kind: &str, reg: &Registry, cfg: &Config) -> String {
    let known: Vec<&str> = reg.kinds().map(|s| s.name.as_str()).collect();
    let inspection = UnreadableKinds::detect(reg, cfg);
    let stored = inspection
        .as_ref()
        .ok()
        .and_then(|found| found.as_ref())
        .filter(|u| u.names.iter().any(|n| n == kind))
        .map(|u| {
            format!(
                " — it has records in the ledger, but no loaded schema declares it; list its \
                 plugin in {}",
                u.plugin_source
            )
        })
        .unwrap_or_default();
    let error = inspection
        .err()
        .map(|e| format!("; storage inspection failed: {e}"))
        .unwrap_or_default();
    format!(
        "unknown kind {kind:?} (registered: {}){stored}{error}",
        known.join(", ")
    )
}

/// Validate every Kind-scoped part of a query against the registry and return the parsed
/// filters. Each restriction names a field the Kind declares; one that does not could only ever
/// match nothing, so it is rejected here rather than answered with a convincing empty result.
pub(crate) fn parse_query(
    reg: &Registry,
    cfg: &Config,
    kind: Option<&str>,
    group_by: Option<&str>,
    sort_by: Option<&str>,
    filter: &[String],
) -> Result<Vec<(String, String)>, String> {
    if let Some(k) = kind
        && reg.kind(k).is_none()
    {
        return Err(unknown_kind(k, reg, cfg));
    }
    if let Some(field) = group_by {
        let spec = kind_of(reg, kind, "--group-by")?;
        if !spec.query_field(field) {
            let accepted: Vec<&str> = spec.fields.iter().map(String::as_str).collect();
            return Err(format!(
                "{} has no field {field:?} to group by — accepted fields: {}",
                spec.name,
                accepted.join(", ")
            ));
        }
    }
    if let Some(measure) = sort_by {
        let spec = kind_of(reg, kind, "--sort-by")?;
        if !spec.measures.iter().any(|m| m == measure) {
            return Err(if spec.measures.is_empty() {
                format!(
                    "{} declares no measures, so its groups rank by record count — \
                     --sort-by has nothing to name",
                    spec.name
                )
            } else {
                format!(
                    "{} has no measure {measure:?} to rank by — declared measures: {}",
                    spec.name,
                    spec.measures.join(", ")
                )
            });
        }
    }
    parse_filters(filter, kind, reg)
}

fn kind_of<'a>(
    reg: &'a Registry,
    kind: Option<&str>,
    flag: &str,
) -> Result<&'a hatel_core::KindSpec, String> {
    let kind = kind.ok_or_else(|| format!("{flag} requires --kind"))?;
    reg.kind(kind)
        .ok_or_else(|| format!("unknown kind {kind:?}"))
}

/// Parse `--filter field=value` pairs and validate each field against the Kind's allow-list —
/// loud, because a field outside the allow-list never reaches the ledger, so the filter could
/// only ever produce an empty (and silently misleading) report. Clap enforces
/// `requires = "kind"`; the check here is the load-bearing backstop.
///
/// A redacted field's value is mapped to its stored hash here, exactly as the write path maps
/// it — so the field is queried by its original value (which still never touches the ledger),
/// rather than silently matching nothing against the stored hash.
pub(crate) fn parse_filters(
    raw: &[String],
    kind: Option<&str>,
    reg: &Registry,
) -> Result<Vec<(String, String)>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let kind = kind.ok_or("--filter requires --kind")?;
    let spec = reg
        .kind(kind)
        .ok_or_else(|| format!("unknown kind {kind:?}"))?;
    let mut filters = Vec::with_capacity(raw.len());
    for f in raw {
        let Some((field, value)) = f.split_once('=') else {
            return Err(format!("filter {f:?} must be field=value"));
        };
        if field.is_empty() {
            return Err(format!("filter {f:?} has an empty field"));
        }
        if !spec.query_field(field) {
            let accepted: Vec<&str> = spec.fields.iter().map(String::as_str).collect();
            return Err(format!(
                "{kind} has no field {field:?} to filter on — accepted fields: {}",
                accepted.join(", ")
            ));
        }
        let value = if field != "project" && spec.redact.contains(field) {
            hatel_core::pii::redacted(value)
        } else {
            value.to_string()
        };
        filters.push((field.to_string(), value));
    }
    Ok(filters)
}

/// Every machine output, printed by the CLI and returned by the MCP tools alike: one line of
/// JSON routed through `serde_json::Value`, whose object is a sorted map, so every key at every
/// depth serializes alphabetically without a struct having to declare its fields in that order.
/// Compact, because its readers are parsers and agents, and indentation roughly doubles the
/// tokens an agent spends on the same answer.
pub(crate) fn machine_json(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .and_then(|v| serde_json::to_string(&v))
        .map(|s| s + "\n")
        .expect("machine output serializes")
}

/// The `kinds --json` payload, shared by the CLI flag and the MCP tool: what this registry can
/// query, and — as `null` when there is none — the gap between that and what the store holds. The
/// second half is what keeps a partial registry from answering as a complete one.
pub(crate) fn kinds_value(reg: &Registry, inspection: &Inventory) -> serde_json::Value {
    let kinds: Vec<serde_json::Value> = reg
        .kinds()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "mode": s.mode,
                "fields": s.fields,
                "field_specs": s.field_specs,
                "version": s.version,
                "schema_id": s.schema_id,
                "metadata_fields": hatel_core::registry::METADATA_FIELDS,
                "group_key": s.group_key,
                "redact": s.redact,
                "measures": s.measures,
                "identity": s.identity,
            })
        })
        .collect();
    serde_json::json!({ "kinds": kinds,
        "unreadable_kinds": inspection.unreadable_kinds,
        "stored_contracts": inspection.stored_contracts,
        "storage_error": inspection.storage_error })
}

fn kinds_cmd(json: bool) -> i32 {
    let (cfg, reg) = match Config::load().and_then(|cfg| {
        let reg = build_registry(&cfg)?;
        Ok((cfg, reg))
    }) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("kinds: {e}");
            return 1;
        }
    };
    let inspection = Inventory::inspect(&reg, &cfg);
    if json {
        print!("{}", machine_json(&kinds_value(&reg, &inspection)));
    } else {
        for s in reg.kinds() {
            let fields: Vec<&str> = s.fields.iter().map(String::as_str).collect();
            // A Kind counting entities rather than records says so here, where someone asking
            // what they can query reads what a count will mean before they run one.
            let identity = s
                .identity
                .as_ref()
                .map(|i| format!(" identity={i}"))
                .unwrap_or_default();
            println!(
                "{:<14} group_key={:<12} fields=[{}]{}{}",
                s.name,
                s.group_key,
                fields.join(", "),
                identity,
                if s.mode == hatel_core::registry::KindMode::Snapshot {
                    " mode=snapshot"
                } else {
                    ""
                }
            );
        }
        for contract in &inspection.stored_contracts {
            println!(
                "contract {} kind={} version={}",
                contract.schema_id, contract.definition.name, contract.definition.version
            );
        }
        if let Some(unreadable) = &inspection.unreadable_kinds {
            println!("\n{unreadable}");
        }
        if let Some(error) = &inspection.storage_error {
            println!("\nstorage inspection failed: {error}");
        }
    }
    i32::from(inspection.storage_error.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_rows_honor_window_and_project() {
        let cfg = test_cfg(vec![]);
        let dir = &cfg.state_dir;
        let row = |sid: &str, proj: &str, ts: &str| {
            serde_json::to_string(&hatel_core::cost::CostRow {
                session_id: sid.into(),
                project: proj.into(),
                tokens: 1,
                ts: ts.into(),
                ..Default::default()
            })
            .unwrap()
        };
        let now = hatel_core::now_iso_utc();
        std::fs::write(
            dir.join("cost_snapshot.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                row("old", "alpha", "2000-01-01T00:00:00Z"),
                row("recent", "alpha", &now),
                row("other", "beta", &now),
            ),
        )
        .unwrap();
        let reg = build_registry(&cfg).unwrap();
        let cost = |project| {
            report::Report::build(
                &reg,
                &cfg,
                "1d",
                &report::Query {
                    since: hatel_core::now_epoch() - 86_400,
                    top_n: 0,
                    project,
                    kind: None,
                    contract: None,
                    group_by: None,
                    sort_by: None,
                    filters: &[],
                },
            )
            .cost
        };
        // window drops the year-2000 row
        assert_eq!(cost(None).len(), 2, "old row outside window dropped");
        // project further restricts
        let alpha = cost(Some("alpha"));
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].session_id, "recent");
    }

    #[test]
    fn input_limit_accepts_the_boundary_and_preserves_read_errors() {
        let mut input = vec![b' '; MAX_INPUT_BYTES - 2];
        input.extend_from_slice(b"{}");
        assert!(
            parse_json_object(&read_input(input.as_slice()).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(read_input([0xff].as_slice()).is_err());
    }

    #[test]
    fn pairs_split_string_and_json_by_separator() {
        let p = parse_pairs(&[
            "check=lint".into(),
            "runs:=14000".into(),
            "ok:=true".into(),
            "note=a:=b".into(),
        ])
        .unwrap();
        assert_eq!(p.get("check").unwrap(), &serde_json::json!("lint"));
        assert_eq!(p.get("runs").unwrap(), &serde_json::json!(14000));
        assert_eq!(p.get("ok").unwrap(), &serde_json::json!(true));
        // The first `=` wins, so this stays a plain string — never re-guessed.
        assert_eq!(p.get("note").unwrap(), &serde_json::json!("a:=b"));
    }

    #[test]
    fn pair_without_separator_or_key_is_an_error() {
        assert!(parse_pairs(&["lonely".into()]).is_err());
        assert!(parse_pairs(&["=novalue".into()]).is_err());
    }

    #[test]
    fn typed_pair_with_bad_json_errors() {
        assert!(parse_pairs(&["n:=not-json".into()]).is_err());
    }

    /// A config over a fresh store, which is removed when this is dropped: use the config through
    /// this value, not a clone of it.
    struct TestConfig {
        cfg: Config,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestConfig {
        type Target = Config;

        fn deref(&self) -> &Config {
            &self.cfg
        }
    }

    /// A record of a Kind no loaded schema declares, as one a removed plugin left behind.
    fn store_unregistered(cfg: &Config, reg: &Registry, kind: &str) {
        let env = hatel_core::Envelope {
            ts: hatel_core::now_iso_utc(),
            kind: kind.into(),
            schema_version: hatel_core::SCHEMA_VERSION,
            payload: Default::default(),
            metadata: Default::default(),
        };
        hatel_core::SqliteSink::open(cfg.database(), reg)
            .write_record(&env)
            .unwrap();
    }

    fn test_cfg(plugins: Vec<std::path::PathBuf>) -> TestConfig {
        let dir = tempfile::tempdir().unwrap();
        TestConfig {
            cfg: Config {
                state_dir: dir.path().to_path_buf(),
                plugins,
                plugin_source: hatel_core::config::PluginSource::ConfigFile,
                rotate_bytes: 10 * 1024 * 1024,
                retention_days: 90,
                disabled: false,
                strict: false,
                export: Ok(Default::default()),
            },
            _dir: dir,
        }
    }

    #[test]
    fn filter_flag_requires_kind() {
        use clap::Parser as _;
        assert!(
            Cli::try_parse_from(["hatel", "report", "--filter", "a=b"]).is_err(),
            "--filter without --kind must be rejected"
        );
        assert!(
            Cli::try_parse_from(["hatel", "report", "--kind", "tool", "--filter", "a=b"]).is_ok()
        );
    }

    #[test]
    fn parse_filters_validates_fields_against_the_allow_list() {
        let reg = build_registry(&test_cfg(vec![])).unwrap();
        assert_eq!(
            parse_filters(&["tool_name=Bash".into()], Some("tool"), &reg).unwrap(),
            vec![("tool_name".to_string(), "Bash".to_string())]
        );
        // A field outside the Kind's allow-list can never match a record — loud error.
        let err = parse_filters(&["nope=1".into()], Some("tool"), &reg).unwrap_err();
        assert!(err.contains("accepted fields"), "got: {err}");
        assert!(parse_filters(&["broken".into()], Some("tool"), &reg).is_err());
        assert!(parse_filters(&["=v".into()], Some("tool"), &reg).is_err());
    }

    #[test]
    fn parse_filters_maps_a_redacted_field_to_its_stored_form() {
        // `ci_check.actor` is redacted: the ledger stores its hash, never the raw identity. A
        // filter must therefore hash the query value the same way — matching by the original
        // value while the original still never touches disk. The raw value compared literally
        // would silently match nothing.
        let plugin =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/example.toml");
        let reg = build_registry(&test_cfg(vec![plugin])).unwrap();
        let parsed =
            parse_filters(&["actor=alice@example.com".into()], Some("ci_check"), &reg).unwrap();
        assert_eq!(parsed[0].0, "actor");
        assert_ne!(
            parsed[0].1, "alice@example.com",
            "the raw identity is never used for matching"
        );
        assert_eq!(parsed[0].1, hatel_core::pii::redacted("alice@example.com"));
    }

    #[test]
    fn report_kind_filter_scopes_to_one_kind() {
        let cfg = test_cfg(vec![]);
        let reg = build_registry(&cfg).unwrap();
        let q = |kind: Option<&'static str>| report::Query {
            since: 0,
            top_n: 0,
            project: None,
            kind,
            contract: None,
            group_by: None,
            sort_by: None,
            filters: &[],
        };
        // An empty ledger still lists one entry per Kind (with empty groups), so the
        // filter is observable without seeding records.
        let full: serde_json::Value = serde_json::from_str(&machine_json(&report::Report::build(
            &reg,
            &cfg,
            "7d",
            &q(None),
        )))
        .unwrap();
        let full_kinds: Vec<&str> = full["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kind"].as_str().unwrap())
            .collect();
        assert!(
            full_kinds.contains(&"tool"),
            "full report lists every Kind: {full_kinds:?}"
        );
        assert!(full_kinds.len() > 1);

        let scoped: serde_json::Value = serde_json::from_str(&machine_json(
            &report::Report::build(&reg, &cfg, "7d", &q(Some("tool"))),
        ))
        .unwrap();
        let scoped_kinds: Vec<&str> = scoped["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            scoped_kinds,
            vec!["tool"],
            "--kind scopes to exactly that Kind"
        );
        // the native cost section is not a Kind, so a scoped report drops it
        assert_eq!(scoped["cost"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn an_unknown_kind_the_store_holds_is_answered_with_the_surface_that_loads_it() {
        // A name the ledger already has records for is an unloaded schema, not a typo. The
        // registered list alone sends whoever reads the error to `kinds`, which knows no more
        // than that list does — the loop closes only where the schema would be listed.
        let cfg = test_cfg(vec![]);
        let reg = build_registry(&cfg).unwrap();
        store_unregistered(&cfg, &reg, "ci_check");

        let stored = unknown_kind("ci_check", &reg, &cfg);
        assert!(stored.contains("records in the ledger"), "got: {stored}");
        assert!(stored.contains(&cfg.plugin_source.label()), "got: {stored}");

        let typo = unknown_kind("toool", &reg, &cfg);
        assert!(
            !typo.contains("ledger"),
            "a name nothing stored is a typo, not an unloaded schema: {typo}"
        );
        assert!(typo.contains("registered: "), "got: {typo}");
    }

    #[test]
    fn kinds_json_reports_the_gap_between_the_registry_and_the_store() {
        // `kinds` answers "what can I query"; a registry missing a schema the store has records
        // for would answer it as if the partial list were the whole one.
        let cfg = test_cfg(vec![]);
        let reg = build_registry(&cfg).unwrap();
        store_unregistered(&cfg, &reg, "ci_check");

        let value = kinds_value(&reg, &Inventory::inspect(&reg, &cfg));
        let names: Vec<&str> = value["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"tool"), "got: {names:?}");
        assert_eq!(
            value["unreadable_kinds"]["names"],
            serde_json::json!(["ci_check"])
        );
        // One shape either way: no gap is `null`, never an absent key.
        assert!(
            Inventory::inspect(&reg, &test_cfg(vec![]))
                .unreadable_kinds
                .is_none()
        );
    }
}
