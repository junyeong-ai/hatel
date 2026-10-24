use std::io::Write;
use std::process::{Command, Stdio};

fn isolated(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hatel"));
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("HATEL_")
        {
            command.env_remove(key);
        }
    }
    command.env("HATEL_CONFIG", home.join("config.toml"));
    command
}

#[test]
fn a_store_this_build_did_not_create_is_reported_and_left_unchanged() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[storage]\nstate_dir={:?}\n", state.to_string_lossy()),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(state.join("telemetry.db")).unwrap();
    conn.execute_batch("CREATE TABLE records(ts TEXT NOT NULL,kind TEXT NOT NULL,schema_version INTEGER NOT NULL,payload TEXT NOT NULL); INSERT INTO records VALUES ('2026-01-01T00:00:00Z','prompt',1,'{}');").unwrap();
    drop(conn);
    let doctor = isolated(home.path())
        .args(["doctor", "--local", "--json"])
        .output()
        .unwrap();
    assert_eq!(doctor.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("not one this build created"));
    let emitted = isolated(home.path())
        .args(["emit", "check", "name=lint", "outcome=passed"])
        .output()
        .unwrap();
    assert_eq!(emitted.status.code(), Some(1));
    let conn = rusqlite::Connection::open(state.join("telemetry.db")).unwrap();
    let columns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('records')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 4, "a refused store is never altered");
}

