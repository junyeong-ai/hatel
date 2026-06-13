//! The local OTLP/HTTP receiver. Decodes native metrics + logs into per-session
//! totals, joins them to projects through the session index (the only source of
//! project identity, since OTel carries none on the wire), and renders a live
//! per-session view filtered to the current project. It also merges a cost snapshot
//! periodically and on shutdown so reports survive offline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
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
    Config, ExportConfig, Payload, Registry, SessionIndex, SessionIndexCache, make_envelope,
    now_iso_utc, resolve_project,
};

use crate::export::{Exporter, OtlpSignal};
use crate::otlp::{Accumulator, SessionTotals, ToolResult, parse_logs, parse_metrics};

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// How often the retention sweep repeats while serving (it also runs once at startup). Daily is
/// plenty — the sweep deletes archives/rows that are already ~`retention_days` old.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
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
/// How many persist cycles a tool outcome whose session isn't in the index yet is held back
/// before it is written unattributed. The `tool_result` batch can race the SessionStart hook's
/// index append; a few cycles (~2 min at the 30s cadence) absorbs that race, while a session
/// that never appears (started before hatel was wired) isn't held hostage forever. The export
/// path absorbs the same race with `exporter::DEFER_TIMEOUT` — deadline-based because that loop
/// has no cadence, and fail-closed because egress privacy outranks delivery; here the ledger
/// fails open into an honest unattributed record, because local data outranks attribution.
const MAX_TOOL_DEFERRALS: u8 = 4;

