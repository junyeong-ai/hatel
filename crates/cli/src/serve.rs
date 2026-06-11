//! The local OTLP/HTTP receiver. Decodes native metrics + logs into per-session
//! totals, joins them to projects through the session index (the only source of
//! project identity, since OTel carries none on the wire), and renders a live
//! per-session view filtered to the current project. It also merges a cost snapshot
//! periodically and on shutdown so reports survive offline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};

use hatel_core::cost::{self, CostRow};
use hatel_core::schema::build_registry;
use hatel_core::sink::build_sink;
use hatel_core::{
    Config, ExportConfig, Payload, Registry, SessionIndex, SessionRow, make_envelope, now_iso_utc,
    resolve_project,
};

use crate::export::{Exporter, OtlpSignal};
use crate::otlp::{
    Accumulator, SessionTotals, ToolResult, parse_events, parse_metrics, parse_tool_results,
};

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// How long shutdown waits for the export queue to drain before abandoning the rest — bounded so
/// a dead downstream can't hang the receiver's exit.
const EXPORT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// OTLP/HTTP body cap. Far above any real batch (axum's 2 MB default would silently
/// 413 a large export and lose it), but bounded so a runaway body can't exhaust memory.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Cap on tool outcomes buffered between flushes — a memory backstop (like the export queue's byte
/// cap) so a stalled `persist` can't grow the buffer without bound. Far above a realistic 30s burst.
const MAX_TOOL_BUFFER: usize = 100_000;
/// Re-log the cumulative tool-buffer drop count once per this many drops (throttled, not per-drop).
const TOOL_DROP_LOG_EVERY: u64 = 1_000;

#[derive(Clone)]
struct AppState {
    acc: Arc<Mutex<Accumulator>>,
    tracked: Arc<BTreeSet<String>>,
    counted: Arc<BTreeSet<String>>,
    /// The full registry, for sanitizing receiver-written `tool` records (the `tool` Kind's
    /// field allow-list is what keeps the rich, PII-bearing `tool_result` event content-free).
    registry: Arc<Registry>,
    /// `tool_result` outcomes decoded since the last flush, written to the ledger by `persist`
    /// (off the request path), exactly as cost is snapshotted — never blocking ingestion on I/O.
    /// Bounded by `MAX_TOOL_BUFFER`; overflow is dropped and counted in `tool_dropped`.
    tool_buffer: Arc<Mutex<Vec<ToolResult>>>,
    /// Cumulative tool outcomes dropped because the buffer was full (an honest undercount surfaced
    /// to stderr), mirroring the export queue's drop accounting.
    tool_dropped: Arc<AtomicU64>,
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
        Ok(r) => Arc::new(r),
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
        registry: registry.clone(),
        tool_buffer: Arc::new(Mutex::new(Vec::new())),
        tool_dropped: Arc::new(AtomicU64::new(0)),
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
        let filter = t
            .filter
            .describe()
            .map(|d| format!(", {d}"))
            .unwrap_or_default();
        println!(
            "  → forwarding to {} ({}{filter})",
            t.endpoint,
            t.mode.as_str()
        );
    }

    // Keep the persisted cost snapshot fresh while running (a long-lived daemon
    // never reaches the shutdown flush otherwise).
    let flush_state = state.clone();
    let flush_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        loop {
            tick.tick().await;
            persist(&flush_state);
        }
    });

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = server.await {
        eprintln!("serve: {e}");
    }
    flush_task.abort();
    let _ = flush_task.await; // wait for it to fully stop, so the final flush is the sole writer
    persist(&state);
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
    // Buffer tool outcomes for the ledger (written off the request path by `persist`). Independent
    // of the counted-event view below: the same body feeds both, decoded once for each.
    match parse_tool_results(body.as_ref()) {
        Ok(results) if !results.is_empty() => {
            let mut buf = lock_buf(&st.tool_buffer);
            let room = MAX_TOOL_BUFFER.saturating_sub(buf.len());
            if results.len() <= room {
                buf.extend(results);
            } else {
                // Persist is falling behind — fill the remaining room and drop the rest, counting
                // it, so the buffer is a hard bound (not a soft cap that overshoots by a whole
                // batch). Honest undercount, never a blocked request.
                let total = results.len();
                buf.extend(results.into_iter().take(room));
                drop(buf);
                record_tool_drop(&st.tool_dropped, (total - room) as u64);
            }
        }
        Ok(_) => {}
        Err(_) => {} // a non-decodable body is reported once by the view path below
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

/// Same poison-recovery for the tool-result buffer.
fn lock_buf(m: &Mutex<Vec<ToolResult>>) -> std::sync::MutexGuard<'_, Vec<ToolResult>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Count dropped tool outcomes and log the cumulative total once per `TOOL_DROP_LOG_EVERY` drops
/// (throttled to avoid per-batch spam), mirroring the export queue's drop accounting.
fn record_tool_drop(counter: &AtomicU64, n: u64) {
    let before = counter.fetch_add(n, Ordering::Relaxed);
    // Surface the first drop immediately (like the export queue), then throttle to once per
    // `TOOL_DROP_LOG_EVERY` — so an operator sees the problem before hundreds are already lost.
    if before == 0 || before / TOOL_DROP_LOG_EVERY != (before + n) / TOOL_DROP_LOG_EVERY {
        eprintln!(
            "hatel: tool buffer full — dropped {} tool record(s) so far (persist falling behind)",
            before + n
        );
    }
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

/// Persist both OTel-derived stores off the request path: the per-session cost snapshot and the
/// buffered per-call tool outcomes. The session index is loaded once and shared, since both join
/// `session.id` to a project label the same way.
fn persist(st: &AppState) {
    let index = SessionIndex::new(st.cfg.state_dir.clone()).load();
    persist_cost(st, &index);
    persist_tool(st, &index);
}

/// Drain the tool-result buffer into the ledger as `tool` records, joining each call's project
/// from the session index. Written via the configured sink (the same write path the hook uses),
/// so `report` aggregates tool latency and success rate exactly like any other Kind. `tool.jsonl`
/// has a single writer — this — since the `tool` Kind has no hook binding. Buffered outcomes since
/// the last flush survive a graceful stop (the shutdown path flushes), but — unlike cost, which
/// re-derives from the cumulative OTel metric on restart — these are discrete events with no
/// resend, so an ungraceful kill loses that ≤30s window.
fn persist_tool(st: &AppState, index: &BTreeMap<String, SessionRow>) {
    let drained = std::mem::take(&mut *lock_buf(&st.tool_buffer));
    if drained.is_empty() {
        return;
    }
    let mut sink = build_sink(&st.cfg);
    for r in drained {
        let project = index
            .get(&r.session_id)
            .map(|row| row.project_label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_default();
        let mut payload = Payload::new();
        payload.insert("session_id".into(), r.session_id.into());
        payload.insert("project".into(), project.into());
        payload.insert("tool_name".into(), r.tool_name.into());
        payload.insert("duration_ms".into(), r.duration_ms.into());
        payload.insert("ok".into(), i64::from(r.ok).into());
        // Only these five fields are ever inserted, so the rich `tool_result` event — which also
        // carries the user's email and tool input — stays out of the ledger. The Kind's field
        // allow-list applied by `make_envelope` is defense-in-depth on top of that.
        match make_envelope("tool", payload, &st.registry, st.cfg.strict) {
            Ok(env) => sink.write_record(&env),
            Err(e) => eprintln!("hatel: tool record dropped — {e}"),
        }
    }
    sink.flush();
}

fn persist_cost(st: &AppState, index: &BTreeMap<String, SessionRow>) {
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
