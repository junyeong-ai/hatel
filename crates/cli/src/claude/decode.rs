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
    pub series: Vec<(String, ScalarValue)>,
    /// True when the datapoint is a per-interval increment to accumulate; false
    /// when it is a running total (cumulative sum or gauge) that replaces.
    pub delta: bool,
    pub project: Option<hatel_core::ProjectRef>,
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
                    (Some(sum), None) => {
                        (&sum.data_points, is_delta(&sum.aggregation_temporality)?)
                    }
                    (None, Some(gauge)) => (&gauge.data_points, false),
                    (None, None) => continue,
                    (Some(_), Some(_)) => return Err("metric contains both sum and gauge".into()),
                };
                for dp in points {
                    let session_id = dp
                        .attributes
                        .iter()
                        .find(|kv| kv.key == super::SESSION_ID)
                        .map(|kv| kv.value.as_string())
                        .transpose()?
                        .unwrap_or_default();
                    let mut attrs: Vec<(String, ScalarValue)> = dp
                        .attributes
                        .iter()
                        .filter(|kv| kv.key != super::SESSION_ID)
                        .map(|kv| Ok((kv.key.clone(), kv.value.scalar()?)))
                        .collect::<Result<_, String>>()?;
                    if session_id.is_empty() {
                        continue;
                    }
                    attrs.sort();
                    out.push(MetricPoint {
                        name: name.to_string(),
                        value: dp.value()?,
                        session_id,
                        series: attrs,
                        delta,
                        project: repository(&rm.resource.attributes, &dp.attributes)?,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Decode one OTLP/JSON logs body into counted-event `(session_id, normalized_name)` pairs. Names
/// are normalized by stripping the `claude_code.` prefix so a match holds whether the wire form is
/// prefixed or bare. A record yields a pair when its normalized `event.name` is in `counted` and it
/// carries a `session.id`.
pub fn parse_logs(
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
                let event = record.name()?.unwrap_or_default();
                let mut session = String::new();
                for kv in &record.attributes {
                    match kv.key.as_str() {
                        "event.name" => {}
                        super::SESSION_ID => session = kv.value.as_string()?,
                        _ => {}
                    }
                }
                if session.is_empty() {
                    continue;
                }
                let name = normalize(&event);
                if counted.contains(name) {
                    out.push((session, name.to_string()));
                }
            }
        }
    }
    Ok(out)
}
pub fn normalize(event: &str) -> &str {
    event.strip_prefix("claude_code.").unwrap_or(event)
}

fn is_delta(v: &serde_json::Value) -> Result<bool, String> {
    match v {
        serde_json::Value::Number(n) if n.as_i64() == Some(1) => Ok(true),
        serde_json::Value::Number(n) if n.as_i64() == Some(2) => Ok(false),
        serde_json::Value::String(s) if s == "1" || s == "AGGREGATION_TEMPORALITY_DELTA" => {
            Ok(true)
        }
        serde_json::Value::String(s) if s == "2" || s == "AGGREGATION_TEMPORALITY_CUMULATIVE" => {
            Ok(false)
        }
        _ => Err("sum must declare delta or cumulative aggregationTemporality".into()),
    }
}

fn json_to_f64(v: &serde_json::Value) -> Result<f64, String> {
    let number = match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    };
    number
        .filter(|n: &f64| n.is_finite())
        .ok_or_else(|| "expected a finite OTLP number".into())
}

fn integer(v: &serde_json::Value) -> Result<i64, String> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    }
    .ok_or_else(|| "expected a signed 64-bit OTLP integer".into())
}

pub(super) fn repository(
    resource: &[KeyValue],
    attributes: &[KeyValue],
) -> Result<Option<hatel_core::ProjectRef>, String> {
    let field = |name: &str| -> Result<Option<String>, String> {
        let resource = resource
            .iter()
            .find(|kv| kv.key == name)
            .map(|kv| kv.value.as_string())
            .transpose()?;
        let point = attributes
            .iter()
            .find(|kv| kv.key == name)
            .map(|kv| kv.value.as_string())
            .transpose()?;
        if matches!((&resource,&point),(Some(a),Some(b)) if a!=b) {
            return Err(format!("conflicting repository attribute: {name}"));
        }
        Ok(point.or(resource))
    };
    let Some(url) = field("vcs.repository.url.full")? else {
        return Ok(None);
    };
    let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid vcs.repository.url.full")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("invalid repository browser URL".into());
    }
    Ok(Some(hatel_core::ProjectRef {
        key: format!("vcs:{url}"),
        label: field("vcs.repository.name")?.unwrap_or(url),
    }))
}

