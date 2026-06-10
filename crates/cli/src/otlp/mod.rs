//! Native OpenTelemetry ingest: decode OTLP/JSON, fold into per-session totals.

pub mod accumulator;
pub mod decode;

pub use accumulator::{Accumulator, SessionTotals};
pub use decode::{ToolResult, parse_events, parse_metrics, parse_tool_results};
