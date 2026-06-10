//! Enriched transform: inject the `project` label — resolved per datapoint from its own
//! `session.id` — into an OTLP/JSON metrics or logs body. Pure (a resolver closure, no fs), so
//! it is fully unit-testable. It walks an untyped `serde_json::Value` rather than the typed
//! shapes in `otlp/decode.rs`, which drop unknown fields: forwarding must be lossless, so every
//! field's value and structure is preserved (object key order may be re-sorted on re-serialization,
//! which is insignificant to any OTLP parser) except the one attribute we add. A datapoint whose
//! session is unknown is forwarded unchanged (the label is omitted, never fabricated).
//!
//! `session.id` is read at the datapoint / log-record level — the layer Claude Code emits it on,
//! and the same layer `otlp/decode.rs` reads for the local view, so the two never disagree. A
//! resource-level `session.id` (a layout Claude Code does not produce) is intentionally not
//! consulted; that would have to change in lockstep with `decode.rs` to stay consistent.

use std::collections::BTreeSet;

use serde_json::{Value, json};

/// Resolve a `session.id` to a project label (`None` when unknown).
pub type Resolve<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Collect the distinct `session.id`s carried by an OTLP body — at the datapoint level for
/// metrics, the log-record level for logs (the same layer enrichment reads). Returns `None` when
/// the body isn't parseable JSON; `Some` (possibly empty) otherwise. The per-destination project
/// filter uses this to resolve a batch's project before deciding whether to forward it.
pub fn session_ids(body: &[u8], is_metrics: bool) -> Option<BTreeSet<String>> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let mut ids = BTreeSet::new();
    if is_metrics {
        for rm in read_children(&root, "resourceMetrics") {
            for sm in read_children(rm, "scopeMetrics") {
                for metric in read_children(sm, "metrics") {
                    for shape in ["sum", "gauge"] {
                        let points = metric
                            .get(shape)
                            .and_then(|s| s.get("dataPoints"))
                            .and_then(Value::as_array);
                        for dp in points.into_iter().flatten() {
                            if let Some(sid) = point_session_id(dp) {
                                ids.insert(sid.to_string());
                            }
                        }
                    }
                }
            }
        }
    } else {
        for rl in read_children(&root, "resourceLogs") {
            for sl in read_children(rl, "scopeLogs") {
                for rec in read_children(sl, "logRecords") {
                    if let Some(sid) = point_session_id(rec) {
                        ids.insert(sid.to_string());
                    }
                }
            }
        }
    }
    Some(ids)
}

/// Inject `project` into every metrics datapoint whose `session.id` resolves. Returns the
/// re-serialized body, or `Err` if the body isn't parseable JSON (the caller skips this
/// destination rather than forwarding a non-enriched stream).
pub fn enrich_metrics(body: &[u8], resolve: &Resolve) -> Result<Vec<u8>, String> {
    let mut root: Value =
        serde_json::from_slice(body).map_err(|e| format!("metrics body is not JSON: {e}"))?;
    for rm in children(&mut root, "resourceMetrics") {
        for sm in children(rm, "scopeMetrics") {
            for metric in children(sm, "metrics") {
                // A metric is a sum or a gauge (the two shapes carrying datapoints we attribute).
                for shape in ["sum", "gauge"] {
                    let Some(points) = metric
                        .get_mut(shape)
                        .and_then(|s| s.get_mut("dataPoints"))
                        .and_then(Value::as_array_mut)
                    else {
                        continue;
                    };
                    for dp in points {
                        inject_project(dp, resolve);
                    }
                }
            }
        }
    }
    serde_json::to_vec(&root).map_err(|e| e.to_string())
}

/// Inject `project` into every log record whose `session.id` resolves.
pub fn enrich_logs(body: &[u8], resolve: &Resolve) -> Result<Vec<u8>, String> {
    let mut root: Value =
        serde_json::from_slice(body).map_err(|e| format!("logs body is not JSON: {e}"))?;
    for rl in children(&mut root, "resourceLogs") {
        for sl in children(rl, "scopeLogs") {
            for record in children(sl, "logRecords") {
                inject_project(record, resolve);
            }
        }
    }
    serde_json::to_vec(&root).map_err(|e| e.to_string())
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

/// Add a `project` attribute to one datapoint/record, resolved from its own `session.id`.
/// No-op when: the attribute set is absent, a `project` is already present (don't duplicate),
/// the datapoint carries no `session.id`, or the session doesn't resolve (don't fabricate).
fn inject_project(point: &mut Value, resolve: &Resolve) {
    let session = point_session_id(point).map(str::to_string);
    let Some(attrs) = point.get_mut("attributes").and_then(Value::as_array_mut) else {
        return;
    };
    if attrs.iter().any(|a| attr_key(a) == Some("project")) {
        return;
    }
    let Some(session) = session else { return };
    let Some(label) = resolve(&session) else {
        return;
    };
    attrs.push(json!({ "key": "project", "value": { "stringValue": label } }));
}

/// The `session.id` string attribute on one datapoint/record, if present.
fn point_session_id(point: &Value) -> Option<&str> {
    point
        .get("attributes")?
        .as_array()?
        .iter()
        .find(|a| attr_key(a) == Some("session.id"))?
        .get("value")?
        .get("stringValue")?
        .as_str()
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
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha")])).unwrap();
        assert_eq!(project_of(&first_datapoint(&out)).as_deref(), Some("alpha"));
    }

    #[test]
    fn multi_session_batch_enriches_each_independently() {
        let body = metrics_body(json!([
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "1" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S2" } }], "asInt": "2" }
        ]));
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha"), ("S2", "beta")])).unwrap();
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
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha")])).unwrap();
        assert_eq!(
            project_of(&first_datapoint(&out)),
            None,
            "no label is fabricated"
        );
    }

    #[test]
    fn datapoint_without_session_is_untouched() {
        let body = metrics_body(json!([{ "attributes": [], "asInt": "1" }]));
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha")])).unwrap();
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
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha")])).unwrap();
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
        let out = enrich_metrics(&body, &resolver(&[("S1", "alpha")])).unwrap();
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
    fn enrich_logs_injects_into_records() {
        let body = serde_json::to_vec(&json!({
            "resourceLogs": [{ "scopeLogs": [{ "logRecords": [{
                "attributes": [
                    { "key": "event.name", "value": { "stringValue": "claude_code.user_prompt" } },
                    { "key": "session.id", "value": { "stringValue": "S1" } }
                ]
            }]}]}]
        }))
        .unwrap();
        let out = enrich_logs(&body, &resolver(&[("S1", "alpha")])).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let rec = &v["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(project_of(rec).as_deref(), Some("alpha"));
    }

    #[test]
    fn non_json_body_is_an_error() {
        assert!(enrich_metrics(b"not json", &resolver(&[])).is_err());
        assert!(enrich_logs(b"\x00\x01proto", &resolver(&[])).is_err());
    }

    #[test]
    fn session_ids_collects_distinct_across_datapoints() {
        let body = metrics_body(json!([
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "1" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S2" } }], "asInt": "2" },
            { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }], "asInt": "3" }
        ]));
        let ids = session_ids(&body, true).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("S1") && ids.contains("S2"));
    }

    #[test]
    fn session_ids_none_for_non_json_some_empty_for_sessionless() {
        assert!(
            session_ids(b"\x00proto", true).is_none(),
            "non-JSON is None"
        );
        let body = metrics_body(json!([{ "attributes": [], "asInt": "1" }]));
        assert!(
            session_ids(&body, true).unwrap().is_empty(),
            "parseable but sessionless is an empty set"
        );
    }
}
