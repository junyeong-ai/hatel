//! Pure OTLP/JSON decode of the narrow subset we attribute. Tracking a focused set
//! of fields (rather than the full proto) keeps the decode controlled and tested:
//! proto3-JSON encodes int64 as a *string* and enums as a name *or* number, so each
//! ambiguous field is parsed defensively.

use std::collections::BTreeSet;

use serde::Deserialize;

/// One decoded numeric datapoint, attributed to a session.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricPoint {
    pub name: String,
    pub value: f64,
    pub session_id: String,
    /// All attributes except `session.id`, sorted — the series within a session.
    pub series: Vec<(String, String)>,
    /// True when the datapoint is a per-interval increment to accumulate; false
    /// when it is a running total (cumulative sum or gauge) that replaces.
    pub delta: bool,
}

/// Metric names are normalized by stripping the `claude_code.` prefix, exactly as
/// event names are — so a tracked entry matches whether the wire form is prefixed
/// or bare, and metrics never silently drop while events still match.
pub fn parse_metrics(bytes: &[u8], tracked: &BTreeSet<String>) -> Result<Vec<MetricPoint>, String> {
    let req: MetricsRequest = serde_json::from_slice(bytes)
        .map_err(|e| format!("invalid OTLP/JSON metrics body: {e}"))?;
    let want: BTreeSet<&str> = tracked.iter().map(|s| normalize(s)).collect();
    let mut out = Vec::new();
    for rm in &req.resource_metrics {
        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                let name = normalize(&metric.name);
                if !want.contains(name) {
                    continue;
                }
                let (points, delta) = match (&metric.sum, &metric.gauge) {
                    (Some(sum), _) => (&sum.data_points, is_delta(&sum.aggregation_temporality)),
                    (None, Some(gauge)) => (&gauge.data_points, false),
                    (None, None) => continue,
                };
                for dp in points {
                    let mut attrs: Vec<(String, String)> = dp
                        .attributes
                        .iter()
                        .map(|kv| (kv.key.clone(), kv.value.as_string()))
                        .collect();
                    let session_id = take(&mut attrs, "session.id");
                    if session_id.is_empty() {
                        continue;
                    }
                    attrs.sort();
                    out.push(MetricPoint {
                        name: name.to_string(),
                        value: dp.value(),
                        session_id,
                        series: attrs,
                        delta,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Decode logs into `(session_id, normalized_event_name)` pairs for counted events.
/// Names are normalized by stripping the `claude_code.` prefix so the match holds
/// whether the wire form is prefixed or bare.
pub fn parse_events(
    bytes: &[u8],
    counted: &BTreeSet<String>,
) -> Result<Vec<(String, String)>, String> {
    let req: LogsRequest =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid OTLP/JSON logs body: {e}"))?;
    let counted: BTreeSet<&str> = counted.iter().map(|s| normalize(s)).collect();
    let mut out = Vec::new();
    for rl in &req.resource_logs {
        for sl in &rl.scope_logs {
            for record in &sl.log_records {
                let mut event = String::new();
                let mut session = String::new();
                for kv in &record.attributes {
                    match kv.key.as_str() {
                        "event.name" => event = kv.value.as_string(),
                        "session.id" => session = kv.value.as_string(),
                        _ => {}
                    }
                }
                let name = normalize(&event);
                if !session.is_empty() && counted.contains(name) {
                    out.push((session, name.to_string()));
                }
            }
        }
    }
    Ok(out)
}

/// One decoded `tool_result` event: a single tool call's outcome. This is the only OTel signal
/// carrying a tool's wall-clock `duration_ms` and `success`, so the `tool` Kind is sourced here
/// rather than from the (duration/outcome-less) `PostToolUse` hook.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub session_id: String,
    pub tool_name: String,
    pub duration_ms: i64,
    pub ok: bool,
}

/// Decode `tool_result` log events into per-call outcomes. A record is yielded only when it is a
/// `tool_result` carrying both identifying fields (`session.id`, `tool_name`); `duration_ms` and
/// `success` are read leniently (proto3-JSON may encode them as number, string, or bool). A
/// `success` attribute that is absent is treated as a success — a `tool_result` is emitted on
/// completion, so its absence is a missing annotation, never evidence of failure (don't fabricate
/// a failure).
pub fn parse_tool_results(bytes: &[u8]) -> Result<Vec<ToolResult>, String> {
    let req: LogsRequest =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid OTLP/JSON logs body: {e}"))?;
    let mut out = Vec::new();
    for rl in &req.resource_logs {
        for sl in &rl.scope_logs {
            for record in &sl.log_records {
                let mut event = String::new();
                let mut session = String::new();
                let mut tool = String::new();
                let mut duration = 0i64;
                let mut ok = true;
                for kv in &record.attributes {
                    match kv.key.as_str() {
                        "event.name" => event = kv.value.as_string(),
                        "session.id" => session = kv.value.as_string(),
                        "tool_name" => tool = kv.value.as_string(),
                        "duration_ms" => duration = kv.value.as_f64() as i64,
                        "success" => ok = kv.value.as_bool(),
                        _ => {}
                    }
                }
                if normalize(&event) == "tool_result" && !session.is_empty() && !tool.is_empty() {
                    out.push(ToolResult {
                        session_id: session,
                        tool_name: tool,
                        duration_ms: duration,
                        ok,
                    });
                }
            }
        }
    }
    Ok(out)
}

pub fn normalize(event: &str) -> &str {
    event.strip_prefix("claude_code.").unwrap_or(event)
}

fn take(attrs: &mut Vec<(String, String)>, key: &str) -> String {
    if let Some(i) = attrs.iter().position(|(k, _)| k == key) {
        attrs.remove(i).1
    } else {
        String::new()
    }
}

/// Whether a sum's `aggregationTemporality` marks it delta. Anything else — cumulative,
/// unspecified, or absent (proto3-JSON may omit an unspecified enum) — reads as non-delta.
/// That is the conservative direction: treating a running total as a per-interval increment
/// would inflate counts, while the reverse merely replaces; it also matches OTLP, whose
/// default temporality for sums is cumulative.
fn is_delta(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Number(n) => n.as_i64() == Some(1),
        serde_json::Value::String(s) => s == "1" || s == "AGGREGATION_TEMPORALITY_DELTA",
        _ => false,
    }
}

fn json_to_f64(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

// ── OTLP/JSON shapes (only the fields we read) ──

#[derive(Debug, Default, Deserialize)]
struct MetricsRequest {
    #[serde(default, rename = "resourceMetrics")]
    resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceMetrics {
    #[serde(default, rename = "scopeMetrics")]
    scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Debug, Default, Deserialize)]
struct ScopeMetrics {
    #[serde(default)]
    metrics: Vec<Metric>,
}

#[derive(Debug, Default, Deserialize)]
struct Metric {
    #[serde(default)]
    name: String,
    #[serde(default)]
    sum: Option<Sum>,
    #[serde(default)]
    gauge: Option<Gauge>,
}

#[derive(Debug, Default, Deserialize)]
struct Sum {
    #[serde(default, rename = "aggregationTemporality")]
    aggregation_temporality: serde_json::Value,
    #[serde(default, rename = "dataPoints")]
    data_points: Vec<DataPoint>,
}

#[derive(Debug, Default, Deserialize)]
struct Gauge {
    #[serde(default, rename = "dataPoints")]
    data_points: Vec<DataPoint>,
}

#[derive(Debug, Default, Deserialize)]
struct DataPoint {
    #[serde(default)]
    attributes: Vec<KeyValue>,
    #[serde(default, rename = "asInt")]
    as_int: Option<serde_json::Value>,
    #[serde(default, rename = "asDouble")]
    as_double: Option<f64>,
}

impl DataPoint {
    fn value(&self) -> f64 {
        if let Some(i) = &self.as_int {
            return json_to_f64(i);
        }
        self.as_double.unwrap_or(0.0)
    }
}

#[derive(Debug, Default, Deserialize)]
struct LogsRequest {
    #[serde(default, rename = "resourceLogs")]
    resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceLogs {
    #[serde(default, rename = "scopeLogs")]
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Default, Deserialize)]
struct ScopeLogs {
    #[serde(default, rename = "logRecords")]
    log_records: Vec<LogRecord>,
}

#[derive(Debug, Default, Deserialize)]
struct LogRecord {
    #[serde(default)]
    attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
struct KeyValue {
    key: String,
    #[serde(default)]
    value: AnyValue,
}

#[derive(Debug, Default, Deserialize)]
struct AnyValue {
    #[serde(default, rename = "stringValue")]
    string_value: Option<String>,
    #[serde(default, rename = "intValue")]
    int_value: Option<serde_json::Value>,
    #[serde(default, rename = "doubleValue")]
    double_value: Option<f64>,
    #[serde(default, rename = "boolValue")]
    bool_value: Option<bool>,
}

impl AnyValue {
    fn as_string(&self) -> String {
        if let Some(s) = &self.string_value {
            return s.clone();
        }
        if let Some(i) = &self.int_value {
            return match i {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
        if let Some(d) = self.double_value {
            return d.to_string();
        }
        if let Some(b) = self.bool_value {
            return b.to_string();
        }
        String::new()
    }

    /// Numeric value of an attribute, accepting proto3-JSON's number-or-string encodings
    /// (`duration_ms` may arrive as `intValue:"23"` or `doubleValue:23`).
    fn as_f64(&self) -> f64 {
        if let Some(i) = &self.int_value {
            return json_to_f64(i);
        }
        if let Some(d) = self.double_value {
            return d;
        }
        self.string_value
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0)
    }

    /// Boolean value of an attribute, accepting a JSON bool or the string `"true"` (proto3-JSON
    /// may encode `success` either way).
    fn as_bool(&self) -> bool {
        if let Some(b) = self.bool_value {
            return b;
        }
        self.string_value.as_deref() == Some("true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked() -> BTreeSet<String> {
        ["claude_code.token.usage"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn delta_sum_attributes_to_session_and_folds_series() {
        let body = serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": 1, "dataPoints": [{
                    "attributes": [
                        {"key": "session.id", "value": {"stringValue": "S1"}},
                        {"key": "type", "value": {"stringValue": "output"}}
                    ],
                    "asInt": "100"
                }]}
            }]}]}]
        })
        .to_string();
        let points = parse_metrics(body.as_bytes(), &tracked()).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].session_id, "S1");
        assert_eq!(points[0].value, 100.0);
        assert!(points[0].delta);
        assert_eq!(
            points[0].series,
            vec![("type".to_string(), "output".to_string())]
        );
    }

    #[test]
    fn tool_result_decodes_duration_and_outcome() {
        let body = serde_json::json!({
            "resourceLogs": [{ "scopeLogs": [{ "logRecords": [
                { "attributes": [
                    {"key": "event.name", "value": {"stringValue": "claude_code.tool_result"}},
                    {"key": "session.id", "value": {"stringValue": "S1"}},
                    {"key": "tool_name", "value": {"stringValue": "Bash"}},
                    {"key": "duration_ms", "value": {"intValue": "23"}},
                    {"key": "success", "value": {"boolValue": true}}
                ]},
                { "attributes": [
                    {"key": "event.name", "value": {"stringValue": "claude_code.tool_result"}},
                    {"key": "session.id", "value": {"stringValue": "S1"}},
                    {"key": "tool_name", "value": {"stringValue": "Edit"}},
                    {"key": "duration_ms", "value": {"doubleValue": 5.0}},
                    {"key": "success", "value": {"stringValue": "false"}}
                ]},
                // A non-tool_result event is ignored.
                { "attributes": [
                    {"key": "event.name", "value": {"stringValue": "claude_code.tool_decision"}},
                    {"key": "session.id", "value": {"stringValue": "S1"}}
                ]}
            ]}]}]
        })
        .to_string();
        let r = parse_tool_results(body.as_bytes()).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            ToolResult {
                session_id: "S1".into(),
                tool_name: "Bash".into(),
                duration_ms: 23,
                ok: true
            }
        );
        assert_eq!(
            r[1],
            ToolResult {
                session_id: "S1".into(),
                tool_name: "Edit".into(),
                duration_ms: 5,
                ok: false
            }
        );
    }

    #[test]
    fn tool_result_without_success_attribute_is_a_success() {
        // `tool_result` fires on completion; a missing `success` is a missing annotation, not a
        // failure — never fabricate a failure.
        let body = serde_json::json!({
            "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{ "attributes": [
                {"key": "event.name", "value": {"stringValue": "tool_result"}},
                {"key": "session.id", "value": {"stringValue": "S1"}},
                {"key": "tool_name", "value": {"stringValue": "Read"}},
                {"key": "duration_ms", "value": {"intValue": "9"}}
            ]}]}]}]
        })
        .to_string();
        let r = parse_tool_results(body.as_bytes()).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].ok, "absent success defaults to ok");
    }

    #[test]
    fn enum_named_temporality_is_delta() {
        let body = serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": "AGGREGATION_TEMPORALITY_DELTA", "dataPoints": [{
                    "attributes": [{"key": "session.id", "value": {"stringValue": "S"}}],
                    "asInt": "3"
                }]}
            }]}]}]
        })
        .to_string();
        assert!(parse_metrics(body.as_bytes(), &tracked()).unwrap()[0].delta);
    }

    #[test]
    fn missing_temporality_reads_as_cumulative() {
        // A sum with no `aggregationTemporality` must read as cumulative (replace, never
        // accumulate) — the OTLP default, and the direction that can inflate nothing. This is
        // load-bearing for the accumulator, which adds delta points and replaces cumulative ones.
        let body = serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "dataPoints": [{
                    "attributes": [{"key": "session.id", "value": {"stringValue": "S"}}],
                    "asInt": "7"
                }]}
            }]}]}]
        })
        .to_string();
        let points = parse_metrics(body.as_bytes(), &tracked()).unwrap();
        assert_eq!(points.len(), 1);
        assert!(!points[0].delta, "absent temporality is cumulative");
    }

    #[test]
    fn gauge_is_not_delta() {
        let body = serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "gauge": { "dataPoints": [{
                    "attributes": [{"key": "session.id", "value": {"stringValue": "S"}}],
                    "asDouble": 5.0
                }]}
            }]}]}]
        })
        .to_string();
        let points = parse_metrics(body.as_bytes(), &tracked()).unwrap();
        assert_eq!(points.len(), 1);
        assert!(!points[0].delta);
        assert_eq!(points[0].value, 5.0);
    }

    #[test]
    fn datapoint_without_session_is_dropped() {
        let body = serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": 1, "dataPoints": [{"attributes": [], "asInt": "9"}]}
            }]}]}]
        })
        .to_string();
        assert!(
            parse_metrics(body.as_bytes(), &tracked())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn events_match_on_normalized_name() {
        let counted: BTreeSet<String> = ["skill_activated"].iter().map(|s| s.to_string()).collect();
        let body = serde_json::json!({
            "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{
                "attributes": [
                    {"key": "event.name", "value": {"stringValue": "claude_code.skill_activated"}},
                    {"key": "session.id", "value": {"stringValue": "S1"}}
                ]
            }]}]}]
        })
        .to_string();
        let pairs = parse_events(body.as_bytes(), &counted).unwrap();
        assert_eq!(
            pairs,
            vec![("S1".to_string(), "skill_activated".to_string())]
        );
    }

    #[test]
    fn malformed_body_is_an_error_not_an_empty_batch() {
        // A non-JSON / wrong-protocol body must stay distinguishable from a valid empty batch: the
        // receiver always answers 200 (the status means "received"), so this distinction drives the
        // stderr note for an undecodable body, not a status code.
        assert!(parse_metrics(b"not json", &tracked()).is_err());
        assert!(parse_events(b"\x00\x01protobuf", &BTreeSet::new()).is_err());
        // a valid-but-empty OTLP body is Ok(empty), not an error.
        assert!(parse_metrics(b"{}", &tracked()).unwrap().is_empty());
    }
}
