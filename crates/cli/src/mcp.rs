//! `hatel mcp` — the machine-facing surface: the collector's read paths (`report`,
//! `kinds`, `doctor`) and its one write (`emit`) as typed MCP tools over stdio, so an
//! agent calls them in-protocol instead of shelling out and parsing stdout. Each read
//! tool returns exactly the JSON its CLI counterpart prints (`--format json` /
//! `--json`). Writes return storage receipts, including per-item batch outcomes.
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

use crate::{EmitError, doctor, emit_record, kinds_value, parse_query, report_json};

#[derive(Debug, Clone)]
pub struct HatelMcp;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
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
        description = "Answer under this stored schema ID instead of the current contract, or \"legacy\" for an explicit unverified interpretation of records without a known definition. Requires kind."
    )]
    pub contract: Option<String>,
    #[schemars(
        description = "Exact-match restrictions, each \"field=value\" (every one must match). Requires `kind`; the field must be in that Kind's allow-list. A redacted field is matched by its original value."
    )]
    pub filter: Option<Vec<String>>,
    #[schemars(description = "Groups shown per Kind (0 = all; default 5)")]
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
pub struct EmitParams {
    #[schemars(description = "A registered Kind name (e.g. \"ci_check\")")]
    pub kind: String,
    #[schemars(
        description = "The record's fields as a JSON object. Keys outside the Kind's allow-list are dropped and warned about in the result text."
    )]
    pub payload: serde_json::Map<String, serde_json::Value>,
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DoctorParams {
    pub local: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchParams {
    pub records: Vec<serde_json::Value>,
}

#[tool_router]
impl HatelMcp {
    #[tool(
        description = "Aggregate the telemetry ledger over a rolling window: per-Kind group counts and summed measures, plus the per-session cost snapshot with its tokens_by_type (cache accounting), by_model (model mix), and by_agent (subagent budget) breakdowns. Each Kind section aggregates only records whose `contracts[].resolution` is selected or compatible and counts the excluded ones. A non-null `unreadable_kinds` means the ledger holds Kinds no loaded schema declares, so this rollup covers less than was collected — report that rather than the totals alone. Same JSON as `hatel report --format json`."
    )]
    async fn report(
        &self,
        Parameters(p): Parameters<ReportParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(move || report_tool(p)).await?
    }

    #[tool(
        description = "List the queryable Kinds under `kinds` — each with its fields (the allow-list), group_key, measures, redact set, and the identity a count is of when one is declared — plus `unreadable_kinds`: the Kinds the ledger holds that no loaded schema declares, so a partial registry is never mistaken for the whole one. Same JSON as `hatel kinds --json`."
    )]
    async fn kinds(&self) -> Result<CallToolResult, McpError> {
        blocking(kinds_tool).await?
    }

    #[tool(
        description = "Inspect durable local-record delivery states per destination. Same JSON as hatel deliveries --json. Native OTLP forwarding is best-effort and is not included."
    )]
    async fn deliveries(&self) -> Result<CallToolResult, McpError> {
        blocking(|| {
            crate::delivery_value()
                .map(|value| {
                    text_result(format!(
                        "{}\n",
                        serde_json::to_string_pretty(&value).expect("states serialize")
                    ))
                })
                .map_err(internal)
        })
        .await?
    }

    #[tool(
        description = "Verify the Claude Code ↔ collector wiring. `ok: false` means a hard requirement failed; each section's findings carry a status (ok / fail / warn / note) and the message names the gap and its fix. `snippet` is the managed-settings paste block. Same JSON as `hatel doctor --json`."
    )]
    async fn doctor(
        &self,
        Parameters(p): Parameters<DoctorParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(move || {
            let json = serde_json::to_string_pretty(&if p.local.unwrap_or(false) {
                doctor::local_report_value()
            } else {
                doctor::report_value()
            })
            .unwrap_or_default();
            text_result(format!("{json}\n"))
        })
        .await
    }

    #[tool(
        description = "Record up to 4096 structured {kind, payload, context?} entries. Each result is stored, rejected, or failed; a stored receipt confirms local storage only."
    )]
    async fn emit_batch(
        &self,
        Parameters(p): Parameters<BatchParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(move || {
            let results = crate::emit_batch(p.records).map_err(internal)?;
            Ok(text_result(
                serde_json::to_string(&results).expect("batch serializes"),
            ))
        })
        .await?
    }

    #[tool(
        description = "Record one domain signal for a registered Kind — the programmatic path for project metrics that aren't derived from a Claude Code hook (a gate decision, a check rollup, a deploy outcome). The payload is allow-list-filtered and redacted like any other record; include attribution (e.g. `project`) as fields."
    )]
    async fn emit(
        &self,
        Parameters(p): Parameters<EmitParams>,
    ) -> Result<CallToolResult, McpError> {
        blocking(move || emit_tool(p)).await?
    }
}

/// Every tool answers from the ledger on disk or from the receiver over the wire — blocking work,
/// run off the runtime's workers so the stdio transport keeps serving while one call reads a large
/// ledger or waits on a probe.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, McpError> {
    tokio::task::spawn_blocking(work).await.map_err(internal)
}

fn report_tool(p: ReportParams) -> Result<CallToolResult, McpError> {
    let window = p.window.unwrap_or_else(|| "30d".to_string());
    let cfg = Config::load().map_err(internal)?;
    let reg =
        build_query_registry(&cfg, p.kind.as_deref(), p.contract.as_deref()).map_err(internal)?;
    let filters = parse_query(
        &reg,
        &cfg,
        p.kind.as_deref(),
        p.group_by.as_deref(),
        p.sort_by.as_deref(),
        &p.filter.unwrap_or_default(),
    )
    .map_err(|e| McpError::invalid_params(e, None))?;
    let Some(window_secs) = report::parse_window(&window) else {
        return Err(McpError::invalid_params(
            format!("invalid window {window:?} (expected e.g. 30d — days only)"),
            None,
        ));
    };
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
    let mut result = text_result(report_json(&report));
    result.is_error = Some(report.failed());
    Ok(result)
}

fn kinds_tool() -> Result<CallToolResult, McpError> {
    let cfg = Config::load().map_err(internal)?;
    let reg = build_registry(&cfg).map_err(internal)?;
    let inspection = Inventory::inspect(&reg, &cfg);
    let json = serde_json::to_string_pretty(&kinds_value(&reg, &inspection))
        .expect("inventory serializes");
    // Trailing newline included, so the payload is byte-equal to the CLI's println!.
    let mut result = text_result(format!("{json}\n"));
    result.is_error = Some(inspection.storage_error.is_some());
    Ok(result)
}

fn emit_tool(p: EmitParams) -> Result<CallToolResult, McpError> {
    let payload: Payload = p.payload.into_iter().collect();
    let context = p
        .context
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| McpError::invalid_params(format!("invalid context: {e}"), None))?
        .unwrap_or_default();
    match emit_record(&p.kind, || Ok(payload), context) {
        Ok(receipt) => Ok(text_result(
            serde_json::to_string(&receipt).expect("receipt serializes"),
        )),
        Err(EmitError::Registry(e)) => Err(internal(e)),
        Err(EmitError::Rejected(e)) => Err(McpError::invalid_params(e, None)),
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
                "Local Claude Code telemetry. Ask `doctor` first when data looks missing \
                 (`ok: false` means the wiring is incomplete). `kinds` lists the queryable \
                 record Kinds and their fields; `report` aggregates them and returns the \
                 per-session cost snapshot with tokens_by_type / by_model / by_agent \
                 breakdowns; `emit` records a custom domain signal for a registered Kind.",
            );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

fn text_result(body: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
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