pub fn repository_projects(
    bytes: &[u8],
    metrics: bool,
) -> Result<std::collections::BTreeMap<String, hatel_core::ProjectRef>, String> {
    let mut projects = std::collections::BTreeMap::new();
    let mut add = |resource: &[KeyValue], attrs: &[KeyValue]| -> Result<(), String> {
        let session = attrs
            .iter()
            .find(|a| a.key == super::SESSION_ID)
            .map(|a| a.value.as_string())
            .transpose()?;
        if let (Some(session), Some(project)) = (session, repository(resource, attrs)?) {
            if projects
                .get(&session)
                .is_some_and(|p: &hatel_core::ProjectRef| p.key != project.key)
            {
                return Err("conflicting repository identities in one session".into());
            }
            projects.insert(session, project);
        }
        Ok(())
    };
    if metrics {
        let request: MetricsRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                for metric in scope.metrics {
                    if let Some(sum) = metric.sum {
                        for point in sum.data_points {
                            add(&resource.resource.attributes, &point.attributes)?;
                        }
                    }
                    for shape in [
                        metric.gauge,
                        metric.histogram,
                        metric.exponential_histogram,
                        metric.summary,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        for point in shape.data_points {
                            add(&resource.resource.attributes, &point.attributes)?;
                        }
                    }
                }
            }
        }
    } else {
        let request: LogsRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        for resource in request.resource_logs {
            for scope in resource.scope_logs {
                for record in scope.log_records {
                    add(&resource.resource.attributes, &record.attributes)?;
                }
            }
        }
    }
    Ok(projects)
}

// ── OTLP/JSON shapes (only the fields we read) ──

#[derive(Debug, Default, Deserialize)]
pub(super) struct Resource {
    #[serde(default, deserialize_with = "attributes")]
    pub attributes: Vec<KeyValue>,
}

