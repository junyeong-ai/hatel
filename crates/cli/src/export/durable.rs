//! The local-record outbox consumer. A separate task per destination isolates network failures.

use hatel_core::{
    Envelope, ExportConfig, ExportTarget, Settings,
    outbox::{self, Outcome},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn value(value: &Value) -> Value {
    match value {
        Value::String(s) => json!({"stringValue":s}),
        Value::Bool(b) => json!({"boolValue":b}),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue":n.to_string()}),
        Value::Number(n) => json!({"doubleValue":n}),
        Value::Array(items) => {
            json!({"arrayValue":{"values":items.iter().map(self::value).collect::<Vec<_>>()}})
        }
        Value::Object(fields) => {
            json!({"kvlistValue":{"values":fields.iter().map(|(k,v)| json!({"key":k,"value":self::value(v)})).collect::<Vec<_>>()}})
        }
        Value::Null => json!({}),
    }
}

fn nanos(ts: &str) -> Option<String> {
    ts.parse::<jiff::Timestamp>()
        .ok()
        .and_then(|t| (t.as_nanosecond() >= 0).then(|| t.as_nanosecond().to_string()))
}

fn log(env: &Envelope) -> Value {
    let mut attributes = Vec::new();
    for (key, item) in [
        ("log.record.uid", env.metadata.record_id.as_deref()),
        ("hatel.source", env.metadata.source.as_deref()),
        ("hatel.run.id", env.metadata.run_id.as_deref()),
        (
            "hatel.correlation.id",
            env.metadata.correlation_id.as_deref(),
        ),
        (
            "project.label",
            env.metadata.project.as_ref().map(|p| p.label.as_str()),
        ),
    ] {
        if let Some(item) = item {
            attributes.push(json!({"key":key,"value":{"stringValue":item}}));
        }
    }
    attributes.push(json!({"key":"hatel.kind.version","value":{"intValue":env.metadata.kind_version.unwrap_or(1).to_string()}}));
    let mut record = json!({"eventName":env.kind,"body":value(&serde_json::to_value(&env.payload).expect("payload serializes")),"attributes":attributes});
    if let Some(ts) = nanos(&env.ts) {
        record["timeUnixNano"] = ts.into();
    }
    if let Some(ts) = env.metadata.observed_at.as_deref().and_then(nanos) {
        record["observedTimeUnixNano"] = ts.into();
    }
    record
}

fn body(records: &[outbox::Pending]) -> Vec<u8> {
    serde_json::to_vec(&json!({"resourceLogs":[{
        "resource":{"attributes":[{"key":"service.name","value":{"stringValue":"hatel"}}]},
        "scopeLogs":[{"scope":{"name":"hatel","version":env!("CARGO_PKG_VERSION")},"logRecords":records.iter().map(|p| log(&p.record)).collect::<Vec<_>>() }]
    }]})).expect("OTLP logs serialize")
}

fn policy(path: &Path, state: &Path) -> Result<ExportConfig, String> {
    let settings = Settings::read(path).map_err(|e| e.to_string())?;
    let (cfg, invalid) = hatel_core::Config::from_settings(&settings);
    if !invalid.is_empty() || cfg.state_dir != state || cfg.sink != hatel_core::SinkKind::Sqlite {
        return Err("running store differs from current configuration".into());
    }
    hatel_core::schema::build_registry(&cfg).map_err(|e| e.to_string())?;
    ExportConfig::from_settings(&settings).map_err(|e| e.to_string())
}

enum Delivery {
    Accepted,
    Retry { error: String, after: i64 },
    Failed(String),
}

async fn send(
    client: &reqwest::Client,
    target: &ExportTarget,
    records: &[outbox::Pending],
) -> Delivery {
    let mut request = client
        .post(format!("{}/v1/logs", target.endpoint))
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(target.timeout_ms.unwrap_or(5000)))
        .body(body(records));
    for (key, value) in &target.headers {
        request = request.header(key, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return Delivery::Retry {
                error: "network failure or timeout; acknowledgement unknown".into(),
                after: 0,
            };
        }
    };
    let status = response.status().as_u16();
    let after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.parse::<i64>().ok().filter(|s| *s >= 0).or_else(|| {
                httpdate::parse_http_date(s).ok().map(|when| {
                    when.duration_since(std::time::SystemTime::now())
                        .unwrap_or_default()
                        .as_secs()
                        .min(i64::MAX as u64) as i64
                })
            })
        })
        .unwrap_or(0);
    if [429, 502, 503, 504].contains(&status) {
        return Delivery::Retry {
            error: format!("HTTP {status}"),
            after,
        };
    }
    if status != 200 {
        return Delivery::Failed(format!("HTTP {status}"));
    }
    if response.content_length().is_some_and(|n| n > 64 * 1024) {
        return Delivery::Failed("OTLP acknowledgement exceeds 64 KiB".into());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= 64 * 1024 => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => return Delivery::Failed("OTLP acknowledgement exceeds 64 KiB".into()),
            Ok(None) => break,
            Err(_) => {
                return Delivery::Retry {
                    error: "acknowledgement incomplete".into(),
                    after: 0,
                };
            }
        }
    }
    let ack: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Delivery::Failed("invalid OTLP JSON acknowledgement".into()),
    };
    if !ack.is_object() {
        return Delivery::Failed("invalid OTLP JSON acknowledgement".into());
    }
    if let Some(partial) = ack
        .get("partialSuccess")
        .or_else(|| ack.get("partial_success"))
    {
        let rejected = partial
            .get("rejectedLogRecords")
            .or_else(|| partial.get("rejected_log_records"));
        let rejected = match rejected {
            None => Some(0),
            Some(Value::String(n)) => n.parse::<u64>().ok(),
            Some(n) => n.as_u64(),
        };
        match rejected {
            Some(0) => {}
            Some(count) => {
                return Delivery::Failed(format!(
                    "OTLP partial rejection: {count}; batch not retried"
                ));
            }
            None => return Delivery::Failed("invalid OTLP partial acknowledgement".into()),
        }
    }
    Delivery::Accepted
}

