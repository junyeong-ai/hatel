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
