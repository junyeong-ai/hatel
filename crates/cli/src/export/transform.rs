//! Enriched transform: inject the `project` label — resolved per record from its own
//! `session.id` — into an OTLP/JSON metrics, logs or traces body. Pure (a resolver closure, no
//! fs), so it is fully unit-testable. It walks an untyped `serde_json::Value` rather than the
//! typed shapes in `claude/decode.rs`, which drop unknown fields: forwarding must be lossless, so
//! every field's value and structure is preserved (object key order may be re-sorted on
//! re-serialization, which is insignificant to any OTLP parser) except the one attribute we add. A
//! record whose session is unknown is forwarded unchanged (the label is omitted, never fabricated).
//!
//! Attributes are read at the record level — metric datapoints, log records and spans — the layer
//! Claude Code emits `session.id` and `user.email` on, and the layer `claude/decode.rs` reads
//! `session.id` from for the local view, so the two never disagree. A resource-level copy is
//! intentionally not consulted; that would have to change in lockstep with `decode.rs` to stay
//! consistent.

use std::collections::BTreeSet;

use hatel_core::OtlpSignal;
use serde_json::{Value, json};

use crate::claude::{PROJECT, SESSION_ID};

/// Resolve a `session.id` to a project label (`None` when unknown).
pub type Resolve<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// The data shapes a metric can hold; a well-formed metric holds exactly one.
const METRIC_SHAPES: [&str; 5] = [
    "sum",
    "gauge",
    "histogram",
    "exponentialHistogram",
    "summary",
];

/// The resource, scope and item arrays a signal's body nests its records in. A metric is an item
/// whose records are the datapoints of its data shape; a log record or a span is a record itself.
fn layout(signal: OtlpSignal) -> [&'static str; 3] {
    match signal {
        OtlpSignal::Metrics => ["resourceMetrics", "scopeMetrics", "metrics"],
        OtlpSignal::Logs => ["resourceLogs", "scopeLogs", "logRecords"],
        OtlpSignal::Traces => ["resourceSpans", "scopeSpans", "spans"],
    }
}

/// Collect the distinct values of the string attribute `key` carried by an OTLP body's records.
/// `None` unless the body holds at least one record and every record carries one non-empty value,
/// including metric types the local Claude Code view does not aggregate: a filtered batch is
/// attributed in full or not at all.
pub fn attribute_values(body: &[u8], signal: OtlpSignal, key: &str) -> Option<BTreeSet<String>> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let mut values = BTreeSet::new();
    for record in records(&root, signal)? {
        values.insert(point_attribute(record, key)?.to_string());
    }
    (!values.is_empty()).then_some(values)
}

/// Inject `project` into every record whose `session.id` resolves. Returns the re-serialized body,
/// or `Err` if the body isn't parseable JSON (the caller skips this destination rather than
/// forwarding a non-enriched stream).
pub fn enrich(body: &[u8], signal: OtlpSignal, resolve: &Resolve) -> Result<Vec<u8>, String> {
    let mut root: Value = serde_json::from_slice(body)
        .map_err(|e| format!("{} body is not JSON: {e}", signal.as_str()))?;
    for record in records_mut(&mut root, signal) {
        inject_project(record, resolve);
    }
    serde_json::to_vec(&root).map_err(|e| e.to_string())
}

/// Every record of a body. `None` when a metric does not hold exactly one data shape, since its
/// datapoints could then not all be attributed.
fn records(root: &Value, signal: OtlpSignal) -> Option<Vec<&Value>> {
    let [resources, scopes, items] = layout(signal);
    let items = read_children(root, resources)
        .flat_map(|resource| read_children(resource, scopes))
        .flat_map(|scope| read_children(scope, items));
    if signal != OtlpSignal::Metrics {
        return Some(items.collect());
    }
    let mut points = Vec::new();
    for metric in items {
        let mut shapes = METRIC_SHAPES.iter().filter_map(|shape| metric.get(*shape));
        let (Some(shape), None) = (shapes.next(), shapes.next()) else {
            return None;
        };
        points.extend(read_children(shape, "dataPoints"));
    }
    Some(points)
}

