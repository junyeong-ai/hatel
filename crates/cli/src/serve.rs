//! The local OTLP/HTTP receiver. Decodes native metrics + logs into per-session
//! totals, joins them to projects through the session index (the only source of
//! project identity, since OTel carries none on the wire), and renders a live
//! per-session view filtered to the current project. It also merges a cost snapshot
//! periodically and on shutdown so reports survive offline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};

use hatel_core::cost::{self, CostRow};
use hatel_core::schema::build_registry;
use hatel_core::{Config, ExportConfig, SessionIndex, now_iso_utc, resolve_project};

use crate::export::{Exporter, OtlpSignal};
use crate::otlp::{Accumulator, SessionTotals, parse_events, parse_metrics};

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// How long shutdown waits for the export queue to drain before abandoning the rest — bounded so
/// a dead downstream can't hang the receiver's exit.
const EXPORT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// OTLP/HTTP body cap. Far above any real batch (axum's 2 MB default would silently
/// 413 a large export and lose it), but bounded so a runaway body can't exhaust memory.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    acc: Arc<Mutex<Accumulator>>,
    tracked: Arc<BTreeSet<String>>,
    counted: Arc<BTreeSet<String>>,
    cfg: Arc<Config>,
    /// Per-session totals already persisted before this receiver started, so a
    /// session that spans a receiver restart continues from its prior total rather
    /// than being overwritten by only the post-restart deltas.
    baseline: Arc<BTreeMap<String, CostRow>>,
    /// The current project's unique key (git-root path) — the default filter, so
    /// two same-named repositories are never conflated.
    current_key: Option<String>,
    /// An explicit `--project` override, matched against the display label.
    project_filter: Option<String>,
    show_all: bool,
    /// The egress forwarder, present only when `[[export]]` destinations are configured. Each
    /// received body is queued here (fire-and-forget) before local decode.
    exporter: Option<Exporter>,
}

pub fn run(port: u16, project: Option<String>, show_all: bool) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("serve: failed to build runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(serve(port, project, show_all))
}

async fn serve(port: u16, project: Option<String>, show_all: bool) -> i32 {
    let cfg = Arc::new(Config::load());
    let registry = match build_registry(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("serve: {e}");
            return 1;
        }
    };
    // A misconfigured export file is fatal at startup (like a bad registry) — fail fast rather
    // than silently drop a destination the operator asked for. Never reached by the hook.
    let export_cfg = match ExportConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("serve: {e}");
            return 1;
        }
    };
    let current_key = std::env::current_dir()
        .ok()
        .map(|d| resolve_project(&d.to_string_lossy()).key);
    let baseline = cost::read_snapshot(&cfg.state_dir)
        .into_iter()
        .map(|r| (r.session_id.clone(), r))
        .collect();
    let (exporter, export_handle) = if export_cfg.targets.is_empty() {
        (None, None)
    } else {
        let (e, h) = Exporter::spawn(export_cfg.targets.clone(), cfg.state_dir.clone());
        (Some(e), Some(h))
    };
    let state = AppState {
        acc: Arc::new(Mutex::new(Accumulator::default())),
        tracked: Arc::new(registry.tracked_metrics.clone()),
        counted: Arc::new(registry.counted_events.clone()),
        cfg: cfg.clone(),
        baseline: Arc::new(baseline),
        current_key,
        project_filter: project,
        show_all,
        exporter,
    };

    let app = Router::new()
        .route("/v1/metrics", post(ingest_metrics))
        .route("/v1/logs", post(ingest_logs))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state.clone());

    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("serve: cannot bind {addr}: {e}");
            return 1;
        }
    };
    let scope = if show_all {
        "all projects"
    } else {
        "this project only"
    };
    println!(
        "hatel receiver on http://{addr} ({scope}) — point \
         OTEL_EXPORTER_OTLP_ENDPOINT here; Ctrl-C to stop"
    );
    // Announce egress so a forwarding deployment is visible in the log (endpoint + transform only;
    // header values are never printed).
    for t in &export_cfg.targets {
        println!("  → forwarding to {} ({})", t.endpoint, t.mode.as_str());
    }

    // Keep the persisted cost snapshot fresh while running (a long-lived daemon
    // never reaches the shutdown flush otherwise).
    let flush_state = state.clone();
    let flush_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        loop {
            tick.tick().await;
            persist_cost(&flush_state);
        }
    });

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = server.await {
        eprintln!("serve: {e}");
    }
    flush_task.abort();
    let _ = flush_task.await; // wait for it to fully stop, so the final flush is the sole writer
    persist_cost(&state);
    // Flush the export queue before exiting (a routine `service` restart would otherwise lose the
    // last, most-recent batches), bounded so an unreachable downstream can't hang the exit.
    if let Some(exporter) = &state.exporter {
        exporter.shutdown();
    }
    if let Some(handle) = export_handle {
        let _ = tokio::time::timeout(EXPORT_DRAIN_TIMEOUT, handle).await;
    }
    0
}

