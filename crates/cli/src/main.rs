//! `hatel` — the human-facing collector binary: OTLP receiver, reports,
//! doctor, and registry introspection.

mod claude_settings;
mod doctor;
mod export;
mod init;
mod otlp;
mod serve;
mod service;

use std::path::Path;

use clap::{Parser, Subcommand, ValueEnum};

use hatel_core::cost;
use hatel_core::schema::build_registry;
use hatel_core::{Config, Payload, Registry, render, report};

#[derive(Parser)]
#[command(
    name = "hatel",
    version,
    about = "Local Claude Code telemetry collector"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the OTLP/HTTP receiver and the live per-session rollup.
    Serve {
        #[arg(long, default_value_t = 4318)]
        port: u16,
        /// Show only this project's sessions (defaults to the receiver's cwd).
        #[arg(long, conflicts_with = "all")]
        project: Option<String>,
        /// Show every project's sessions.
        #[arg(long)]
        all: bool,
    },
    /// Aggregate the ledger over a rolling window.
    Report {
        /// Rolling window in days, e.g. `30d` (days only).
        #[arg(long, default_value = "30d")]
        window: String,
        #[arg(long, value_enum, default_value_t = Format::Md)]
        format: Format,
        /// Restrict to one project (its label); default is all projects.
        #[arg(long)]
        project: Option<String>,
        /// Restrict to one registered Kind (e.g. `ci_check`); default is every Kind.
        /// Scopes the rollup to that Kind and drops the native cost section.
        #[arg(long)]
        kind: Option<String>,
        /// How many groups to show per Kind (0 = all).
        #[arg(long, default_value_t = report::TOP_N)]
        top: usize,
    },
    /// Wire this collector into Claude Code's settings.json — idempotent and
    /// non-destructive (adds the telemetry env and lifecycle hooks without touching
    /// existing hooks or a repointed endpoint), so it is safe to re-run.
    Init {
        /// Which settings file to write: `user` (all projects), `project` (committed),
        /// or `local` (this repo, per-dev). Org/managed settings are print-only.
        #[arg(long, value_enum, default_value_t = init::Scope::User)]
        scope: init::Scope,
        /// Print the settings block instead of writing it (for managed/org settings).
        #[arg(long, conflicts_with = "remove")]
        print: bool,
        /// Remove this collector's wiring from the scope (leaves the native telemetry env).
        #[arg(long)]
        remove: bool,
        /// Insert hatel in front of an existing corporate OTLP endpoint: capture it as a
        /// `config.toml` export target and repoint Claude Code at the local receiver — so you keep
        /// the corporate collector and gain hatel. Operates on whichever scope sets the endpoint.
        #[arg(long, conflicts_with_all = ["remove", "print"])]
        insert: bool,
        /// The transform for the captured endpoint when `--insert`ing (default: enriched, so the
        /// corporate collector gains the project label too).
        #[arg(long, value_enum, default_value_t = init::InsertMode::Enriched, requires = "insert")]
        mode: init::InsertMode,
    },
    /// Install or remove the receiver as a background user service (launchd on macOS,
    /// systemd --user on Linux) for gap-free collection — the unit runs `serve --all`.
    Service {
        /// Remove the service instead of installing it.
        #[arg(long, conflicts_with = "print")]
        remove: bool,
        /// Print the unit file instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Verify the settings.json wiring and report policy gaps.
    Doctor,
    /// List the registered Kinds (core + plugins).
    Kinds {
        #[arg(long)]
        json: bool,
    },
    /// Record one domain signal for a registered Kind — the programmatic path for
    /// project metrics that aren't derived from a Claude Code hook (a gate decision,
    /// a check rollup, a deploy outcome). The payload is allow-list-filtered and
    /// redacted like any other record.
    ///
    /// Give fields as `key=value` (string) or `key:=value` (JSON — for numbers,
    /// bools, arrays), or pass a whole JSON object via `--json` or on stdin.
    Emit {
        /// A registered Kind name (e.g. `ci_check`).
        kind: String,
        /// `key=value` (string) or `key:=value` (JSON) field pairs.
        #[arg(value_name = "key=value")]
        fields: Vec<String>,
        /// A full JSON object payload (instead of field pairs or stdin).
        #[arg(long, conflicts_with = "fields")]
        json: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Md,
    Text,
    Json,
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    match Cli::parse().command {
        Command::Serve { port, project, all } => serve::run(port, project, all),
        Command::Report {
            window,
            format,
            project,
            kind,
            top,
        } => report_cmd(&window, format, top, project.as_deref(), kind.as_deref()),
        Command::Init {
            scope,
            print,
            remove,
            insert,
            mode,
        } => {
            if insert {
                init::insert(mode)
            } else {
                init::run(scope, print, remove)
            }
        }
        Command::Service { remove, print } => service::run(remove, print),
        Command::Doctor => doctor::run(),
        Command::Kinds { json } => kinds_cmd(json),
        Command::Emit { kind, fields, json } => emit_cmd(&kind, fields, json),
    }
}

/// Record one domain signal. Caller errors (unknown kind, malformed input, or — in
/// strict mode — disallowed keys) exit non-zero so the caller knows it wasn't
/// recorded; an IO write failure stays fail-open (the sink notes it to stderr).
fn emit_cmd(kind: &str, fields: Vec<String>, json: Option<String>) -> i32 {
    let cfg = Config::load();
    let reg = match build_registry(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("emit: {e}");
            return 1;
        }
    };
    if reg.kind(kind).is_none() {
        let known: Vec<&str> = reg.kinds().map(|s| s.name.as_str()).collect();
        eprintln!(
            "emit: unknown kind {kind:?}; registered: {}",
            known.join(", ")
        );
        return 2;
    }
    let payload = match build_payload(fields, json) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("emit: {e}");
            return 2;
        }
    };
    // Warn loudly on a field the Kind doesn't accept — on the interactive `emit` path the
    // user typed it, so a silent allow-list drop (correct for automatic hooks) would just
    // hide a typo. The dropped field never reaches the sink either way.
    if let Some(spec) = reg.kind(kind) {
        let unknown: Vec<&str> = payload
            .keys()
            .filter(|k| !spec.fields.contains(*k))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            let mut accepted: Vec<&str> = spec.fields.iter().map(String::as_str).collect();
            accepted.sort_unstable();
            eprintln!(
                "emit: {kind} does not accept {unknown:?} (dropped) — accepted fields: {}",
                accepted.join(", ")
            );
        }
    }
    match hatel_core::make_envelope(kind, payload, &reg, cfg.strict) {
        Ok(env) => {
            let mut sink = hatel_core::build_sink(&cfg);
            sink.write_record(&env);
            sink.flush();
            0
        }
        Err(e) => {
            eprintln!("emit: {e}");
            2
        }
    }
}