pub(crate) async fn deliver(
    path: &Path,
    state: &Path,
    destination: &str,
    client: &reqwest::Client,
) -> Result<(), String> {
    let current = policy(path, state)?;
    let conn = rusqlite_connection(state)?;
    let Some(target) = current
        .targets
        .iter()
        .find(|t| t.endpoint == destination && !t.kinds.is_empty())
    else {
        return outbox::cancel_destination(&conn, destination, hatel_core::now_epoch())
            .map_err(|e| e.to_string());
    };
    let now = hatel_core::now_epoch();
    let mut records = Vec::new();
    for pending in outbox::due(&conn, destination, now).map_err(|e| e.to_string())? {
        let id = pending
            .record
            .metadata
            .record_id
            .as_deref()
            .ok_or("record has no id")?;
        let outcome = if !target.allows_record(&pending.record) {
            Some(Outcome::Cancelled(
                "current kind or project policy excludes record",
            ))
        } else if now.saturating_sub(pending.created_at) >= i64::from(target.queue_days) * 86400 {
            Some(Outcome::Expired)
        } else if pending.attempts >= target.retry_attempts {
            Some(Outcome::Failed("maximum attempts reached"))
        } else {
            None
        };
        if let Some(outcome) = outcome {
            outbox::finish(&conn, destination, id, now, outcome).map_err(|e| e.to_string())?;
        } else {
            records.push(pending);
        }
    }
    drop(conn);
    if records.is_empty() {
        return Ok(());
    }
    // Policy is checked after loading the batch as well; an unreadable policy never authorizes egress.
    let latest = policy(path, state)?;
    let Some(target) = latest
        .targets
        .iter()
        .find(|t| t.endpoint == destination && records.iter().all(|p| t.allows_record(&p.record)))
    else {
        return Ok(());
    };
    let delivery = send(client, target, &records).await;
    let conn = rusqlite_connection(state)?;
    let now = hatel_core::now_epoch();
    for pending in records {
        let id = pending
            .record
            .metadata
            .record_id
            .as_deref()
            .ok_or("record has no id")?;
        let outcome = match &delivery {
            Delivery::Accepted => Outcome::Delivered,
            Delivery::Failed(error) => Outcome::Failed(error),
            Delivery::Retry { error, after } if pending.attempts + 1 < target.retry_attempts => {
                let backoff = (5i64.saturating_mul(1i64 << pending.attempts.min(10))).min(3600);
                Outcome::Retry {
                    next_attempt_at: now.saturating_add(backoff.max(*after)).min(
                        pending
                            .created_at
                            .saturating_add(i64::from(target.queue_days) * 86400),
                    ),
                    error,
                }
            }
            Delivery::Retry { error, .. } => Outcome::Failed(error),
        };
        outbox::finish(&conn, destination, id, now, outcome).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn rusqlite_connection(state: &Path) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open_with_flags(
        state.join("telemetry.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_millis(100))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

pub fn spawn(
    path: PathBuf,
    state: PathBuf,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(_) => {
                eprintln!("hatel: cannot create durable delivery client");
                return;
            }
        };
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut tasks: tokio::task::JoinSet<(String, Result<(), String>)> =
            tokio::task::JoinSet::new();
        let mut active = std::collections::BTreeSet::new();
        let mut errors = std::collections::BTreeSet::new();
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                    match completed {
                        Ok((destination,result)) => {
                            active.remove(&destination);
                            if let Err(error) = result { if errors.insert(destination.clone()) { eprintln!("hatel: durable delivery paused: {error}"); } }
                            else { errors.remove(&destination); }
                        },
                        Err(error) => eprintln!("hatel: durable delivery worker failed: {error}"),
                    }
                },
                _ = interval.tick() => {
                    if !state.join("telemetry.db").exists() { continue; }
                    let destinations = rusqlite_connection(&state).and_then(|conn| outbox::destinations(&conn).map_err(|e| e.to_string()));
                    let destinations = match destinations { Ok(destinations) => destinations, Err(error) => { if errors.insert(String::new()) { eprintln!("hatel: cannot read durable deliveries: {error}"); } continue; } };
                    errors.remove("");
                    for destination in destinations {
                        if active.len() >= 32 { break; }
                        if !active.insert(destination.clone()) { continue; }
                        let path = path.clone(); let state = state.clone(); let client = client.clone();
                        tasks.spawn(async move { let result = deliver(&path,&state,&destination,&client).await; (destination,result) });
                    }
                }
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    });
    (stop, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn configure(home: &Path, endpoint: &str, extra: &str) -> (PathBuf, hatel_core::Config) {
        let path = home.join("config.toml");
        std::fs::write(&path, format!("[storage]\nsink='sqlite'\nstate_dir={:?}\n[[export]]\nendpoint={endpoint:?}\nmode='raw'\nsignals=[]\nkinds=['prompt']\n{extra}", home.to_string_lossy())).unwrap();
        let settings = Settings::read(&path).unwrap();
        let (cfg, invalid) = hatel_core::Config::from_settings_in(&settings, &|_| None);
        assert!(invalid.is_empty());
        (path, cfg)
    }

    #[tokio::test]
    async fn restart_retries_with_the_same_id_and_current_policy_controls_egress() {
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = received.clone();
        let app = axum::Router::new().route(
            "/v1/logs",
            axum::routing::post(move |body: bytes::Bytes| {
                let requests = requests.clone();
                async move {
                    let mut requests = requests.lock().unwrap();
                    requests.push(serde_json::from_slice(&body).unwrap());
                    (
                        if requests.len() == 1 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            axum::http::StatusCode::OK
                        },
                        "{}",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let home = tempfile::tempdir().unwrap();
        let (path, cfg) = configure(home.path(), &endpoint, "");
        let reg = hatel_core::schema::build_registry(&cfg).unwrap();
        let env = hatel_core::make_envelope("prompt", Default::default(), &reg, true).unwrap();
        assert_eq!(
            hatel_core::build_sink(&cfg)
                .write_record(&env)
                .unwrap()
                .delivery
                .queued,
            1
        );
        let client = reqwest::Client::new();
        deliver(&path, home.path(), &endpoint, &client)
            .await
            .unwrap();
        let conn = rusqlite_connection(home.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT status FROM deliveries", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "pending"
        );
        conn.execute("UPDATE deliveries SET next_attempt_at=0", [])
            .unwrap();
        drop(conn);
        deliver(&path, home.path(), &endpoint, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(
            outbox::states(&home.path().join("telemetry.db")).unwrap()[0].status,
            "delivered"
        );
        {
            let requests = received.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0], requests[1]);
            assert!(!requests[0].to_string().contains("project.key"));
        }
        let env = hatel_core::make_envelope("prompt", Default::default(), &reg, true).unwrap();
        hatel_core::build_sink(&cfg).write_record(&env).unwrap();
        configure(home.path(), &endpoint, "projects=['company']\n");
        deliver(&path, home.path(), &endpoint, &client)
            .await
            .unwrap();
        assert_eq!(received.lock().unwrap().len(), 2);
        assert!(
            outbox::states(&home.path().join("telemetry.db"))
                .unwrap()
                .iter()
                .any(|s| s.status == "cancelled")
        );
        server.abort();
    }
}