#[derive(Debug, Default, Deserialize)]
struct MetricsRequest {
    #[serde(default, rename = "resourceMetrics")]
    resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceMetrics {
    #[serde(default)]
    resource: Resource,
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
    #[serde(default)]
    histogram: Option<Gauge>,
    #[serde(default, rename = "exponentialHistogram")]
    exponential_histogram: Option<Gauge>,
    #[serde(default)]
    summary: Option<Gauge>,
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
    #[serde(default, deserialize_with = "attributes")]
    attributes: Vec<KeyValue>,
    #[serde(default, rename = "asInt")]
    as_int: Option<serde_json::Value>,
    #[serde(default, rename = "asDouble")]
    as_double: Option<f64>,
}

impl DataPoint {
    fn value(&self) -> Result<f64, String> {
        match (&self.as_int, self.as_double) {
            (Some(i), None) => integer(i).map(|n| n as f64),
            (None, Some(value)) if value.is_finite() => Ok(value),
            _ => Err("datapoint must contain exactly one finite asInt or asDouble".into()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct LogsRequest {
    #[serde(default, rename = "resourceLogs")]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ResourceLogs {
    #[serde(default)]
    pub resource: Resource,
    #[serde(default, rename = "scopeLogs")]
    pub scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ScopeLogs {
    #[serde(default, rename = "logRecords")]
    pub log_records: Vec<LogRecord>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct LogRecord {
    #[serde(default, rename = "timeUnixNano")]
    pub time_unix_nano: Option<serde_json::Value>,
    #[serde(default, rename = "eventName")]
    pub event_name: Option<String>,
    #[serde(default, deserialize_with = "attributes")]
    pub attributes: Vec<KeyValue>,
}

impl LogRecord {
    pub(super) fn name(&self) -> Result<Option<String>, String> {
        let attribute = self
            .attributes
            .iter()
            .find(|kv| kv.key == "event.name")
            .map(|kv| kv.value.as_string())
            .transpose()?;
        if let (Some(event), Some(attribute)) = (&self.event_name, &attribute)
            && normalize(event) != normalize(attribute)
        {
            return Err("conflicting event names".into());
        }
        Ok(self
            .event_name
            .as_deref()
            .or(attribute.as_deref())
            .map(|name| normalize(name).to_string()))
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct KeyValue {
    pub key: String,
    #[serde(default)]
    pub value: AnyValue,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "RawAnyValue")]
pub(super) struct AnyValue {
    scalar: Option<ScalarValue>,
}

impl Default for AnyValue {
    fn default() -> Self {
        Self {
            scalar: Some(ScalarValue::Empty),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScalarValue {
    Empty,
    String(String),
    Integer(i64),
    Double(String),
    Boolean(bool),
}

impl From<&str> for ScalarValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawAnyValue {
    #[serde(default, rename = "stringValue")]
    string_value: Option<String>,
    #[serde(default, rename = "intValue")]
    int_value: Option<serde_json::Value>,
    #[serde(default, rename = "doubleValue")]
    double_value: Option<f64>,
    #[serde(default, rename = "boolValue")]
    bool_value: Option<bool>,
    #[serde(default, rename = "arrayValue")]
    array_value: Option<serde_json::Value>,
    #[serde(default, rename = "kvlistValue")]
    kvlist_value: Option<serde_json::Value>,
    #[serde(default, rename = "bytesValue")]
    bytes_value: Option<String>,
}

impl TryFrom<RawAnyValue> for AnyValue {
    type Error = String;

    fn try_from(raw: RawAnyValue) -> Result<Self, String> {
        let present = [
            raw.string_value.is_some(),
            raw.int_value.is_some(),
            raw.double_value.is_some(),
            raw.bool_value.is_some(),
            raw.array_value.is_some(),
            raw.kvlist_value.is_some(),
            raw.bytes_value.is_some(),
        ];
        if present.into_iter().filter(|p| *p).count() > 1 {
            return Err("OTLP AnyValue must contain at most one value".into());
        }
        let scalar = if let Some(s) = raw.string_value {
            Some(ScalarValue::String(s))
        } else if let Some(i) = raw.int_value {
            Some(ScalarValue::Integer(integer(&i)?))
        } else if let Some(d) = raw.double_value {
            if !d.is_finite() {
                return Err("expected a finite OTLP double".into());
            }
            Some(ScalarValue::Double(if d == 0.0 {
                "0".into()
            } else {
                d.to_string()
            }))
        } else if let Some(b) = raw.bool_value {
            Some(ScalarValue::Boolean(b))
        } else if raw.array_value.is_some()
            || raw.kvlist_value.is_some()
            || raw.bytes_value.is_some()
        {
            None
        } else {
            Some(ScalarValue::Empty)
        };
        Ok(Self { scalar })
    }
}

impl AnyValue {
    fn scalar(&self) -> Result<ScalarValue, String> {
        self.scalar
            .clone()
            .ok_or_else(|| "local Claude metric attributes support scalar values only".into())
    }

    pub(super) fn number(&self) -> Result<serde_json::Value, String> {
        match &self.scalar {
            Some(ScalarValue::Integer(n)) => Ok((*n).into()),
            Some(ScalarValue::String(s)) => {
                if let Ok(n) = s.parse::<i64>() {
                    return Ok(n.into());
                }
                let n = json_to_f64(&serde_json::Value::String(s.clone()))?;
                Ok(serde_json::json!(n))
            }
            Some(ScalarValue::Double(s)) => Ok(serde_json::json!(json_to_f64(
                &serde_json::Value::String(s.clone())
            )?)),
            _ => Err("expected a numeric Claude event attribute".into()),
        }
    }

    pub(super) fn as_string(&self) -> Result<String, String> {
        match &self.scalar {
            Some(ScalarValue::String(s)) => Ok(s.clone()),
            _ => Err("expected a string Claude attribute".into()),
        }
    }
}

fn attributes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<KeyValue>, D::Error> {
    let attributes = Vec::<KeyValue>::deserialize(deserializer)?;
    let mut keys = BTreeSet::new();
    for attribute in &attributes {
        if attribute.key.is_empty() || !keys.insert(&attribute.key) {
            return Err(serde::de::Error::custom(
                "OTLP attribute keys must be non-empty and unique",
            ));
        }
    }
    Ok(attributes)
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
            vec![("type".to_string(), "output".into())]
        );
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
    fn missing_sum_temporality_is_rejected() {
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
        assert!(parse_metrics(body.as_bytes(), &tracked()).is_err());
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
        let pairs = parse_logs(body.as_bytes(), &counted).unwrap();
        assert_eq!(
            pairs,
            vec![("S1".to_string(), "skill_activated".to_string())]
        );
    }

    #[test]
    fn malformed_body_is_an_error_not_an_empty_batch() {
        assert!(parse_metrics(b"not json", &tracked()).is_err());
        assert!(parse_logs(b"\x00\x01protobuf", &BTreeSet::new()).is_err());
        // a valid-but-empty OTLP body is Ok(empty), not an error.
        assert!(parse_metrics(b"{}", &tracked()).unwrap().is_empty());
        assert_eq!(
            parse_logs(b"{}", &BTreeSet::new()).unwrap(),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn ambiguous_or_invalid_native_attributes_are_rejected() {
        let metric = |attributes: serde_json::Value, value: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({"resourceMetrics":[{"scopeMetrics":[{"metrics":[{
                "name":"claude_code.token.usage", "gauge":{"dataPoints":[{"attributes":attributes,"asInt":value}]}
            }]}]}]})).unwrap()
        };
        let session = serde_json::json!({"key":"session.id","value":{"stringValue":"S"}});
        for attributes in [
            serde_json::json!([session, session]),
            serde_json::json!([{"key":"session.id","value":{"intValue":"1"}}]),
            serde_json::json!([{"key":"session.id","value":{"stringValue":"S","boolValue":true}}]),
            serde_json::json!([session,{"key":"x","value":{"intValue":"9223372036854775808"}}]),
            serde_json::json!([session,{"key":"","value":{"stringValue":"x"}}]),
        ] {
            assert!(
                parse_metrics(
                    &metric(attributes.clone(), serde_json::json!(1)),
                    &tracked()
                )
                .is_err(),
                "{attributes}"
            );
        }
        for value in [
            serde_json::json!(1.5),
            serde_json::json!("9223372036854775808"),
        ] {
            assert!(
                parse_metrics(
                    &metric(serde_json::json!([session]), value.clone()),
                    &tracked()
                )
                .is_err(),
                "{value}"
            );
        }
        assert_eq!(
            parse_metrics(
                &metric(serde_json::json!([session]), serde_json::json!("0")),
                &tracked()
            )
            .unwrap()[0]
                .value,
            0.0
        );
    }

    #[test]
    fn cumulative_series_keep_attribute_types_and_reject_unsupported_dimensions() {
        let mut points = Vec::new();
        for value in [
            serde_json::json!({"stringValue":"1"}),
            serde_json::json!({"intValue":"1"}),
            serde_json::json!({"boolValue":true}),
            serde_json::json!({}),
        ] {
            let body = serde_json::to_vec(&serde_json::json!({"resourceMetrics":[{"scopeMetrics":[{"metrics":[{
                "name":"claude_code.token.usage", "gauge":{"dataPoints":[{"attributes":[
                    {"key":"session.id","value":{"stringValue":"S"}}, {"key":"dimension","value":value}
                ],"asInt":"2"}]}
            }]}]}]})).unwrap();
            points.extend(parse_metrics(&body, &tracked()).unwrap());
        }
        let mut accumulator = super::super::accumulator::Accumulator::default();
        accumulator.update_metrics(points, jiff::Timestamp::UNIX_EPOCH);
        assert_eq!(accumulator.sessions()["S"].tokens(), 8);
        let compound: AnyValue =
            serde_json::from_value(serde_json::json!({"arrayValue":{"values":[]}})).unwrap();
        assert!(compound.scalar().is_err());
    }
}
