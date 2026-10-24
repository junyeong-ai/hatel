//! `hatel mcp` — the machine-facing surface: the read paths (`report`, `kinds`, `doctor`,
//! `deliveries`) and the writes (`emit`, `emit_batch`) as typed MCP tools over stdio, so an agent
//! calls them in-protocol instead of shelling out and parsing stdout. Each tool returns exactly
//! the JSON its CLI counterpart prints. A failure the caller can act on — invalid arguments, an
//! unreadable configuration or store — is a tool result with `isError`, which the model reads and
//! can correct; a protocol error means the server itself failed.
//! stdout is the protocol channel; diagnostics go to stderr only.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};

use hatel_core::schema::{Inventory, build_query_registry, build_registry};
use hatel_core::{Config, Payload, report};

use crate::{EmitError, doctor, emit_record, kinds_value, machine_json, parse_query};

#[derive(Debug, Clone)]
pub struct HatelMcp;

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportParams {
    #[schemars(description = "Rolling window in days, e.g. \"30d\" (days only; default \"30d\")")]
    pub window: Option<String>,
    #[schemars(
        description = "Restrict to one project by canonical key or label; default is all projects"
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Restrict to one registered Kind; scopes the rollup to it and drops the cost section"
    )]
    pub kind: Option<String>,
    #[schemars(
        description = "Answer under this stored schema ID (see stored_contracts in kinds) instead of the Kind's current contract. Requires kind."
    )]
    pub contract: Option<String>,
    #[schemars(
        description = "Exact-match restrictions, each \"field=value\" (every one must match). Requires `kind`; the field must be in that Kind's allow-list. A redacted field is matched by its original value."
    )]
    pub filter: Option<Vec<String>>,
    #[schemars(
        description = "Items per list: each Kind's groups and the cost section's sessions and projects (0 = all; default 5). group_count, cost_session_count and cost_group_count give the totals before the cut."
    )]
    pub top: Option<usize>,
    #[schemars(
        description = "Group by this field instead of the Kind's declared group_key. Requires `kind`; the field must be in that Kind's allow-list."
    )]
    pub group_by: Option<String>,
    #[schemars(
        description = "Rank groups by this measure instead of the Kind's first declared one. Requires `kind`; the measure must be one that Kind declares."
    )]
    pub sort_by: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmitParams {
    #[schemars(description = "A registered Kind name (e.g. \"check\")")]
    pub kind: String,
    #[schemars(
        description = "The record's fields as a JSON object. Keys outside the Kind's allow-list are dropped and named in the receipt's warning."
    )]
    pub payload: serde_json::Map<String, serde_json::Value>,
    #[schemars(
        description = "Common record context: occurred_at (RFC 3339), source, run_id, correlation_id"
    )]
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoctorParams {
    #[schemars(
        description = "Check only local recording, schemas, storage and deliveries, without Claude Code wiring (default false)"
    )]
    pub local: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchParams {
    #[schemars(
        description = "Up to 4096 entries, each {kind, payload, context?} as in emit. Each entry is stored, rejected or failed on its own; the batch is not atomic."
    )]
    pub records: Vec<serde_json::Value>,
}

#[tool_router]
impl HatelMcp {
    #[tool(
        title = "Aggregate recorded telemetry",
        description = "Aggregate the telemetry ledger over a rolling window: per-Kind group counts and summed measures, plus the cost snapshot: sessions ranked by spend with their tokens_by_type (cache accounting), by_model (model mix) and by_agent (subagent budget) breakdowns, and their per-project rollup in cost_groups. Each Kind section aggregates only records whose `contracts[].resolution` is selected or compatible and counts the excluded ones. A non-null `unreadable_kinds` means the ledger holds Kinds no loaded schema declares, so this rollup covers less than was collected — report that rather than the totals alone. Same JSON as `hatel report --format json`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn report(
        &self,
        Parameters(p): Parameters<ReportParams>,
    ) -> Result<CallToolResult, McpError> {
        answer(move || report_tool(p)).await
    }

    #[tool(
        title = "List queryable Kinds",
        description = "List the queryable Kinds under `kinds` — each with its fields (the allow-list), group_key, measures, redact set, and the identity a count is of when one is declared — plus `stored_contracts` and `unreadable_kinds`: the Kinds the ledger holds that no loaded schema declares, so a partial registry is never mistaken for the whole one. Same JSON as `hatel kinds --json`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn kinds(&self) -> Result<CallToolResult, McpError> {
        answer(kinds_tool).await
    }

    #[tool(
        title = "Show delivery states",
        description = "Inspect durable local-record delivery states per destination, status and error reason. Native OTLP forwarding is best-effort and is not included. Same JSON as `hatel deliveries --json`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn deliveries(&self) -> Result<CallToolResult, McpError> {
        answer(|| crate::delivery_value().map(|value| success(machine_json(&value)))).await
    }

    #[tool(
        title = "Diagnose the wiring",
        description = "Verify the Claude Code ↔ collector wiring. `ok: false` means a hard requirement failed; each section's findings carry a status (ok / fail / warn / note) and the message names the gap and its fix. `snippet` is the managed-settings paste block. Same JSON as `hatel doctor --json`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn doctor(
        &self,
        Parameters(p): Parameters<DoctorParams>,
    ) -> Result<CallToolResult, McpError> {
        answer(move || {
            let report = doctor::report(p.local.unwrap_or(false));
            let mut result = success(machine_json(&report));
            result.is_error = Some(!report.ok);
            Ok(result)
        })
        .await
    }

    #[tool(
        title = "Record a batch of signals",
        description = "Record up to 4096 structured {kind, payload, context?} entries. Returns one ordered result per entry: stored, rejected or failed; a stored receipt confirms local storage only. `isError` is set only when the whole call failed, so resend just the entries that were not stored — resending the batch would duplicate the stored ones.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn emit_batch(
        &self,
        Parameters(p): Parameters<BatchParams>,
    ) -> Result<CallToolResult, McpError> {
        answer(move || crate::emit_batch(p.records).map(|results| success(machine_json(&results))))
            .await
    }

    #[tool(
        title = "Record one signal",
        description = "Record one domain signal for a registered Kind — the programmatic path for project metrics that aren't derived from a Claude Code hook (a gate decision, a check rollup, a deploy outcome). The payload is allow-list-filtered and redacted like any other record; `metadata.project` comes from the Git repository the server runs in. Returns the storage receipt.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn emit(
        &self,
        Parameters(p): Parameters<EmitParams>,
    ) -> Result<CallToolResult, McpError> {
        answer(move || emit_tool(p)).await
    }
}

