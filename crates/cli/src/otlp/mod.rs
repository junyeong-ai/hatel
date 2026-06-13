//! Native OpenTelemetry ingest: decode OTLP/JSON, fold into per-session totals.

pub mod accumulator;
pub mod decode;

pub use accumulator::{Accumulator, SessionTotals};
pub use decode::{ToolResult, parse_logs, parse_metrics};

/// The datapoint / log-record attribute carrying the Claude Code session id — the join key the
/// receiver attributes on. Read identically by the typed decode and the lossless export walker, so
/// it lives here rather than as a literal both must keep in lockstep.
pub const SESSION_ID: &str = "session.id";

/// The attribute the enriched export injects to label a datapoint with its project. Named once so
/// the inject side and the duplicate-guard read cannot disagree on its spelling.
pub const PROJECT: &str = "project";
