//! `hatel mcp` — the machine-facing surface: the collector's read paths (`report`,
//! `kinds`, `doctor`) and its one write (`emit`) as typed MCP tools over stdio, so an
//! agent calls them in-protocol instead of shelling out and parsing stdout. Every tool
//! returns exactly the JSON its CLI `--json` counterpart prints — one payload shape per
//! question, however it is asked. stdout is the protocol channel; diagnostics go to
//! stderr only.

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};

use hatel_core::schema::build_registry;
use hatel_core::{Config, Payload, report};

use crate::{EmitError, doctor, emit_record, kinds_value, parse_filters, report_json};

#[derive(Debug, Clone)]
pub struct HatelMcp;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportParams {
    #[schemars(description = "Rolling window in days, e.g. \"30d\" (days only; default \"30d\")")]
    pub window: Option<String>,
    #[schemars(
        description = "Restrict to one project (its basename label); default is all projects"
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Restrict to one registered Kind; scopes the rollup to it and drops the cost section"
    )]
    pub kind: Option<String>,
    #[schemars(
        description = "Exact-match restrictions, each \"field=value\" (every one must match). Requires `kind`; the field must be in that Kind's allow-list. A redacted field is matched by its original value."
    )]
    pub filter: Option<Vec<String>>,
    #[schemars(description = "Groups shown per Kind (0 = all; default 5)")]
    pub top: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EmitParams {
    #[schemars(description = "A registered Kind name (e.g. \"ci_check\")")]
    pub kind: String,
    #[schemars(
        description = "The record's fields as a JSON object. Keys outside the Kind's allow-list are dropped and warned about in the result text."
    )]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

#[tool_router]
impl HatelMcp {
    #[tool(
        description = "Aggregate the telemetry ledger over a rolling window: per-Kind group counts and summed measures, plus the per-session cost snapshot with its tokens_by_type (cache accounting), by_model (model mix), and by_agent (subagent budget) breakdowns. Same JSON as `hatel report --format json`."
    )]
    fn report(&self, Parameters(p): Parameters<ReportParams>) -> Result<CallToolResult, McpError> {
        let window = p.window.unwrap_or_else(|| "30d".to_string());
        let cfg = Config::load();
        let reg = build_registry(&cfg).map_err(internal)?;
        if let Some(k) = p.kind.as_deref()
            && reg.kind(k).is_none()
        {
            let known: Vec<&str> = reg.kinds().map(|s| s.name.as_str()).collect();
            return Err(McpError::invalid_params(
                format!("unknown kind {k:?}; registered: {}", known.join(", ")),
                None,
            ));
        }
        let filters = parse_filters(&p.filter.unwrap_or_default(), p.kind.as_deref(), &reg)
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
            filters: &filters,
        };
        Ok(text_result(report_json(&reg, &cfg, &window, &q)))
    }

    #[tool(
        description = "List every registered Kind (core + plugins) with its fields (the allow-list), group_key, measures, redact set, and whether it is receiver-sourced. Same JSON as `hatel kinds --json`."
    )]
    fn kinds(&self) -> Result<CallToolResult, McpError> {
        let cfg = Config::load();
        let reg = build_registry(&cfg).map_err(internal)?;
        let json = serde_json::to_string_pretty(&kinds_value(&reg)).unwrap_or_default();
        Ok(text_result(json))
    }

    #[tool(
        description = "Verify the Claude Code ↔ collector wiring. `ok: false` means a hard requirement failed; each section's findings carry a status (ok / fail / warn / note) and the message names the gap and its fix. `snippet` is the managed-settings paste block. Same JSON as `hatel doctor --json`."
    )]
    fn doctor(&self) -> Result<CallToolResult, McpError> {
        let json = serde_json::to_string_pretty(&doctor::report_value()).unwrap_or_default();
        Ok(text_result(json))
    }

    #[tool(
        description = "Record one domain signal for a registered Kind — the programmatic path for project metrics that aren't derived from a Claude Code hook (a gate decision, a check rollup, a deploy outcome). The payload is allow-list-filtered and redacted like any other record; include attribution (e.g. `project`) as fields."
    )]
    fn emit(&self, Parameters(p): Parameters<EmitParams>) -> Result<CallToolResult, McpError> {
        let payload: Payload = p.payload.into_iter().collect();
        match emit_record(&p.kind, || Ok(payload)) {
            Ok(None) => Ok(text_result(format!("recorded {}", p.kind))),
            Ok(Some(warning)) => Ok(text_result(format!(
                "recorded {} — warning: {warning}",
                p.kind
            ))),
            Err(EmitError::Registry(e)) => Err(internal(e)),
            Err(EmitError::Rejected(e)) => Err(McpError::invalid_params(e, None)),
        }
    }
}

// `call_tool`/`list_tools` come from the router; `get_info` is written out so the
// server identifies as hatel (not the SDK) and carries usage instructions a client
// can show its agent.
#[tool_handler]
impl ServerHandler for HatelMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default()
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
