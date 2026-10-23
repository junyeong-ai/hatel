//! The local-record outbox consumer. A separate task per destination isolates network failures.

use hatel_core::{
    Envelope, ExportConfig, ExportTarget, Settings,
    outbox::{self, Outcome},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn value(value: &Value) -> Result<Value, String> {
    Ok(match value {
        Value::String(s) => json!({"stringValue":s}),
        Value::Bool(b) => json!({"boolValue":b}),
        Value::Number(n) if n.is_i64() => json!({"intValue":n.to_string()}),
        Value::Number(n) if n.is_u64() => {
            return Err("OTLP intValue requires a signed 64-bit integer".into());
        }
        Value::Number(n) => json!({"doubleValue":n}),
        Value::Array(items) => {
            json!({"arrayValue":{"values":items.iter().map(self::value).collect::<Result<Vec<_>,_>>()?}})
        }
        Value::Object(fields) => {
            json!({"kvlistValue":{"values":fields.iter().map(|(k,v)| Ok(json!({"key":k,"value":self::value(v)?}))).collect::<Result<Vec<Value>,String>>()?}})
        }
        Value::Null => json!({}),
    })
}

fn nanos(ts: &str) -> Result<String, String> {
    let timestamp = ts
        .parse::<jiff::Timestamp>()
        .map_err(|_| "invalid OTLP timestamp")?;
    u64::try_from(timestamp.as_nanosecond())
        .ok()
        .filter(|value| *value != 0)
        .map(|value| value.to_string())
        .ok_or_else(|| "OTLP timestamp must be a known positive 64-bit nanosecond value".into())
}

fn log(env: &Envelope, redact_project: bool) -> Result<Value, String> {
    let project_label = env.metadata.project.as_ref().map(|p| {
        if redact_project {
            hatel_core::pii::redacted(&p.label)
        } else {
            p.label.clone()
        }
    });
    let mut attributes = Vec::new();
    for (key, item) in [
        ("log.record.uid", env.metadata.record_id.as_deref()),
        ("hatel.source", env.metadata.source.as_deref()),
        ("hatel.run.id", env.metadata.run_id.as_deref()),
        (
            "hatel.correlation.id",
            env.metadata.correlation_id.as_deref(),
        ),
        ("project.label", project_label.as_deref()),
    ] {
        if let Some(item) = item {
            attributes.push(json!({"key":key,"value":{"stringValue":item}}));
        }
    }
    attributes.push(json!({"key":"hatel.kind.version","value":{"intValue":env.metadata.kind_version.unwrap_or(1).to_string()}}));
    let mut record = json!({"eventName":env.kind,"body":value(&serde_json::to_value(&env.payload).expect("payload serializes"))?,"attributes":attributes,"timeUnixNano":nanos(&env.ts)?});
    if let Some(timestamp) = env.metadata.observed_at.as_deref() {
        record["observedTimeUnixNano"] = nanos(timestamp)?.into();
    }
    Ok(record)
}

fn body(logs: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({"resourceLogs":[{
        "resource":{"attributes":[{"key":"service.name","value":{"stringValue":"hatel"}}]},
        "scopeLogs":[{"scope":{"name":"hatel","version":env!("CARGO_PKG_VERSION")},"logRecords":logs}]
    }]})).expect("OTLP logs serialize")
}

struct Policy {
    export: ExportConfig,
    registry: hatel_core::Registry,
}

impl Policy {
    fn blocked(
        &self,
        target: &ExportTarget,
        pending: &outbox::Pending,
        now: i64,
    ) -> Option<Outcome<'static>> {
        if !self.schema_matches(&pending.record) {
            Some(Outcome::Cancelled(
                "record schema changed; explicit migration is required",
            ))
        } else if !target.allows_record(&pending.record) {
            Some(Outcome::Cancelled(
                "current kind or project policy excludes record",
            ))
        } else if now.saturating_sub(pending.created_at) >= i64::from(target.queue_days) * 86400 {
            Some(Outcome::Expired)
        } else if pending.attempts >= target.retry_attempts {
            Some(Outcome::FailedBeforeSend("maximum attempts reached"))
        } else {
            None
        }
    }

    fn schema_matches(&self, record: &Envelope) -> bool {
        self.registry.kind(&record.kind).is_some_and(|spec| {
            record.metadata.schema_id.as_deref() == Some(spec.schema_id.as_str())
        })
    }
}