#[test]
fn incompatible_contracts_are_excluded_and_selected_history_uses_its_saved_definition() {
    let home = tempfile::tempdir().unwrap();
    let plugin = home.path().join("plugin.toml");
    let config = home.path().join("config.toml");
    let storage = format!(
        "[storage]\nstate_dir={:?}\n",
        home.path().join("state").to_string_lossy()
    );
    std::fs::write(
        &config,
        format!("plugins=[{:?}]\n{storage}", plugin.to_string_lossy()),
    )
    .unwrap();
    let schema = |version, unit| {
        format!(
            "[[kind]]\nname='sample.duration'\nversion={version}\nfields={{name={{type='string',required=true}},duration={{type='number',unit='{unit}'}}}}\ngroup_key='name'\nmeasures=['duration']\n"
        )
    };
    let mut ids = Vec::new();
    for (version, unit, value) in [(1, "ms", "duration:=1000"), (2, "s", "duration:=1")] {
        std::fs::write(&plugin, schema(version, unit)).unwrap();
        let emitted = isolated(home.path())
            .args(["emit", "sample.duration", "name=lint", value, "--receipt"])
            .output()
            .unwrap();
        assert!(
            emitted.status.success(),
            "{}",
            String::from_utf8_lossy(&emitted.stderr)
        );
        let inventory = isolated(home.path())
            .args(["kinds", "--json"])
            .output()
            .unwrap();
        assert!(inventory.status.success());
        let value: serde_json::Value = serde_json::from_slice(&inventory.stdout).unwrap();
        let spec = value["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == "sample.duration")
            .unwrap();
        ids.push(spec["schema_id"].as_str().unwrap().to_string());
        assert_eq!(value["stored_contracts"].as_array().unwrap().len(), version);
    }
    let current = isolated(home.path())
        .args(["report", "--kind", "sample.duration", "--format", "json"])
        .output()
        .unwrap();
    assert!(current.status.success());
    let value: serde_json::Value = serde_json::from_slice(&current.stdout).unwrap();
    let section = &value["kinds"][0];
    assert!(section["error"].is_null());
    assert_eq!(section["contract"], ids[1].as_str());
    assert_eq!(section["groups"][0]["sums"][0]["sum"], 1.0);
    let resolutions: Vec<_> = section["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["schema_id"].as_str().unwrap(),
                c["resolution"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(resolutions.contains(&(ids[0].as_str(), "incompatible")));
    assert!(resolutions.contains(&(ids[1].as_str(), "selected")));
    std::fs::remove_file(&plugin).unwrap();
    std::fs::write(&config, storage).unwrap();
    for ((id, unit), expected) in ids.iter().zip(["ms", "s"]).zip([1000.0, 1.0]) {
        let selected = isolated(home.path())
            .args([
                "report",
                "--kind",
                "sample.duration",
                "--contract",
                id,
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            selected.status.success(),
            "{}",
            String::from_utf8_lossy(&selected.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&selected.stdout).unwrap();
        assert!(value["kinds"][0]["error"].is_null());
        assert_eq!(value["kinds"][0]["contract"], *id);
        assert_eq!(value["kinds"][0]["groups"][0]["sums"][0]["unit"], unit);
        assert_eq!(value["kinds"][0]["groups"][0]["sums"][0]["sum"], expected);
    }
}

#[test]
fn records_without_a_known_definition_are_counted_and_never_aggregated() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[storage]\nstate_dir={:?}\n", state.to_string_lossy()),
    )
    .unwrap();
    let database = state.join("telemetry.db");
    let registry = hatel_core::schema::load_core().unwrap();
    drop(hatel_core::SqliteSink::open(database.clone(), &registry));
    let insert = |metadata: serde_json::Value| {
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute(
                "INSERT INTO records VALUES (?1,'prompt',1,'{\"prompt_len\":5}',?2,?3,0)",
                rusqlite::params![
                    hatel_core::now_iso_utc(),
                    metadata.to_string(),
                    hatel_core::now_epoch()
                ],
            )
            .unwrap();
    };
    insert(serde_json::json!({}));
    insert(serde_json::json!({"schema_id":"unknown-contract","kind_version":1}));
    insert(serde_json::json!({"schema_id":registry.kind("prompt").unwrap().schema_id}));
    let report = isolated(home.path())
        .args(["report", "--kind", "prompt", "--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success());
    let value: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let section = &value["kinds"][0];
    assert_eq!(section["groups"][0]["count"], 1);
    let mut resolutions: Vec<_> = section["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["resolution"].as_str().unwrap(),
                c["count"].as_i64().unwrap(),
            )
        })
        .collect();
    resolutions.sort();
    assert_eq!(
        resolutions,
        [("selected", 1), ("undefined", 1), ("undefined", 1)]
    );
    let interpreted = isolated(home.path())
        .args([
            "report",
            "--kind",
            "prompt",
            "--contract",
            "unknown-contract",
        ])
        .output()
        .unwrap();
    assert!(
        !interpreted.status.success(),
        "a contract without a stored definition is never interpreted"
    );
}

#[test]
fn a_compatible_schema_change_answers_with_earlier_records() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let plugin = home.path().join("plugin.toml");
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "plugins=[{:?}]\n[storage]\nstate_dir={:?}\n",
            plugin.to_string_lossy(),
            state.to_string_lossy()
        ),
    )
    .unwrap();
    let kind = "[[kind]]\nname='team.deploy'\nfields={service={type='string',required=true},duration_s={type='number',unit='s'}}\ngroup_key='service'\nmeasures=['duration_s']\n";
    std::fs::write(&plugin, kind).unwrap();
    let emit = |duration: &str| {
        let output = isolated(home.path())
            .args(["emit", "team.deploy", "service=api", duration])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    emit("duration_s:=1");
    emit("duration_s:=2");
    std::fs::write(
        &plugin,
        kind.replace("unit='s'}", "unit='s'},outcome={type='string'}"),
    )
    .unwrap();
    emit("duration_s:=4");
    let output = isolated(home.path())
        .args(["report", "--kind", "team.deploy", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let section = &value["kinds"][0];
    assert_eq!(section["groups"][0]["count"], 3);
    assert_eq!(section["groups"][0]["sums"][0]["sum"], 7.0);
    let mut resolutions: Vec<_> = section["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["resolution"].as_str().unwrap())
        .collect();
    resolutions.sort();
    assert_eq!(resolutions, ["compatible", "selected"]);
}

#[test]
fn read_errors_are_machine_visible_and_do_not_disable_independent_recording() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[storage]\nstate_dir = {:?}\n", state.to_string_lossy()),
    )
    .unwrap();
    std::fs::write(state.join("telemetry.db"), "invalid database").unwrap();
    let inventory = isolated(home.path())
        .args(["kinds", "--json"])
        .output()
        .unwrap();
    assert_eq!(inventory.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&inventory.stdout).unwrap();
    assert!(value["storage_error"].is_string());
    std::fs::remove_file(state.join("telemetry.db")).unwrap();
    let snapshot = state.join("cost_snapshot.jsonl");
    std::fs::create_dir(&snapshot).unwrap();
    let report = isolated(home.path())
        .args(["report", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(report.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert!(value["cost_error"].is_string());
    assert_eq!(value["cost"], serde_json::json!([]));
    let record = isolated(home.path())
        .args(["emit", "check", "name=lint", "outcome=passed", "--receipt"])
        .output()
        .unwrap();
    assert!(
        record.status.success(),
        "{}",
        String::from_utf8_lossy(&record.stderr)
    );
    let scoped = isolated(home.path())
        .args(["report", "--kind", "check", "--format", "json"])
        .output()
        .unwrap();
    assert!(scoped.status.success());
    let value: serde_json::Value = serde_json::from_slice(&scoped.stdout).unwrap();
    assert!(value["cost_error"].is_null());
    assert_eq!(value["kinds"][0]["groups"][0]["count"], 1);
    assert!(snapshot.is_dir());
}

#[test]
fn the_cost_section_follows_top_and_states_its_totals() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("[storage]\nstate_dir = {:?}\n", state.to_string_lossy()),
    )
    .unwrap();
    let ts = hatel_core::now_iso_utc();
    let rows: String = [("s1", "alpha", 1.0), ("s2", "beta", 5.0), ("s3", "alpha", 9.0)]
        .into_iter()
        .map(|(session, project, cost)| {
            format!(
                "{}\n",
                serde_json::json!({"session_id": session, "project": project, "tokens": 1, "cost_usd": cost, "active_time_s": 1.0, "lines": 0, "observed_metrics": ["cost.usage"], "ts": ts})
            )
        })
        .collect();
    std::fs::write(state.join("cost_snapshot.jsonl"), rows).unwrap();
    let report = |top: &str| -> serde_json::Value {
        let output = isolated(home.path())
            .args(["report", "--top", top, "--format", "json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let top = report("1");
    assert_eq!(top["cost"].as_array().unwrap().len(), 1);
    assert_eq!(top["cost"][0]["session_id"], "s3");
    assert_eq!(top["cost_session_count"], 3);
    assert_eq!(top["cost_groups"][0]["key"], "alpha");
    assert_eq!(top["cost_groups"][0]["sums"][1]["sum"], 10.0);
    assert_eq!(top["cost_group_count"], 2);
    let all = report("0");
    assert_eq!(all["cost"].as_array().unwrap().len(), 3);
    assert_eq!(all["cost_groups"].as_array().unwrap().len(), 2);
}

#[test]
fn record_commands_reject_oversized_stdin_before_storing_records() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nstate_dir = {:?}\n",
            home.path().join("state").to_string_lossy()
        ),
    )
    .unwrap();
    for (args, json) in [
        (vec!["emit", "prompt", "--receipt"], "{}"),
        (vec!["emit-batch"], r#"[{"kind":"prompt","payload":{}}]"#),
    ] {
        let mut input = vec![b' '; 8 * 1024 * 1024 + 1];
        input.extend_from_slice(json.as_bytes());
        let mut child = isolated(home.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Err(error) = child.stdin.take().unwrap().write_all(&input) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("input exceeds 8 MiB")
        );
        assert!(!home.path().join("state/telemetry.db").exists());
    }
}

#[test]
fn batch_returns_each_storage_outcome_and_only_stored_rows_are_queryable() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nstate_dir = {:?}\n",
            home.path().join("state").to_string_lossy()
        ),
    )
    .unwrap();
    let mut child = isolated(home.path())
        .arg("emit-batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"[
        {"kind":"tool","payload":{"tool_name":"Bash","duration_ms":0}},
        {"kind":"tool","payload":{"tool_name":"Bash","duration_ms":"oops"}},
        {"kind":"prompt","payload":{},"context":{"occurred_at":"invalid"}},
        {"kind":"unknown","payload":{}}
    ]"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows[0]["status"], "stored");
    assert!(rows[0]["receipt"]["record_id"].is_string());
    for i in 1..4 {
        assert_eq!(rows[i]["status"], "rejected");
    }
    let report = isolated(home.path())
        .args(["report", "--kind", "tool", "--format", "json"])
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["kinds"][0]["groups"][0]["count"], 1);
    std::fs::write(home.path().join("occupied"), "file").unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nstate_dir = {:?}\n",
            home.path().join("occupied").to_string_lossy()
        ),
    )
    .unwrap();
    let failure = isolated(home.path())
        .args(["emit", "prompt", "--json", "{}", "--receipt"])
        .output()
        .unwrap();
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
}