/// Mutable counterpart of [`records`], over every data shape a metric holds.
fn records_mut(root: &mut Value, signal: OtlpSignal) -> Vec<&mut Value> {
    let [resources, scopes, items] = layout(signal);
    let items = children(root, resources)
        .flat_map(|resource| children(resource, scopes))
        .flat_map(|scope| children(scope, items));
    if signal != OtlpSignal::Metrics {
        return items.collect();
    }
    items
        .filter_map(Value::as_object_mut)
        .flat_map(|metric| {
            metric
                .iter_mut()
                .filter(|(field, _)| METRIC_SHAPES.contains(&field.as_str()))
                .flat_map(|(_, shape)| children(shape, "dataPoints"))
        })
        .collect()
}

/// Mutable iterator over a JSON array field, empty if the field is absent or not an array.
fn children<'a>(v: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    v.get_mut(key)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

/// Read-only counterpart of [`children`].
fn read_children<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// Add a `project` attribute to one record, resolved from its own `session.id`.
/// No-op when: the attribute set is absent, a `project` is already present (don't duplicate),
/// the record carries no `session.id`, or the session doesn't resolve (don't fabricate).
fn inject_project(point: &mut Value, resolve: &Resolve) {
    let session = point_attribute(point, SESSION_ID).map(str::to_string);
    let Some(attrs) = point.get_mut("attributes").and_then(Value::as_array_mut) else {
        return;
    };
    if attrs.iter().any(|a| attr_key(a) == Some(PROJECT)) {
        return;
    }
    let Some(session) = session else { return };
    let Some(label) = resolve(&session) else {
        return;
    };
    attrs.push(json!({ "key": PROJECT, "value": { "stringValue": label } }));
}

/// The non-empty string attribute `key` on one record — `None` when it is absent,
/// empty, not a string, or carried more than once.
fn point_attribute<'a>(point: &'a Value, key: &str) -> Option<&'a str> {
    let mut attributes = point
        .get("attributes")?
        .as_array()?
        .iter()
        .filter(|a| attr_key(a) == Some(key));
    let attribute = attributes.next()?;
    if attributes.next().is_some() {
        return None;
    }
    attribute
        .get("value")?
        .get("stringValue")?
        .as_str()
        .filter(|value| !value.is_empty())
}