fn policy(path: &Path, state: &Path) -> Result<Policy, String> {
    let settings = Settings::read(path).map_err(|e| e.to_string())?;
    let (cfg, invalid) = hatel_core::Config::from_settings(&settings);
    if !invalid.is_empty() || cfg.state_dir != state || cfg.sink != hatel_core::SinkKind::Sqlite {
        return Err("running store differs from current configuration".into());
    }
    let registry = hatel_core::schema::build_registry(&cfg).map_err(|e| e.to_string())?;
    Ok(Policy {
        registry,
        export: ExportConfig::from_settings(&settings).map_err(|e| e.to_string())?,
    })
}

enum Delivery {
    Accepted,
    Retry { error: String, after: i64 },
    Failed(String),
}

async fn send(client: &reqwest::Client, target: &ExportTarget, payload: Vec<u8>) -> Delivery {
    let mut request = client
        .post(format!("{}/v1/logs", target.endpoint))
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(target.timeout_ms.unwrap_or(5000)))
        .body(payload);
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
        if !partial.is_object() {
            return Delivery::Failed("invalid OTLP partial acknowledgement".into());
        }
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
    let conn = rusqlite_connection(state)?;
    let now = hatel_core::now_epoch();
    let pending = outbox::due(&conn, destination, now).map_err(|e| e.to_string())?;
    drop(conn);
    if pending.is_empty() {
        return Ok(());
    }
    let current = policy(path, state)?;
    let conn = rusqlite_connection(state)?;
    let Some(target) = current
        .export
        .targets
        .iter()
        .find(|target| target.endpoint == destination && !target.kinds.is_empty())
    else {
        return outbox::cancel_destination(&conn, destination, now).map_err(|e| e.to_string());
    };
    let mut records = Vec::new();
    let mut logs = Vec::new();
    for pending in pending {
        let id = pending
            .record
            .metadata
            .record_id
            .as_deref()
            .ok_or("record has no id")?;
        let outcome = current.blocked(target, &pending, now);
        if let Some(outcome) = outcome {
            outbox::finish(&conn, destination, id, now, outcome).map_err(|e| e.to_string())?;
            continue;
        }
        match log(
            &pending.record,
            current
                .registry
                .kind(&pending.record.kind)
                .is_some_and(|spec| spec.redact.contains("project")),
        ) {
            Ok(log) => {
                records.push(pending);
                logs.push(log);
            }
            Err(error) => outbox::finish(
                &conn,
                destination,
                id,
                now,
                Outcome::FailedBeforeSend(&error),
            )
            .map_err(|e| e.to_string())?,
        }
    }
    drop(conn);
    if records.is_empty() {
        return Ok(());
    }
    let latest = policy(path, state)?;
    let now = hatel_core::now_epoch();
    let Some(target) = latest.export.targets.iter().find(|target| {
        target.endpoint == destination
            && records
                .iter()
                .all(|record| latest.blocked(target, record, now).is_none())
    }) else {
        return Ok(());
    };
    let delivery = send(client, target, body(logs)).await;
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

    #[test]
    fn wire_numbers_and_timestamps_preserve_representable_boundaries() {
        assert_eq!(
            value(&json!(i64::MIN)).unwrap(),
            json!({"intValue":i64::MIN.to_string()})
        );
        assert_eq!(
            value(&json!(i64::MAX)).unwrap(),
            json!({"intValue":i64::MAX.to_string()})
        );
        assert!(value(&json!({"nested":[u64::MAX]})).is_err());
        assert_eq!(
            value(&json!({"nested":[null,0]})).unwrap(),
            json!({"kvlistValue":{"values":[{"key":"nested","value":{"arrayValue":{"values":[{}, {"intValue":"0"}]}}}]}})
        );
        for (timestamp, accepted) in [
            (0, false),
            (1, true),
            (i128::from(u64::MAX), true),
            (-1, false),
            (i128::from(u64::MAX) + 1, false),
        ] {
            let timestamp = jiff::Timestamp::from_nanosecond(timestamp)
                .unwrap()
                .to_string();
            assert_eq!(nanos(&timestamp).is_ok(), accepted);
        }
    }

    #[tokio::test]
    async fn unsendable_records_and_exhausted_policy_do_not_count_as_http_attempts() {
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = received.clone();
        let app = axum::Router::new().route(
            "/v1/logs",
            axum::routing::post(move |bytes: bytes::Bytes| {
                let requests = requests.clone();
                async move {
                    let body: Value = serde_json::from_slice(&bytes).unwrap();
                    let valid = body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|record| {
                            record["body"]["kvlistValue"]["values"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter(|field| field["key"] == "prompt_len")
                                .all(|field| {
                                    field["value"]["intValue"]
                                        .as_str()
                                        .is_some_and(|value| value.parse::<i64>().is_ok())
                                })
                        });
                    requests.lock().unwrap().push(body);
                    (
                        if valid {
                            axum::http::StatusCode::OK
                        } else {
                            axum::http::StatusCode::BAD_REQUEST
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
        let registry = hatel_core::schema::build_registry(&cfg).unwrap();
        let valid = hatel_core::make_envelope(
            "prompt",
            [("prompt_len".into(), json!(7))].into(),
            &registry,
            true,
        )
        .unwrap();
        let invalid = hatel_core::make_envelope(
            "prompt",
            [("prompt_len".into(), json!(u64::MAX))].into(),
            &registry,
            true,
        )
        .unwrap();
        for record in [&invalid, &valid] {
            hatel_core::build_sink(&cfg).write_record(record).unwrap();
        }
        let client = reqwest::Client::new();
        deliver(&path, home.path(), &endpoint, &client)
            .await
            .unwrap();
        let state = |id: &str| {
            rusqlite_connection(home.path())
                .unwrap()
                .query_row(
                    "SELECT status,attempts FROM deliveries WHERE record_id=?1",
                    [id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )
                .unwrap()
        };
        assert_eq!(
            state(invalid.metadata.record_id.as_deref().unwrap()),
            ("failed".into(), 0)
        );
        assert_eq!(
            state(valid.metadata.record_id.as_deref().unwrap()),
            ("delivered".into(), 1)
        );
        {
            let requests = received.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0]["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        let exhausted =
            hatel_core::make_envelope("prompt", Default::default(), &registry, true).unwrap();
        hatel_core::build_sink(&cfg)
            .write_record(&exhausted)
            .unwrap();
        let conn = rusqlite_connection(home.path()).unwrap();
        conn.execute(
            "UPDATE deliveries SET attempts=5 WHERE record_id=?1",
            [exhausted.metadata.record_id.as_deref().unwrap()],
        )
        .unwrap();
        drop(conn);
        configure(home.path(), &endpoint, "retry_attempts=1\n");
        deliver(&path, home.path(), &endpoint, &client)
            .await
            .unwrap();
        assert_eq!(
            state(exhausted.metadata.record_id.as_deref().unwrap()),
            ("failed".into(), 5)
        );
        assert_eq!(received.lock().unwrap().len(), 1);
        let failures = outbox::states(&home.path().join("telemetry.db"))
            .unwrap()
            .into_iter()
            .filter(|state| state.status == "failed")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert!(failures.iter().any(|state| state.error.as_deref()
            == Some("maximum attempts reached")
            && state.records == 1));
        assert!(failures.iter().any(|state| state.error.as_deref()
            == Some("OTLP intValue requires a signed 64-bit integer")
            && state.records == 1));
        server.abort();
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
    #[tokio::test]
    async fn a_changed_redaction_contract_cancels_pending_data_even_without_a_version_bump() {
        let home = tempfile::tempdir().unwrap();
        let plugin = home.path().join("plugin.toml");
        std::fs::write(
            &plugin,
            "[[kind]]\nname='secret'\nfields=['label','secret']\ngroup_key='label'\n",
        )
        .unwrap();
        let path = home.path().join("config.toml");
        std::fs::write(&path,format!("plugins=[{plugin:?}]\n[storage]\nsink='sqlite'\nstate_dir={:?}\n[[export]]\nendpoint='http://127.0.0.1:1'\nmode='raw'\nsignals=[]\nkinds=['secret']\n",home.path().to_string_lossy())).unwrap();
        let settings = Settings::read(&path).unwrap();
        let (cfg, _) = hatel_core::Config::from_settings_in(&settings, &|_| None);
        let registry = hatel_core::schema::build_registry(&cfg).unwrap();
        let env = hatel_core::make_envelope(
            "secret",
            [
                ("label".into(), json!("check")),
                ("secret".into(), json!("private")),
            ]
            .into(),
            &registry,
            true,
        )
        .unwrap();
        hatel_core::build_sink(&cfg).write_record(&env).unwrap();
        std::fs::write(&plugin,"[[kind]]\nname='secret'\nfields=['label','secret']\ngroup_key='label'\nredact=['secret']\n").unwrap();
        deliver(
            &path,
            home.path(),
            "http://127.0.0.1:1",
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            outbox::states(&home.path().join("telemetry.db")).unwrap()[0].status,
            "cancelled"
        );
    }
}