/// The receiver always answers 200: the status reflects that the body was *received* (and, when
/// forwarding, queued for egress), not whether this build could decode it. An undecodable body is
/// noted to stderr and surfaced by `doctor` (which detects a wrong protocol from the settings),
/// never via a status code — so a raw tee of a protobuf body the local view can't read still
/// succeeds, and an OTLP client never retries (a retry would inflate downstream delta counts).
type IngestResponse = (StatusCode, Json<serde_json::Value>);

fn ok() -> IngestResponse {
    (StatusCode::OK, Json(serde_json::json!({})))
}

async fn ingest_metrics(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> IngestResponse {
    // Queue for egress first (cheap refcount clone), independent of local decode — a raw tee must
    // forward even a body this build can't decode for its own view.
    if let Some(exporter) = &st.exporter {
        let (ct, ce) = body_headers(&headers);
        exporter.enqueue(OtlpSignal::Metrics, body.clone(), ct, ce);
    }
    match parse_metrics(body.as_ref(), &st.tracked) {
        Ok(points) if !points.is_empty() => {
            lock(&st.acc).update_metrics(points);
            render(&st);
        }
        Ok(_) => {}
        Err(e) => eprintln!("hatel: undecodable OTLP metrics body — {e}"),
    }
    ok()
}

async fn ingest_logs(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> IngestResponse {
    if let Some(exporter) = &st.exporter {
        let (ct, ce) = body_headers(&headers);
        exporter.enqueue(OtlpSignal::Logs, body.clone(), ct, ce);
    }
    match parse_events(body.as_ref(), &st.counted) {
        Ok(pairs) if !pairs.is_empty() => {
            lock(&st.acc).update_events(pairs);
            render(&st);
        }
        Ok(_) => {}
        Err(e) => eprintln!("hatel: undecodable OTLP logs body — {e}"),
    }
    ok()
}

/// The inbound `Content-Type` and `Content-Encoding`, preserved so a raw tee forwards a body
/// byte-faithfully (a protobuf body stays protobuf; a gzip body keeps its encoding).
fn body_headers(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let get = |name| {
        headers
            .get(name)
            .and_then(|v: &axum::http::HeaderValue| v.to_str().ok())
            .map(str::to_string)
    };
    (
        get(axum::http::header::CONTENT_TYPE),
        get(axum::http::header::CONTENT_ENCODING),
    )
}

/// Recover a poisoned accumulator lock rather than cascading panics through every
/// handler — a daemon stays up even if one request panicked mid-update.
fn lock(m: &Mutex<Accumulator>) -> std::sync::MutexGuard<'_, Accumulator> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

async fn shutdown_signal() {
    // A service manager (launchd/systemd) stops the daemon with SIGTERM, an interactive run with
    // Ctrl-C (SIGINT) — wait on both so the graceful path (and the final cost flush) runs either way.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;

    eprintln!("\nshutting down; persisting cost snapshot…");
}

fn render(st: &AppState) {
    let index = SessionIndex::new(st.cfg.state_dir.clone()).load();
    // Build the whole frame under the lock, then release it before any stdout I/O —
    // so a slow/blocked terminal can never stall OTLP ingestion behind the lock.
    let out = {
        let acc = lock(&st.acc);
        let mut out = String::from("\n=== hatel (live) ===\n");
        out.push_str(&format!(
            "{:<8} {:<20} {:>9} {:>9} {:>8} {:>6} {:>7} {:>6} {:>9}\n",
            "session",
            "project",
            "tokens",
            "cost$",
            "active_s",
            "lines",
            "prompts",
            "skills",
            "decisions"
        ));
        for (sid, totals) in acc.sessions() {
            let row_ref = index.get(sid);
            let label = row_ref
                .map(|r| r.project_label.clone())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "(unknown)".to_string());
            let key = row_ref.map(|r| r.project_key.as_str()).unwrap_or("");
            if !passes_filter(st, key, &label) {
                continue;
            }
            out.push_str(&row(sid, &label, totals));
            out.push_str(&agent_rows(totals));
        }
        out
    };
    print!("{out}");
    let _ = std::io::stdout().flush();
}

/// Filter on the unique project *key* by default; on the *label* only when the user
/// gave an explicit `--project`.
fn passes_filter(st: &AppState, project_key: &str, project_label: &str) -> bool {
    if st.show_all {
        return true;
    }
    if let Some(filter) = &st.project_filter {
        return project_label == filter;
    }
    match &st.current_key {
        Some(k) => project_key == k,
        None => true,
    }
}

/// Indented per-subagent breakdown, shown only when a real subagent is present, so
/// single-agent sessions stay uncluttered (`main` / `(unattributed)` only → hidden).
fn agent_rows(t: &SessionTotals) -> String {
    let agents = t.by_agent();
    // Show the breakdown only when a real (named) subagent is present — hide it when
    // everything is top-level (`main` / `(unattributed)`), however many such buckets.
    let only_top_level = agents.keys().all(|a| a == "main" || a == "(unattributed)");
    if only_top_level {
        return String::new();
    }
    let mut out = String::new();
    for (agent, (tokens, cost)) in &agents {
        out.push_str(&format!(
            "  └ {:<27} {:>9} {:>9.4}\n",
            truncate(agent, 27),
            tokens,
            cost
        ));
    }
    out
}

fn row(sid: &str, label: &str, t: &SessionTotals) -> String {
    format!(
        "{:<8} {:<20} {:>9} {:>9.4} {:>8.1} {:>6} {:>7} {:>6} {:>9}\n",
        truncate(sid, 8),
        truncate(label, 20),
        t.tokens(),
        t.cost(),
        t.active_time_s(),
        t.lines(),
        t.event_count("user_prompt"),
        t.event_count("skill_activated"),
        t.event_count("tool_decision"),
    )
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn persist_cost(st: &AppState) {
    let index = SessionIndex::new(st.cfg.state_dir.clone()).load();
    let now = now_iso_utc();
    let acc = lock(&st.acc);
    let rows: Vec<CostRow> = acc
        .sessions()
        .iter()
        .map(|(sid, t)| {
            // The pre-restart baseline is added per metric, and only where that metric
            // is delta — a cumulative metric already reports its full total, so adding
            // it would double-count. Per-metric (not per-session) keeps a mixed-
            // temporality session correct.
            let base = st.baseline.get(sid);
            let add = |is_delta: bool, pick: fn(&CostRow) -> f64| -> f64 {
                if is_delta {
                    base.map_or(0.0, pick)
                } else {
                    0.0
                }
            };
            CostRow {
                session_id: sid.clone(),
                project: index
                    .get(sid)
                    .map(|r| r.project_label.clone())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_default(),
                tokens: t.tokens() + add(t.tokens_is_delta(), |b| b.tokens as f64) as i64,
                cost_usd: t.cost() + add(t.cost_is_delta(), |b| b.cost_usd),
                active_time_s: t.active_time_s()
                    + add(t.active_time_is_delta(), |b| b.active_time_s),
                lines: t.lines() + add(t.lines_is_delta(), |b| b.lines as f64) as i64,
                ts: now.clone(),
            }
        })
        .collect();
    drop(acc);
    // Always merge — even with no active sessions this flush — so the retention prune
    // runs on an idle receiver too, and stale prior-run rows can't linger unbounded.
    // `retention_days` is capped at parse time, so this product cannot overflow.
    let retain_since = hatel_core::now_epoch() - st.cfg.retention_days * 86_400;
    cost::merge_snapshot(&st.cfg.state_dir, rows, retain_since);
}