fn attr_key(attr: &Value) -> Option<&str> {
    attr.get("key").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |s| {
            pairs
                .iter()
                .find(|(k, _)| *k == s)
                .map(|(_, v)| v.to_string())
        }
    }

    fn project_of(point: &Value) -> Option<String> {
        point["attributes"]
            .as_array()?
            .iter()
            .find(|a| attr_key(a) == Some("project"))?
            .get("value")?
            .get("stringValue")?
            .as_str()
            .map(str::to_string)
    }

    fn metrics_body(points: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": 1, "dataPoints": points }
            }]}]}]
        }))
        .unwrap()
    }

    fn first_datapoint(body: &[u8]) -> Value {
        let v: Value = serde_json::from_slice(body).unwrap();
        v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0].clone()
    }

    #[test]
    fn injects_project_at_datapoint_level() {
        let body = metrics_body(json!([{
            "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }],
            "asInt": "100"
        }]));
        let out = enrich(&body, OtlpSignal::Metrics, &resolver(&[("S1", "alpha")])).unwrap();
        assert_eq!(project_of(&first_datapoint(&out)).as_deref(), Some("alpha"));
    }

    #[test]
    fn multi_session_batch_enriches_each_independently() {
        let body = metrics_body(json!([
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "1" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S2" } }], "asInt": "2" }
        ]));
        let out = enrich(
            &body,
            OtlpSignal::Metrics,
            &resolver(&[("S1", "alpha"), ("S2", "beta")]),
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let dps = v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
            .as_array()
            .unwrap();
        assert_eq!(project_of(&dps[0]).as_deref(), Some("alpha"));
        assert_eq!(project_of(&dps[1]).as_deref(), Some("beta"));
    }

    #[test]
    fn unknown_session_leaves_that_datapoint_unenriched() {
        let body = metrics_body(json!([{
            "attributes": [{ "key": "session.id", "value": { "stringValue": "ghost" } }],
            "asInt": "1"
        }]));
        let out = enrich(&body, OtlpSignal::Metrics, &resolver(&[("S1", "alpha")])).unwrap();
        assert_eq!(
            project_of(&first_datapoint(&out)),
            None,
            "no label is fabricated"
        );
    }

    #[test]
    fn datapoint_without_session_is_untouched() {
        let body = metrics_body(json!([{ "attributes": [], "asInt": "1" }]));
        let out = enrich(&body, OtlpSignal::Metrics, &resolver(&[("S1", "alpha")])).unwrap();
        assert_eq!(project_of(&first_datapoint(&out)), None);
    }

    #[test]
    fn existing_project_is_not_duplicated() {
        let body = metrics_body(json!([{
            "attributes": [
                { "key": "session.id", "value": { "stringValue": "S1" } },
                { "key": "project", "value": { "stringValue": "preset" } }
            ],
            "asInt": "1"
        }]));
        let out = enrich(&body, OtlpSignal::Metrics, &resolver(&[("S1", "alpha")])).unwrap();
        let dp = first_datapoint(&out);
        let projects = dp["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| attr_key(a) == Some("project"))
            .count();
        assert_eq!(projects, 1, "no duplicate project attribute");
        assert_eq!(
            project_of(&dp).as_deref(),
            Some("preset"),
            "the existing value is kept"
        );
    }

    #[test]
    fn unknown_and_resource_fields_round_trip() {
        // A body carrying fields hatel never decodes (resource attributes, scope, exemplars) must
        // survive enrichment untouched — forwarding is lossless except the injected attribute.
        let body = serde_json::to_vec(&json!({
            "resourceMetrics": [{
                "resource": { "attributes": [{ "key": "host.name", "value": { "stringValue": "mac" } }] },
                "scopeMetrics": [{
                    "scope": { "name": "claude-code", "version": "1.2.3" },
                    "metrics": [{
                        "name": "claude_code.token.usage",
                        "sum": { "aggregationTemporality": 1, "isMonotonic": true, "dataPoints": [{
                            "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }],
                            "asInt": "5", "timeUnixNano": "123", "exemplars": []
                        }]}
                    }]
                }]
            }]
        }))
        .unwrap();
        let out = enrich(&body, OtlpSignal::Metrics, &resolver(&[("S1", "alpha")])).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["resourceMetrics"][0]["resource"]["attributes"][0]["key"],
            "host.name"
        );
        assert_eq!(
            v["resourceMetrics"][0]["scopeMetrics"][0]["scope"]["version"],
            "1.2.3"
        );
        let dp = &v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0];
        assert_eq!(dp["timeUnixNano"], "123");
        assert_eq!(
            v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["isMonotonic"],
            true
        );
        assert_eq!(project_of(dp).as_deref(), Some("alpha"));
    }

    #[test]
    fn log_records_and_spans_are_enriched_per_record() {
        let attributes = json!([
            { "key": "event.name", "value": { "stringValue": "claude_code.user_prompt" } },
            { "key": "session.id", "value": { "stringValue": "S1" } }
        ]);
        for (signal, body, record) in [
            (
                OtlpSignal::Logs,
                json!({"resourceLogs": [{"scopeLogs": [{"logRecords": [{"attributes": attributes}]}]}]}),
                "/resourceLogs/0/scopeLogs/0/logRecords/0",
            ),
            (
                OtlpSignal::Traces,
                json!({"resourceSpans": [{"scopeSpans": [{"spans": [{"name": "claude_code.tool", "attributes": attributes}]}]}]}),
                "/resourceSpans/0/scopeSpans/0/spans/0",
            ),
        ] {
            let body = serde_json::to_vec(&body).unwrap();
            let out = enrich(&body, signal, &resolver(&[("S1", "alpha")])).unwrap();
            let v: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                project_of(v.pointer(record).unwrap()).as_deref(),
                Some("alpha")
            );
        }
    }

    #[test]
    fn non_json_body_is_an_error() {
        for signal in OtlpSignal::ALL {
            assert!(enrich(b"\x00\x01proto", signal, &resolver(&[])).is_err());
        }
    }

    #[test]
    fn attribute_values_collects_distinct_across_datapoints() {
        let body = metrics_body(json!([
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "1" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S2" } }], "asInt": "2" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "3" }
        ]));
        let ids = attribute_values(&body, OtlpSignal::Metrics, SESSION_ID).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("S1") && ids.contains("S2"));
    }

    #[test]
    fn attribute_values_reject_bodies_that_do_not_attribute_every_record() {
        assert!(
            attribute_values(b"\x00proto", OtlpSignal::Metrics, SESSION_ID).is_none(),
            "non-JSON is None"
        );
        let body = metrics_body(json!([{ "attributes": [], "asInt": "1" }]));
        assert!(
            attribute_values(&body, OtlpSignal::Metrics, SESSION_ID).is_none(),
            "a record without a session cannot authorize a filtered batch"
        );
        assert!(
            attribute_values(&metrics_body(json!([])), OtlpSignal::Metrics, SESSION_ID).is_none(),
            "a body without records attributes nothing"
        );
    }

    #[test]
    fn a_filtered_batch_cannot_hide_unattributed_records_behind_a_known_session() {
        for shape in [
            "sum",
            "gauge",
            "histogram",
            "exponentialHistogram",
            "summary",
        ] {
            let body = json!({"resourceMetrics":[{"scopeMetrics":[{"metrics":[
                {"name":"known","sum":{"dataPoints":[{"attributes":[{"key":"session.id","value":{"stringValue":"company"}}]}]}},
                {"name":"unattributed",shape:{"dataPoints":[{"attributes":[]}]}}
            ]}]}]});
            assert!(
                attribute_values(
                    &serde_json::to_vec(&body).unwrap(),
                    OtlpSignal::Metrics,
                    SESSION_ID
                )
                .is_none(),
                "{shape}"
            );
        }
        let body = json!({"resourceLogs":[{"scopeLogs":[{"logRecords":[
            {"attributes":[{"key":"session.id","value":{"stringValue":"company"}}]},
            {"attributes":[]}
        ]}]}]});
        assert!(
            attribute_values(
                &serde_json::to_vec(&body).unwrap(),
                OtlpSignal::Logs,
                SESSION_ID
            )
            .is_none()
        );
        let body = json!({"resourceSpans":[{"scopeSpans":[{"spans":[
            {"attributes":[{"key":"session.id","value":{"stringValue":"company"}}]},
            {"attributes":[]}
        ]}]}]});
        assert!(
            attribute_values(
                &serde_json::to_vec(&body).unwrap(),
                OtlpSignal::Traces,
                SESSION_ID
            )
            .is_none()
        );
        let duplicate = json!({"resourceLogs":[{"scopeLogs":[{"logRecords":[{"attributes":[
            {"key":"session.id","value":{"stringValue":"company"}},
            {"key":"session.id","value":{"stringValue":"personal"}}
        ]}]}]}]});
        assert!(
            attribute_values(
                &serde_json::to_vec(&duplicate).unwrap(),
                OtlpSignal::Logs,
                SESSION_ID
            )
            .is_none()
        );
    }
}