/// Build the payload from, in precedence: an explicit `--json` object, else
/// `key=value` / `key:=json` field pairs, else a JSON object on stdin.
fn build_payload(fields: Vec<String>, json: Option<String>) -> Result<Payload, String> {
    if let Some(j) = json {
        return parse_json_object(&j);
    }
    if !fields.is_empty() {
        return parse_pairs(&fields);
    }
    use std::io::Read as _;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    parse_json_object(&buf)
}

fn parse_json_object(s: &str) -> Result<Payload, String> {
    let value: serde_json::Value =
        serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "payload must be a JSON object".to_string())?;
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `key=value` → a string; `key:=value` → `value` parsed as JSON (numbers, bools,
/// arrays). The `:=` is explicit, so types are never guessed from the string.
fn parse_pairs(fields: &[String]) -> Result<Payload, String> {
    let mut payload = Payload::new();
    for f in fields {
        let eq = f
            .find('=')
            .ok_or_else(|| format!("field {f:?} must be key=value or key:=json"))?;
        let typed = eq > 0 && f.as_bytes()[eq - 1] == b':';
        let key = f[..if typed { eq - 1 } else { eq }].to_string();
        if key.is_empty() {
            return Err(format!("field {f:?} has an empty key"));
        }
        let raw = &f[eq + 1..];
        let value = if typed {
            serde_json::from_str(raw)
                .map_err(|e| format!("field {key:?}: invalid JSON value {raw:?}: {e}"))?
        } else {
            serde_json::Value::String(raw.to_string())
        };
        payload.insert(key, value);
    }
    Ok(payload)
}

fn report_cmd(
    window: &str,
    format: Format,
    top: usize,
    project: Option<&str>,
    kind: Option<&str>,
) -> i32 {
    let cfg = Config::load();
    let reg = match build_registry(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("report: {e}");
            return 1;
        }
    };
    if let Some(k) = kind
        && reg.kind(k).is_none()
    {
        let known: Vec<&str> = reg.kinds().map(|s| s.name.as_str()).collect();
        eprintln!(
            "report: unknown kind {k:?}; registered: {}",
            known.join(", ")
        );
        return 2;
    }
    let Some(window_secs) = report::parse_window(window) else {
        eprintln!("report: invalid window {window:?} (expected e.g. 30d — days only)");
        return 2;
    };
    // One window boundary for the whole report — both the Kind aggregation and the cost
    // section use this single `since`, so they can't disagree on a record in the last second.
    let since = hatel_core::now_epoch().saturating_sub(window_secs);
    // `--kind` scopes the rollup to one Kind; the native cost snapshot is not a Kind, so it
    // is shown only for the full report.
    match format {
        Format::Json => print!(
            "{}",
            report_json(&reg, &cfg, since, window, top, project, kind)
        ),
        Format::Md => {
            print!(
                "{}",
                render::format_markdown(&reg, &cfg, since, window, top, project, kind)
            );
            if kind.is_none() {
                print!("{}", cost_section(&cfg.state_dir, true, since, project));
            }
        }
        Format::Text => {
            print!(
                "{}",
                render::format_table(&reg, &cfg, since, window, top, project, kind)
            );
            if kind.is_none() {
                print!("{}", cost_section(&cfg.state_dir, false, since, project));
            }
        }
    }
    0
}