/// Every tool answers from the ledger on disk or from the receiver over the wire — blocking work,
/// run off the runtime's workers so the stdio transport keeps serving while one call reads a large
/// ledger or waits on a probe. Its failure message becomes an `isError` result.
async fn answer(
    work: impl FnOnce() -> Result<CallToolResult, String> + Send + 'static,
) -> Result<CallToolResult, McpError> {
    let result = tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(result.unwrap_or_else(|message| CallToolResult::error(vec![ContentBlock::text(message)])))
}

fn report_tool(p: ReportParams) -> Result<CallToolResult, String> {
    let window = p.window.unwrap_or_else(|| "30d".to_string());
    let window_secs = report::parse_window(&window)
        .ok_or_else(|| format!("invalid window {window:?} (expected e.g. 30d — days only)"))?;
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let reg = build_query_registry(&cfg, p.kind.as_deref(), p.contract.as_deref())
        .map_err(|e| e.to_string())?;
    let filters = parse_query(
        &reg,
        &cfg,
        p.kind.as_deref(),
        p.group_by.as_deref(),
        p.sort_by.as_deref(),
        &p.filter.unwrap_or_default(),
    )?;
    let q = report::Query {
        since: hatel_core::now_epoch().saturating_sub(window_secs),
        top_n: p.top.unwrap_or(report::TOP_N),
        project: p.project.as_deref(),
        kind: p.kind.as_deref(),
        contract: p.contract.as_deref(),
        group_by: p.group_by.as_deref(),
        sort_by: p.sort_by.as_deref(),
        filters: &filters,
    };
    let report = report::Report::build(&reg, &cfg, &window, &q);
    let mut result = success(machine_json(&report));
    result.is_error = Some(report.failed());
    Ok(result)
}

fn kinds_tool() -> Result<CallToolResult, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let reg = build_registry(&cfg).map_err(|e| e.to_string())?;
    let inspection = Inventory::inspect(&reg, &cfg);
    let mut result = success(machine_json(&kinds_value(&reg, &inspection)));
    result.is_error = Some(inspection.storage_error.is_some());
    Ok(result)
}

fn emit_tool(p: EmitParams) -> Result<CallToolResult, String> {
    let payload: Payload = p.payload.into_iter().collect();
    let context = p
        .context
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("invalid context: {e}"))?
        .unwrap_or_default();
    match emit_record(&p.kind, || Ok(payload), context) {
        Ok(receipt) => Ok(success(machine_json(&receipt))),
        Err(EmitError::Registry(e) | EmitError::Rejected(e)) => Err(e),
    }
}

// `call_tool`/`list_tools` come from the router; `get_info` is written out so the
// server identifies as hatel (not the SDK) and carries usage instructions a client
// can show its agent.
#[tool_handler]
impl ServerHandler for HatelMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default()
            .with_server_info(Implementation::new("hatel", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Local telemetry records and Claude Code usage. Ask `doctor` first when data \
                 looks missing (`ok: false` means the wiring is incomplete; `local: true` checks \
                 recording and storage only). `kinds` lists the queryable record Kinds, their \
                 fields and stored contracts; `report` aggregates them, cuts each list at `top` \
                 while stating its total, and adds the Claude Code cost snapshot unless a Kind is \
                 given; `deliveries` shows downstream delivery states. `emit` and `emit_batch` \
                 record domain signals for registered Kinds and return storage receipts. A \
                 failed call returns an error result whose text names the problem.",
            );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn success(body: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

/// Serve MCP over stdio until the client disconnects (a clean disconnect is a normal
/// exit). Runs on the same multi-thread runtime shape as `serve`.
pub fn run() -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mcp: failed to build runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(async {
        match HatelMcp.serve(stdio()).await {
            Ok(service) => {
                if let Err(e) = service.waiting().await {
                    eprintln!("mcp: {e}");
                    return 1;
                }
                0
            }
            Err(e) => {
                eprintln!("mcp: {e}");
                1
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_arguments_are_tool_errors_the_model_can_read() {
        let result = answer(|| {
            report_tool(ReportParams {
                window: Some("7 days".into()),
                ..Default::default()
            })
        })
        .await
        .expect("a tool failure is a result, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .is_some_and(|text| text.text.contains("invalid window"))
        );
    }
}
