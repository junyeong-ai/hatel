//! Safe local projections of documented Claude Code events. Content stays in the input stream.

use super::decode::{LogsRequest, repository};
use hatel_core::{Envelope, Payload, Registry};

pub fn records(bytes: &[u8], registry: &Registry) -> Result<Vec<Envelope>, String> {
    let request: LogsRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for resource in request.resource_logs {
        for scope in resource.scope_logs {
            for record in scope.log_records {
                let attribute = |key: &str| {
                    record
                        .attributes
                        .iter()
                        .find(|kv| kv.key == key)
                        .map(|kv| &kv.value)
                };
                let name = record.name()?;
                let kind = match name.as_deref() {
                    Some("api_request") => "claude.api_request",
                    Some("hook_execution_complete") => "claude.hook",
                    _ => continue,
                };
                let timestamp = if let Some(timestamp) = attribute("event.timestamp") {
                    timestamp
                        .as_string()
                        .parse::<jiff::Timestamp>()
                        .map_err(|_| "invalid event.timestamp")?
                } else if let Some(timestamp) = record.time_unix_nano.as_ref().and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                }) {
                    let nanos = timestamp
                        .parse::<u64>()
                        .map_err(|_| "invalid timeUnixNano")?;
                    jiff::Timestamp::from_nanosecond(i128::from(nanos))
                        .map_err(|_| "invalid timeUnixNano")?
                } else {
                    return Err(format!("{kind} has no occurrence timestamp"));
                };
                let mut payload = Payload::new();
                let string_fields: &[(&str, &str)] = if kind == "claude.api_request" {
                    &[
                        ("session_id", "session.id"),
                        ("request_id", "request_id"),
                        ("model", "model"),
                        ("agent", "agent.name"),
                        ("skill", "skill.name"),
                        ("query_source", "query_source"),
                    ]
                } else {
                    &[
                        ("session_id", "session.id"),
                        ("hook_event", "hook_event"),
                        ("hook_name", "hook_name"),
                    ]
                };
                let number_fields: &[(&str, &str)] = if kind == "claude.api_request" {
                    &[
                        ("cost_usd", "cost_usd"),
                        ("duration_ms", "duration_ms"),
                        ("input_tokens", "input_tokens"),
                        ("output_tokens", "output_tokens"),
                        ("cache_read_tokens", "cache_read_tokens"),
                        ("cache_creation_tokens", "cache_creation_tokens"),
                    ]
                } else {
                    &[
                        ("duration_ms", "total_duration_ms"),
                        ("num_hooks", "num_hooks"),
                        ("num_success", "num_success"),
                        ("num_blocking", "num_blocking"),
                        ("num_non_blocking_error", "num_non_blocking_error"),
                        ("num_cancelled", "num_cancelled"),
                    ]
                };
                for (field, key) in string_fields {
                    if let Some(value) = attribute(key) {
                        payload.insert((*field).into(), value.as_string().into());
                    }
                }
                for (field, key) in number_fields {
                    if let Some(value) = attribute(key) {
                        payload.insert((*field).into(), value.number()?);
                    }
                }
                if !payload
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty())
                {
                    return Err(format!("{kind} requires a non-empty session.id"));
                }
                if payload.get("request_id").and_then(|v| v.as_str()) == Some("") {
                    payload.remove("request_id");
                }
                let mut env = hatel_core::make_envelope(kind, payload, registry, true)
                    .map_err(|e| e.to_string())?;
                env.ts = timestamp.to_string();
                env.metadata.source = Some("claude_code.otlp".into());
                env.metadata.projection = true;
                env.metadata.project =
                    repository(&resource.resource.attributes, &record.attributes)?;
                env.metadata.correlation_id = attribute("prompt.id").map(|v| v.as_string());
                env.metadata.run_id = attribute("workflow.run_id").map(|v| v.as_string());
                if kind == "claude.api_request"
                    && let (Some(session), Some(request)) = (
                        env.payload.get("session_id").and_then(|v| v.as_str()),
                        env.payload.get("request_id").and_then(|v| v.as_str()),
                    )
                {
                    env.metadata.record_id = Some(hatel_core::model::source_record_id(
                        kind,
                        &[session, request],
                    ));
                }
                out.push(env);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatel_core::Sink;
    use serde_json::json;

    fn event(attributes: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"resourceLogs":[{"resource":{"attributes":[
            {"key":"vcs.repository.url.full","value":{"stringValue":"https://example.com/team/repo"}},
            {"key":"vcs.repository.name","value":{"stringValue":"repo"}}
        ]},"scopeLogs":[{"logRecords":[{"eventName":"claude_code.api_request","attributes":attributes}]}]}]})).unwrap()
    }

    #[test]
    fn request_projections_use_occurrence_time_and_keep_content_out_of_storage() {
        let registry = hatel_core::schema::load_core().unwrap();
        let bytes = event(json!([
            {"key":"session.id","value":{"stringValue":"session"}},
            {"key":"event.timestamp","value":{"stringValue":"2026-01-01T00:00:00Z"}},
            {"key":"request_id","value":{"stringValue":"request"}},
            {"key":"model","value":{"stringValue":"model"}},
            {"key":"cost_usd","value":{"stringValue":"0.02"}},
            {"key":"input_tokens","value":{"stringValue":"123"}},
            {"key":"prompt","value":{"stringValue":"sensitive content"}},
            {"key":"body","value":{"stringValue":"sensitive API body"}}
        ]));
        let first = records(&bytes, &registry).unwrap().remove(0);
        let second = records(&bytes, &registry).unwrap().remove(0);
        assert_eq!(first.ts, "2026-01-01T00:00:00Z");
        assert_eq!(first.payload["cost_usd"], json!(0.02));
        assert_eq!(first.payload["input_tokens"], json!(123));
        assert_eq!(first.metadata.record_id, second.metadata.record_id);
        assert_eq!(
            first.metadata.project.as_ref().unwrap().key,
            "vcs:https://example.com/team/repo"
        );
        assert!(first.metadata.projection);
        assert!(!first.to_json_line().contains("sensitive"));
        let home = tempfile::tempdir().unwrap();
        let mut sink = hatel_core::sink::SqliteSink::open(home.path().join("telemetry.db"));
        sink.write_record(&first).unwrap();
        sink.write_record(&second).unwrap();
        let conn = rusqlite::Connection::open(home.path().join("telemetry.db")).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let mut conflict = second;
        conflict.payload.insert("cost_usd".into(), json!(9));
        assert!(sink.write_record(&conflict).is_err());
    }

    #[test]
    fn absence_invalid_numbers_and_missing_occurrence_time_are_distinct() {
        let registry = hatel_core::schema::load_core().unwrap();
        let minimal = json!([
            {"key":"session.id","value":{"stringValue":"session"}},
            {"key":"event.timestamp","value":{"stringValue":"2026-01-01T00:00:00Z"}}
        ]);
        let record = records(&event(minimal.clone()), &registry)
            .unwrap()
            .remove(0);
        assert!(!record.payload.contains_key("cost_usd"));
        let mut invalid = minimal.clone();
        invalid
            .as_array_mut()
            .unwrap()
            .push(json!({"key":"cost_usd","value":{"stringValue":"oops"}}));
        assert!(records(&event(invalid), &registry).is_err());
        let no_time = json!([{"key":"session.id","value":{"stringValue":"session"}}]);
        assert!(records(&event(no_time), &registry).is_err());
    }
}