/// Machine-readable report: per-Kind group aggregates plus the cost snapshot.
fn report_json(
    reg: &Registry,
    cfg: &Config,
    since: i64,
    window: &str,
    top: usize,
    project: Option<&str>,
    kind: Option<&str>,
) -> String {
    let kinds: Vec<serde_json::Value> = reg
        .kinds()
        .filter(|s| kind.is_none_or(|k| s.name == k))
        .map(|s| {
            serde_json::json!({
                "kind": s.name,
                "groups": report::aggregate(reg, cfg, &s.name, since, top, project),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "window": window,
        "project": project,
        "kinds": kinds,
        "cost": if kind.is_some() { Vec::new() } else { cost_rows(&cfg.state_dir, since, project) },
    }))
    .map(|s| s + "\n")
    .unwrap_or_default()
}

/// Cost snapshot rows within the window (`since` = epoch-second lower bound), optionally
/// restricted to one project — so the cost section honors `--window`/`--project` exactly
/// as the Kind aggregation does.
fn cost_rows(state_dir: &Path, since: i64, project: Option<&str>) -> Vec<cost::CostRow> {
    cost::read_snapshot(state_dir)
        .into_iter()
        .filter(|r| project.is_none_or(|p| r.project == p))
        .filter(|r| hatel_core::ts_epoch(&r.ts).is_some_and(|t| t >= since))
        .collect()
}

/// The native-OTel cost snapshot (written by `serve`). Read from the snapshot file,
/// not the event ledger.
fn cost_section(state_dir: &Path, markdown: bool, since: i64, project: Option<&str>) -> String {
    let rows = cost_rows(state_dir, since, project);
    if rows.is_empty() {
        return String::new();
    }
    let project = |p: &str| {
        if p.is_empty() {
            "(unknown)".to_string()
        } else {
            p.to_string()
        }
    };
    let short = |s: &str| s.chars().take(8).collect::<String>();
    let mut out = String::new();
    if markdown {
        out.push_str("\n## cost (latest snapshot per session)\n\n");
        out.push_str("| session | project | tokens | cost$ | active_s | lines |\n");
        out.push_str("|---|---|---:|---:|---:|---:|\n");
        for r in &rows {
            out.push_str(&format!(
                "| {} | {} | {} | {:.4} | {:.1} | {} |\n",
                short(&r.session_id),
                project(&r.project),
                r.tokens,
                r.cost_usd,
                r.active_time_s,
                r.lines
            ));
        }
    } else {
        out.push_str("\n--- cost (latest per session) ---\n");
        for r in &rows {
            out.push_str(&format!(
                "{} {} tokens={} cost={:.4} active={:.1} lines={}\n",
                short(&r.session_id),
                project(&r.project),
                r.tokens,
                r.cost_usd,
                r.active_time_s,
                r.lines
            ));
        }
    }
    out
}

fn kinds_cmd(json: bool) -> i32 {
    let cfg = Config::load();
    let reg = match build_registry(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kinds: {e}");
            return 1;
        }
    };
    if json {
        let arr: Vec<serde_json::Value> = reg
            .kinds()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "fields": s.fields,
                    "group_key": s.group_key,
                    "redact": s.redact,
                    "measures": s.measures,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    } else {
        for s in reg.kinds() {
            let fields: Vec<&str> = s.fields.iter().map(String::as_str).collect();
            println!(
                "{:<14} group_key={:<12} fields=[{}]",
                s.name,
                s.group_key,
                fields.join(", ")
            );
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_rows_honor_window_and_project() {
        let dir = std::env::temp_dir().join(format!("ht-costwin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let row = |sid: &str, proj: &str, ts: &str| {
            format!(
                "{{\"session_id\":\"{sid}\",\"project\":\"{proj}\",\"tokens\":1,\"cost_usd\":0.0,\
                 \"active_time_s\":0.0,\"lines\":0,\"ts\":\"{ts}\"}}"
            )
        };
        let now = hatel_core::now_iso_utc();
        std::fs::write(
            dir.join("cost_snapshot.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                row("old", "alpha", "2000-01-01T00:00:00Z"),
                row("recent", "alpha", &now),
                row("other", "beta", &now),
            ),
        )
        .unwrap();
        let day_ago = hatel_core::now_epoch() - 86_400;
        // window drops the year-2000 row
        let in_window = cost_rows(&dir, day_ago, None);
        assert_eq!(in_window.len(), 2, "old row outside window dropped");
        // project further restricts
        let alpha = cost_rows(&dir, day_ago, Some("alpha"));
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].session_id, "recent");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pairs_split_string_and_json_by_separator() {
        let p = parse_pairs(&[
            "check=lint".into(),
            "runs:=14000".into(),
            "ok:=true".into(),
            "note=a:=b".into(),
        ])
        .unwrap();
        assert_eq!(p.get("check").unwrap(), &serde_json::json!("lint"));
        assert_eq!(p.get("runs").unwrap(), &serde_json::json!(14000));
        assert_eq!(p.get("ok").unwrap(), &serde_json::json!(true));
        // The first `=` wins, so this stays a plain string — never re-guessed.
        assert_eq!(p.get("note").unwrap(), &serde_json::json!("a:=b"));
    }

    #[test]
    fn pair_without_separator_or_key_is_an_error() {
        assert!(parse_pairs(&["lonely".into()]).is_err());
        assert!(parse_pairs(&["=novalue".into()]).is_err());
    }

    #[test]
    fn typed_pair_with_bad_json_errors() {
        assert!(parse_pairs(&["n:=not-json".into()]).is_err());
    }

    #[test]
    fn report_json_kind_filter_scopes_to_one_kind() {
        let dir = std::env::temp_dir().join(format!("ht-kindfilter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            sink: hatel_core::SinkKind::Jsonl,
            ledger_dir: dir.join("ledger"),
            state_dir: dir.clone(),
            plugins: vec![],
            rotate_bytes: 10 * 1024 * 1024,
            retention_days: 90,
            disabled: false,
            strict: false,
        };
        let reg = build_registry(&cfg).unwrap();
        // An empty ledger still lists one entry per Kind (with empty groups), so the
        // filter is observable without seeding records.
        let full: serde_json::Value =
            serde_json::from_str(&report_json(&reg, &cfg, 0, "7d", 0, None, None)).unwrap();
        let full_kinds: Vec<&str> = full["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kind"].as_str().unwrap())
            .collect();
        assert!(
            full_kinds.contains(&"tool"),
            "full report lists every Kind: {full_kinds:?}"
        );
        assert!(full_kinds.len() > 1);

        let scoped: serde_json::Value =
            serde_json::from_str(&report_json(&reg, &cfg, 0, "7d", 0, None, Some("tool"))).unwrap();
        let scoped_kinds: Vec<&str> = scoped["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            scoped_kinds,
            vec!["tool"],
            "--kind scopes to exactly that Kind"
        );
        // the native cost section is not a Kind, so a scoped report drops it
        assert_eq!(scoped["cost"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