/// A buffered `tool_result` outcome plus how many persist cycles it has been deferred waiting
/// for its session to appear in the index.
struct BufferedTool {
    result: ToolResult,
    deferrals: u8,
}

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
    tool_buffer: Arc<Mutex<Vec<BufferedTool>>>,
    /// Cumulative tool outcomes dropped because the buffer was full (an honest undercount surfaced
    /// to stderr), mirroring the export queue's drop accounting.
    tool_dropped: Arc<AtomicU64>,
    cfg: Arc<Config>,
    /// The change-gated session→project map, shared by the live render, each flush, and (via the
    /// exporter) egress — re-folded only when the index files change, so a growing index is not
    /// re-parsed on every batch. Taken before `acc` wherever both are held.
    index_cache: Arc<Mutex<SessionIndexCache>>,
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
    // The single-writer lock, taken before any work: the cost snapshot and the tool ledger assume
    // one receiver per state dir, so a second one is refused here rather than left to race. Held in
    // `_state_lock` for the whole run; the OS releases it on exit.
    let _state_lock = match acquire_state_lock(&cfg.state_dir) {
        LockOutcome::Acquired(f) => f,
        LockOutcome::Held => {
            // Another receiver currently holds the lock — this instance can't run. Exit NON-ZERO so
            // the service manager (launchd `SuccessfulExit=false` / systemd `Restart=on-failure`,
            // both throttled to ≥5s) RETRIES rather than giving up: that retry is what lets a
            // service-managed receiver take over gap-free once the holder exits — e.g. when it lost
            // a startup race to a manual `serve --all`. The ≥5s throttle keeps the retry from
            // becoming a tight loop. An interactive run simply reports the message and exits non-zero.
            eprintln!(
                "serve: another hatel receiver already holds the lock on {} — exiting; a service \
                 manager will retry (only one runs per state dir)",
                cfg.state_dir.display()
            );
            return 1;
        }
        LockOutcome::Failed(e) => {
            eprintln!("serve: {e}");
            return 1;
        }
    };
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
        index_cache: Arc::new(Mutex::new(SessionIndexCache::new(cfg.state_dir.clone()))),
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

    // Retention sweep — strictly after the bind succeeded: the port is the single-writer lock,
    // and a destructive sweep belongs to the one receiver. Repeats daily from the flush loop.
    prune_ledger(&cfg);

    // Keep the persisted cost snapshot fresh while running (a long-lived daemon
    // never reaches the shutdown flush otherwise).
    let flush_state = state.clone();
    let flush_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        let mut last_prune = std::time::Instant::now();
        loop {
            tick.tick().await;
            persist(&flush_state, false);
            if last_prune.elapsed() >= PRUNE_INTERVAL {
                last_prune = std::time::Instant::now();
                prune_ledger(&flush_state.cfg);
            }
        }
    });

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = server.await {
        eprintln!("serve: {e}");
    }
    flush_task.abort();
    let _ = flush_task.await; // wait for it to fully stop, so the final flush is the sole writer
    persist(&state, true);
    // Flush the export queue before exiting (a routine `service` restart would otherwise lose the
    // last, most-recent batches), bounded so an unreachable downstream can't hang the exit.
    if let Some(exporter) = &state.exporter {
        exporter.shutdown();
    }
    if let Some(handle) = export_handle
        && tokio::time::timeout(EXPORT_DRAIN_TIMEOUT, handle)
            .await
            .is_err()
    {
        // The bound exists so a dead downstream can't hang the exit; crossing it means
        // whatever was still queued or deferred went undelivered — say so, since every
        // other drop in this binary is visible.
        eprintln!(
            "hatel: export drain exceeded {}s at shutdown — undelivered batches were dropped",
            EXPORT_DRAIN_TIMEOUT.as_secs()
        );
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
    // Decode the body once: the per-call tool outcomes (buffered for the ledger, written off the
    // request path by `persist`) and the counted-event tallies (folded into the live view) both
    // come from the single walk.
    match parse_logs(body.as_ref(), &st.counted) {
        Ok(decoded) => {
            buffer_tool_results(&st, decoded.tool_results);
            if !decoded.events.is_empty() {
                lock(&st.acc).update_events(decoded.events);
                render(&st);
            }
        }
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
fn lock_buf(m: &Mutex<Vec<BufferedTool>>) -> std::sync::MutexGuard<'_, Vec<BufferedTool>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Same poison-recovery for the session-index cache.
fn lock_index(m: &Mutex<SessionIndexCache>) -> std::sync::MutexGuard<'_, SessionIndexCache> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The outcome of trying to take the receiver's single-writer lock: `Acquired` (the held file —
/// keep it alive for the process lifetime), `Held` (another receiver currently holds it — this
/// instance can't run and exits non-zero so a throttled service manager retries until the lock
/// frees), or `Failed` (a genuine I/O problem).
enum LockOutcome {
    Acquired(std::fs::File),
    Held,
    Failed(String),
}

/// Take the receiver's single-writer lock on the state dir — an advisory lock held for the process
/// lifetime, which the OS releases on exit (even a crash). A second receiver over the same state dir
/// is told to stand down instead of racing the cost snapshot and the tool ledger, which assume one
/// writer.
#[cfg(unix)]
fn acquire_state_lock(state_dir: &Path) -> LockOutcome {
    use std::os::unix::io::AsRawFd as _;
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        return LockOutcome::Failed(format!(
            "cannot create state dir {}: {e}",
            state_dir.display()
        ));
    }
    let path = state_dir.join("serve.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            return LockOutcome::Failed(format!(
                "cannot open receiver lock {}: {e}",
                path.display()
            ));
        }
    };
    // SAFETY: `flock` on a valid borrowed fd; the kernel owns the lock and frees it when the fd
    // closes at process exit. `LOCK_NB` makes a held lock fail fast rather than block.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        LockOutcome::Acquired(file)
    } else {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            LockOutcome::Held // another receiver holds it
        } else {
            LockOutcome::Failed(format!(
                "cannot lock state dir {}: {e}",
                state_dir.display()
            ))
        }
    }
}

#[cfg(windows)]
fn acquire_state_lock(state_dir: &Path) -> LockOutcome {
    use std::os::windows::fs::OpenOptionsExt as _;
    if let Err(e) = std::fs::create_dir_all(state_dir) {
        return LockOutcome::Failed(format!(
            "cannot create state dir {}: {e}",
            state_dir.display()
        ));
    }
    let path = state_dir.join("serve.lock");
    // `share_mode(0)` denies all sharing, so a second receiver's open fails with a sharing violation
    // — the Windows analogue of the unix `flock`, released when the handle closes at process exit.
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .share_mode(0)
        .open(&path)
    {
        Ok(f) => LockOutcome::Acquired(f),
        // ERROR_SHARING_VIOLATION (32) means another receiver already holds it.
        Err(e) if e.raw_os_error() == Some(32) => LockOutcome::Held,
        Err(e) => LockOutcome::Failed(format!("cannot open receiver lock {}: {e}", path.display())),
    }
}