struct Receiver(std::process::Child);
impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_waits_without_a_receiver_and_delivery_resumes_after_restart() {
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = axum::Router::new().route(
        "/v1/logs",
        axum::routing::post(move |body: bytes::Bytes| {
            let sent = sent.clone();
            async move {
                sent.send(serde_json::from_slice::<serde_json::Value>(&body).unwrap())
                    .unwrap();
                "{}"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"),format!("[storage]\nstate_dir={:?}\n[[export]]\nendpoint={endpoint:?}\nmode='raw'\nsignals=[]\nkinds=['prompt']\n",home.path().join("state").to_string_lossy())).unwrap();
    for _ in 0..2 {
        let receipt = isolated(home.path())
            .args(["emit", "prompt", "--json", "{}", "--receipt"])
            .output()
            .unwrap();
        assert!(
            receipt.status.success(),
            "{}",
            String::from_utf8_lossy(&receipt.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&receipt.stdout).unwrap();
        assert_eq!(receipt["delivery"]["queued"], 1);
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port().to_string();
        drop(reservation);
        let receiver = Receiver(
            isolated(home.path())
                .args(["serve", "--all", "--port", &port])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let body = tokio::time::timeout(std::time::Duration::from_secs(8), received.recv())
            .await
            .unwrap()
            .unwrap();
        let attributes = body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"]
            .as_array()
            .unwrap();
        assert!(
            attributes.iter().any(|a| a["key"] == "log.record.uid"
                && a["value"]["stringValue"] == receipt["record_id"])
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let states = isolated(home.path())
                    .args(["deliveries", "--json"])
                    .output()
                    .unwrap();
                let states: Vec<serde_json::Value> =
                    serde_json::from_slice(&states.stdout).unwrap();
                if states.iter().all(|s| s["status"] == "delivered") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(receiver);
    }
    server.abort();
}

#[test]
fn local_diagnosis_does_not_require_claude_code_settings() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nstate_dir={:?}\n",
            home.path().join("state").to_string_lossy()
        ),
    )
    .unwrap();
    let output = isolated(home.path())
        .args(["doctor", "--local", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert!(value["settings_files"].as_array().unwrap().is_empty());
    assert!(
        value["sections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "hooks" && s["name"] != "native_telemetry")
    );
}

#[test]
fn a_redacted_payload_project_does_not_change_the_canonical_metadata_filter() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        home.path().join("plugin.toml"),
        "[[kind]]\nname='private'\nfields={ project = { type = 'any' } }\ngroup_key='project'\nredact=['project']\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "plugins=['plugin.toml']\n[storage]\nstate_dir={:?}\n",
            home.path().join("state").to_string_lossy()
        ),
    )
    .unwrap();
    let write = isolated(home.path())
        .current_dir(&repo)
        .args(["emit", "private", "project=repo"])
        .output()
        .unwrap();
    assert!(write.status.success());
    let query = |filter: Option<&str>| {
        let mut command = isolated(home.path());
        command.args(["report", "--kind", "private", "--format", "json"]);
        if let Some(filter) = filter {
            command.args(["--filter", filter]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let all = query(None);
    let key = all["kinds"][0]["groups"][0]["key"].as_str().unwrap();
    assert_eq!(key, repo.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(
        query(Some(&format!("project={key}")))["kinds"][0]["groups"][0]["count"],
        1
    );
    assert!(
        query(Some("project=/other/repo"))["kinds"][0]["groups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
