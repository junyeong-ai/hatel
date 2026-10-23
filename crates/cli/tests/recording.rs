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
fn read_errors_are_machine_visible_and_do_not_disable_independent_recording() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nsink = \"sqlite\"\nstate_dir = {:?}\n",
            state.to_string_lossy()
        ),
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
fn record_commands_reject_oversized_stdin_before_storing_records() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[storage]\nsink = \"sqlite\"\nstate_dir = {:?}\n",
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
            "[storage]\nsink = \"sqlite\"\nstate_dir = {:?}\n",
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
            "[storage]\nsink = \"sqlite\"\nstate_dir = {:?}\n",
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
    std::fs::write(home.path().join("config.toml"),format!("[storage]\nsink='sqlite'\nstate_dir={:?}\n[[export]]\nendpoint={endpoint:?}\nmode='raw'\nsignals=[]\nkinds=['prompt']\n",home.path().join("state").to_string_lossy())).unwrap();
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
        "[[kind]]\nname='private'\nfields=['project']\ngroup_key='project'\nredact=['project']\n",
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