#[cfg(not(any(unix, windows)))]
fn acquire_state_lock(_state_dir: &Path) -> LockOutcome {
    // No advisory-lock primitive on this platform: the cost snapshot and tool ledger require one
    // writer, so refuse rather than run without that guarantee (never a silent no-op). Unreachable
    // on real targets — unix and windows cover every platform that can run the receiver.
    LockOutcome::Failed("the receiver's single-writer lock is unsupported on this platform".into())
}

/// Count dropped tool outcomes and log the cumulative total once per `TOOL_DROP_LOG_EVERY` drops
/// (throttled to avoid per-batch spam), mirroring the export queue's drop accounting.
fn record_tool_drop(counter: &AtomicU64, n: u64) {
    let before = counter.fetch_add(n, Ordering::Relaxed);
    if crate::throttle::should_log(before, n, TOOL_DROP_LOG_EVERY) {
        eprintln!(
            "hatel: tool buffer full — dropped {} tool record(s) so far (persist falling behind)",
            before + n
        );
    }
}

/// Buffer decoded `tool_result` outcomes for the ledger (written off the request path by
/// `persist`). Bounded by `MAX_TOOL_BUFFER`: when `persist` falls behind, the remaining room is
/// filled and the rest dropped and counted, so the buffer is a hard bound — an honest undercount,
/// never a blocked request.
fn buffer_tool_results(st: &AppState, results: Vec<ToolResult>) {
    if results.is_empty() {
        return;
    }
    let mut buf = lock_buf(&st.tool_buffer);
    let room = MAX_TOOL_BUFFER.saturating_sub(buf.len());
    let fresh = |result| BufferedTool {
        result,
        deferrals: 0,
    };
    if results.len() <= room {
        buf.extend(results.into_iter().map(fresh));
    } else {
        let total = results.len();
        buf.extend(results.into_iter().take(room).map(fresh));
        drop(buf);
        record_tool_drop(&st.tool_dropped, (total - room) as u64);
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
    // Refresh the change-gated index cache, then build the whole frame under the locks and release
    // them before any stdout I/O, so a slow/blocked terminal can never stall OTLP ingestion. The
    // index cache is taken before the accumulator — the one lock order this and `persist` share.
    let out = {
        let mut index = lock_index(&st.index_cache);
        index.refresh();
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

/// Apply the retention horizon (`HATEL_RETENTION_DAYS`, default 90) on ONE horizon to every record
/// store: the ledger (here), the cost snapshot (`persist_cost`), and the session index's archives
/// (below). `retention_days` is capped at parse time, so the product cannot overflow.
///
/// Pruning the session index is safe even though it is the project-attribution join table. Only
/// whole ARCHIVES past the horizon go — the active file is never touched — and an index archive is
/// past the horizon only once its NEWEST session start is, i.e. every session it holds ended long
/// ago and produces no more data to attribute. Live attribution reads only recent rows (in the
/// active file), and historical cost/tool records bake in their project label at write time, so they
/// never re-consult the index. Nothing still needing attribution can be pruned.
fn prune_ledger(cfg: &Config) {
    let cutoff = hatel_core::now_epoch() - cfg.retention_days * 86_400;
    let removed = hatel_core::sink::prune_before(cfg, cutoff);
    if removed > 0 {
        let unit = match cfg.sink {
            hatel_core::SinkKind::Jsonl => "archived ledger file(s)",
            hatel_core::SinkKind::Sqlite => "ledger row(s)",
        };
        eprintln!(
            "hatel: retention — removed {removed} {unit} older than {} days",
            cfg.retention_days
        );
    }
    // The session index is sink-independent, so its archives are pruned on the same horizon
    // regardless of which sink holds the records.
    let index_removed = SessionIndex::new(cfg.state_dir.clone()).prune(cutoff);
    if index_removed > 0 {
        eprintln!(
            "hatel: retention — removed {index_removed} archived session-index file(s) older than {} days",
            cfg.retention_days
        );
    }
}

/// Persist both OTel-derived stores off the request path: the per-session cost snapshot and the
/// buffered per-call tool outcomes. Each resolves project attribution under the index-cache lock and
/// releases it before writing, so a flush never holds a lock across I/O. `final_flush` is the
/// shutdown pass: nothing may stay buffered after it.
fn persist(st: &AppState, final_flush: bool) {
    persist_cost(st);
    persist_tool(st, final_flush);
}

/// Drain the tool-result buffer into the ledger as `tool` records, joining each call's project
/// from the session index. Written via the configured sink (the same write path the hook uses),
/// so `report` aggregates tool latency and success rate exactly like any other Kind. `tool.jsonl`
/// has a single writer — this — since the `tool` Kind has no hook binding. Buffered outcomes since
/// the last flush survive a graceful stop (the shutdown path flushes), but — unlike cost, which
/// re-derives from the cumulative OTel metric on restart — these are discrete events with no
/// resend, so an ungraceful kill loses that ≤30s window.
///
/// An outcome whose session isn't in the index yet is deferred (re-buffered) for up to
/// `MAX_TOOL_DEFERRALS` cycles rather than written unattributed immediately — the batch can race
/// the SessionStart hook's index append, and a record's project is fixed at write time. Once the
/// deferrals are exhausted, or on the final flush, it is written with an empty project: recording
/// reality (outcome known, attribution unknown) rather than dropping data. A deferred record's
/// envelope timestamp is its (later) write time, exactly like every buffered outcome — at most
/// ~2 min of skew against day-scale report windows.
fn persist_tool(st: &AppState, final_flush: bool) {
    let drained = std::mem::take(&mut *lock_buf(&st.tool_buffer));
    if drained.is_empty() {
        return;
    }
    // Resolve each outcome's project under the cache lock, partitioning into write-now (with its
    // resolved label) and defer-again, then release the lock before the sink writes below.
    let mut to_write: Vec<(ToolResult, String)> = Vec::new();
    let mut deferred: Vec<BufferedTool> = Vec::new();
    {
        let mut index = lock_index(&st.index_cache);
        index.refresh();
        for mut item in drained {
            let project = index
                .get(&item.result.session_id)
                .map(|row| row.project_label.clone())
                .filter(|l| !l.is_empty());
            if project.is_none() && !final_flush && item.deferrals < MAX_TOOL_DEFERRALS {
                item.deferrals += 1;
                deferred.push(item);
            } else {
                to_write.push((item.result, project.unwrap_or_default()));
            }
        }
    }
    if !to_write.is_empty() {
        let mut sink = build_sink(&st.cfg);
        for (r, project) in to_write {
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
    if !deferred.is_empty() {
        // Re-buffer the deferred outcomes under the same hard bound as ingestion — arrivals since
        // the drain have first claim on the room, so the cap can never overshoot.
        let mut buf = lock_buf(&st.tool_buffer);
        let room = MAX_TOOL_BUFFER.saturating_sub(buf.len());
        let total = deferred.len();
        buf.extend(deferred.into_iter().take(room));
        if total > room {
            drop(buf);
            record_tool_drop(&st.tool_dropped, (total - room) as u64);
        }
    }
}

fn persist_cost(st: &AppState) {
    let now = now_iso_utc();
    // Resolve totals and attribution under the index + accumulator locks, dropping both before the
    // snapshot write — a flush never holds a lock across I/O.
    let mut index = lock_index(&st.index_cache);
    index.refresh();
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
    drop(index); // release the cache before the snapshot I/O
    // Always merge — even with no active sessions this flush — so the retention prune
    // runs on an idle receiver too, and stale prior-run rows can't linger unbounded.
    // `retention_days` is capped at parse time, so this product cannot overflow.
    let retain_since = hatel_core::now_epoch() - st.cfg.retention_days * 86_400;
    cost::merge_snapshot(&st.cfg.state_dir, rows, retain_since);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use hatel_core::{Envelope, ProjectRef, sink};

    /// A minimal receiver state over a scratch dir — just enough for the persist path.
    fn test_state(dir: &Path) -> AppState {
        let cfg = Config {
            sink: hatel_core::SinkKind::Jsonl,
            state_dir: dir.to_path_buf(),
            ledger_dir: dir.join("ledger"),
            plugins: vec![],
            rotate_bytes: 10 * 1024 * 1024,
            retention_days: 90,
            disabled: false,
            strict: false,
        };
        let registry = Arc::new(build_registry(&cfg).unwrap());
        AppState {
            acc: Arc::new(Mutex::new(Accumulator::default())),
            tracked: Arc::new(registry.tracked_metrics.clone()),
            counted: Arc::new(registry.counted_events.clone()),
            registry,
            tool_buffer: Arc::new(Mutex::new(Vec::new())),
            tool_dropped: Arc::new(AtomicU64::new(0)),
            index_cache: Arc::new(Mutex::new(SessionIndexCache::new(dir.to_path_buf()))),
            cfg: Arc::new(cfg),
            baseline: Arc::new(BTreeMap::new()),
            current_key: None,
            project_filter: None,
            show_all: true,
            exporter: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ht-serve-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn buffer_tool(st: &AppState, sid: &str) {
        lock_buf(&st.tool_buffer).push(BufferedTool {
            result: ToolResult {
                session_id: sid.into(),
                tool_name: "Bash".into(),
                duration_ms: 5,
                ok: true,
            },
            deferrals: 0,
        });
    }

    fn tool_records(st: &AppState) -> Vec<Envelope> {
        sink::read_records(&st.cfg, "tool", None)
    }

    #[test]
    fn an_unindexed_tool_outcome_is_deferred_then_attributed() {
        let dir = scratch("defer");
        let st = test_state(&dir);
        buffer_tool(&st, "S1");
        // The session isn't indexed yet (the tool_result batch raced the SessionStart
        // hook) — the outcome is deferred, not written with an empty project.
        persist(&st, false);
        assert!(
            tool_records(&st).is_empty(),
            "unindexed outcome is deferred"
        );
        assert_eq!(lock_buf(&st.tool_buffer).len(), 1, "still buffered");
        // The SessionStart hook lands; the next cycle attributes the deferred outcome.
        SessionIndex::new(st.cfg.state_dir.clone()).record(
            "S1",
            &ProjectRef {
                key: "/k/alpha".into(),
                label: "alpha".into(),
            },
            st.cfg.rotate_bytes,
        );
        persist(&st, false);
        let recs = tool_records(&st);
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].payload.get("project").and_then(|v| v.as_str()),
            Some("alpha"),
            "deferred outcome written with its project once the index catches up"
        );
        assert!(lock_buf(&st.tool_buffer).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deferrals_exhaust_to_an_honest_unattributed_record() {
        let dir = scratch("exhaust");
        let st = test_state(&dir);
        buffer_tool(&st, "GHOST"); // a session that never gets indexed
        for cycle in 0..MAX_TOOL_DEFERRALS {
            persist(&st, false);
            assert!(
                tool_records(&st).is_empty(),
                "cycle {cycle}: still deferred"
            );
        }
        // Deferrals exhausted — written with an empty project (outcome known,
        // attribution unknown) rather than held or dropped.
        persist(&st, false);
        let recs = tool_records(&st);
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].payload.get("project").and_then(|v| v.as_str()),
            Some(""),
            "exhausted deferral records reality: unattributed"
        );
        assert!(lock_buf(&st.tool_buffer).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_final_flush_writes_unresolved_outcomes_immediately() {
        let dir = scratch("final");
        let st = test_state(&dir);
        buffer_tool(&st, "GHOST");
        // Shutdown pass: nothing may stay buffered, deferred or not.
        persist(&st, true);
        assert_eq!(tool_records(&st).len(), 1, "final flush writes everything");
        assert!(lock_buf(&st.tool_buffer).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
