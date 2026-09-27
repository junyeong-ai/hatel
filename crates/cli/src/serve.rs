//! The local OTLP/HTTP receiver. Decodes native metrics + logs into per-session
//! totals, joins them to projects through the session index (the only source of
//! project identity, since OTel carries none on the wire), and renders a live
//! per-session view filtered to the current project. It also persists the cost snapshot on
//! every flush and when it stops, so reports survive offline.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use hatel_core::config::Retention;
use hatel_core::cost::{self, CostRow};
use hatel_core::schema::build_registry;
use hatel_core::{
    Config, ExportConfig, Registry, SessionIndex, SessionIndexCache, Settings, resolve_project,
};

use crate::export::{Exporter, OtlpSignal};
use crate::otlp::{Accumulator, SessionTotals, UNATTRIBUTED, parse_logs, parse_metrics};
use crate::receiver;
use crate::throttle::{Tally, Throttle};

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
/// How long shutdown waits for the export queue to drain before abandoning the rest — bounded so
/// a dead downstream can't hang the receiver's exit.
pub(crate) const EXPORT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// OTLP/HTTP body cap. Far above any real batch (axum's 2 MB default would silently
/// 413 a large export and lose it), but bounded so a runaway body can't exhaust memory.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone)]
struct AppState {
    acc: Arc<Mutex<Accumulator>>,
    tracked: Arc<BTreeSet<String>>,
    counted: Arc<BTreeSet<String>>,
    cfg: Arc<Config>,
    /// The configuration file this receiver read when it started, read again by a step that
    /// deletes records ([`horizon`]), and the environment it resolves that file against: the
    /// process's own, which a test replaces.
    config_file: Option<PathBuf>,
    env: fn(&str) -> Option<std::ffi::OsString>,
    /// The change-gated session→project map, shared by the live render, each flush, and (via the
    /// exporter) egress — re-folded only when the index files change, so a growing index is not
    /// re-parsed on every batch. Taken before `acc` wherever both are held.
    index_cache: Arc<Mutex<SessionIndexCache>>,
    /// The persisted side of cost. Taken only by the flush and the sweep, ahead of `index_cache`.
    costs: Arc<Mutex<Costs>>,
    /// The current project's unique key (git-root path) — the default filter, so
    /// two same-named repositories are never conflated.
    current_key: Option<String>,
    /// An explicit `--project` override, matched against the display label.
    project_filter: Option<String>,
    show_all: bool,
    /// Whether a terminal is attached to stdout, and so whether the live view has a reader.
    live: bool,
    /// The egress forwarder, present only when `[[export]]` destinations are configured. Each
    /// received body is queued here (fire-and-forget) before local decode.
    exporter: Option<Exporter>,
    undecodable: Arc<Undecodable>,
    failed_writes: Arc<FailedWrites>,
}

/// Bodies this build could not decode for its own view, per signal. A client on the wrong protocol
/// sends nothing else, so each signal's are noted through [`crate::throttle`], not per request.
#[derive(Default)]
struct Undecodable {
    metrics: Throttle,
    logs: Throttle,
}

/// The cost snapshot this receiver writes, and what it holds from before this receiver started.
struct Costs {
    snapshot: cost::Snapshot,
    /// Per-session totals already persisted before this receiver started, so a session that spans
    /// a receiver restart continues from its prior total rather than being overwritten by only the
    /// post-restart deltas. An entry lasts as long as its session's snapshot row, which is dated by
    /// when the session was last heard, not by when the entry was loaded.
    baseline: BTreeMap<String, CostRow>,
}

impl Costs {
    fn load(state_dir: &Path) -> Self {
        let snapshot = cost::Snapshot::load(state_dir);
        let baseline = snapshot.rows().clone();
        Costs { snapshot, baseline }
    }
}

fn note_undecodable(bodies: &Throttle, signal: &str, e: &str) {
    if let Some(count) = bodies.occur(Instant::now()) {
        eprintln!("hatel: undecodable OTLP {signal} body — {e} ({count} so far)");
    }
}

/// Local writes that failed, per store. A store that stays unwritable fails on every flush, so
/// each is noted through [`crate::throttle`]; the write itself is retried by the next flush.
#[derive(Default)]
struct FailedWrites {
    cost: Throttle,
    index: Throttle,
}

fn note_failed_write(writes: &Throttle, store: &str, result: std::io::Result<()>) {
    if let Err(e) = result
        && let Some(count) = writes.occur(Instant::now())
    {
        eprintln!("hatel: {store} write failed — {e} ({count} so far)");
    }
}

pub fn run(port: u16, project: Option<String>, show_all: bool, wait: bool) -> i32 {
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
    runtime.block_on(serve(port, project, show_all, wait))
}

/// What a receiver needs before it serves: the configuration, the registry of Kinds, and the export
/// destinations. Each is fatal when broken, so `service` resolves them too before it starts a
/// receiver, since stopping a running one for one that will not start is an outage.
pub(crate) struct Startup {
    cfg: Config,
    registry: Registry,
    export: ExportConfig,
}

pub(crate) fn startup() -> Result<Startup, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    let registry = build_registry(&cfg).map_err(|e| e.to_string())?;
    // A misconfigured export file is fatal (like a bad registry) — fail fast rather than silently
    // drop a destination the operator asked for. Never reached by the hook.
    let export = ExportConfig::load().map_err(|e| e.to_string())?;
    Ok(Startup {
        cfg,
        registry,
        export,
    })
}

async fn serve(port: u16, project: Option<String>, show_all: bool, wait: bool) -> i32 {
    // One stop request for the whole run, listened for from here on, so one sent while this
    // receiver waits or starts takes the graceful path as one sent while it serves does.
    let mut stop = match stop_requested() {
        Ok(stop) => Box::pin(stop),
        Err(e) => {
            eprintln!("serve: cannot listen for a stop request: {e}");
            return 1;
        }
    };
    let addr = format!("127.0.0.1:{port}");
    // Each attempt starts afresh: it reads the configuration, takes the single-writer lock on the
    // state dir that configuration names, and binds the port. The cost snapshot and the tool ledger
    // assume one receiver per state dir, so the lock comes before any write and is held in
    // `_state_lock` for the whole run; the OS releases it on exit. With `--wait`, as the service runs
    // it, a receiver that finds either held gives up both and tries again: it takes over as soon as
    // the holder exits, where a service manager would start it again only after its throttle, it
    // leaves the store to a receiver started meanwhile on another port, and it serves the file as it
    // is at takeover. Without `--wait`, it says which is held and exits non-zero.
    let mut waiting_for = None;
    let (started, _state_lock, listener) = loop {
        let started = match startup() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("serve: {e}");
                return 1;
            }
        };
        let held = match acquire_state_lock(&started.cfg.state_dir) {
            LockOutcome::Acquired(lock) => match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => break (started, lock, listener),
                Err(e) if wait && e.kind() == std::io::ErrorKind::AddrInUse => {
                    format!("{addr} is in use — waiting for it to be freed")
                }
                Err(e) => {
                    eprintln!("serve: cannot bind {addr}: {e}");
                    return 1;
                }
            },
            LockOutcome::Held if wait => format!(
                "another hatel receiver holds the lock on {} — waiting for it to exit",
                started.cfg.state_dir.display()
            ),
            LockOutcome::Held => {
                eprintln!(
                    "serve: another hatel receiver already holds the lock on {} — exiting (only \
                     one runs per state dir; `serve --wait` waits for it instead)",
                    started.cfg.state_dir.display()
                );
                return 1;
            }
            LockOutcome::Failed(e) => {
                eprintln!("serve: {e}");
                return 1;
            }
        };
        if waiting_for.as_ref() != Some(&held) {
            eprintln!("serve: {held}");
            waiting_for = Some(held);
        }
        if stopped_while_waiting(&mut stop).await {
            return 0;
        }
    };
    let Startup {
        cfg,
        registry,
        export: export_cfg,
    } = started;
    let cfg = Arc::new(cfg);
    let registry = Arc::new(registry);
    let current_key = std::env::current_dir()
        .ok()
        .and_then(|d| resolve_project(&d.to_string_lossy()))
        .map(|p| p.key);
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
        config_file: Settings::path(),
        env: |key| std::env::var_os(key),
        index_cache: Arc::new(Mutex::new(SessionIndexCache::new(cfg.state_dir.clone()))),
        costs: Arc::new(Mutex::new(Costs::load(&cfg.state_dir))),
        current_key,
        project_filter: project,
        show_all,
        live: std::io::stdout().is_terminal(),
        exporter,
        undecodable: Arc::default(),
        failed_writes: Arc::default(),
    };

    let app = Router::new()
        .route("/v1/metrics", post(ingest_metrics))
        .route("/v1/logs", post(ingest_logs))
        .route(receiver::IDENTITY_PATH, get(identity))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state.clone());

    println!(
        "hatel receiver on http://{addr} ({}) — point \
         OTEL_EXPORTER_OTLP_ENDPOINT here; Ctrl-C to stop",
        scope_label(&state)
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

    // The state lock is what makes an unrenamed temp collectable: no other writer holds one, and
    // this receiver has not written yet.
    let orphans = hatel_core::cost::sweep_orphan_temps(&cfg.state_dir);
    if orphans > 0 {
        eprintln!("hatel: removed {orphans} cost-snapshot temp file(s) a previous run left behind");
    }
    // Retention sweep, destructive, so under the same lock and once this receiver is serving. The
    // flush loop repeats it.
    sweep(&state);

    // Keep the persisted cost snapshot fresh while running (a long-lived daemon
    // never reaches the shutdown flush otherwise).
    let flush_state = state.clone();
    let flush_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH_INTERVAL);
        let sweep_every = flush_state.cfg.sweep_interval_secs();
        let mut last_prune = hatel_core::now_epoch();
        loop {
            tick.tick().await;
            persist_cost(&flush_state);
            renew_index(&flush_state);
            render(&flush_state);
            note_tallies(&flush_state, Tally::Overdue(Instant::now()));
            let now = hatel_core::now_epoch();
            if sweep_due(now, last_prune, sweep_every) {
                last_prune = now;
                sweep(&flush_state);
            }
        }
    });

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        stop.await;
        eprintln!("\nshutting down; persisting cost snapshot…");
    });
    if let Err(e) = server.await {
        eprintln!("serve: {e}");
    }
    flush_task.abort();
    let _ = flush_task.await; // wait for it to fully stop, so the final flush is the sole writer
    flush_on_stop(&state);
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
    // After the drain, finished or given up on, so what the export held back is counted too.
    note_tallies(&state, Tally::Stopping);
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

/// Which build is answering here, and the store it writes — what `doctor` compares its own
/// against.
async fn identity(State(st): State<AppState>) -> Json<receiver::Identity> {
    Json(receiver::Identity::of(&st.cfg))
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
            lock(&st.acc).update_metrics(points, jiff::Timestamp::now());
        }
        Ok(_) => {}
        Err(e) => note_undecodable(&st.undecodable.metrics, "metrics", &e),
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
    // Decode the body once for the counted-event tallies the live view folds.
    match parse_logs(body.as_ref(), &st.counted) {
        Ok(decoded) => {
            if !decoded.is_empty() {
                lock(&st.acc).update_events(decoded, jiff::Timestamp::now());
            }
        }
        Err(e) => note_undecodable(&st.undecodable.logs, "logs", &e),
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

/// Recover a poisoned lock rather than cascading panics through every handler — a daemon stays up
/// even if one request panicked mid-update.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The outcome of trying to take the receiver's single-writer lock: `Acquired` (the held file —
/// keep it alive for the process lifetime), `Held` (another receiver currently holds it — this
/// instance can't run yet: it waits for the lock under `--wait` and exits non-zero otherwise), or
/// `Failed` (a genuine I/O problem).
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

/// How often a receiver started with `--wait` tries the lock or the port again.
const WAIT_RETRY: Duration = Duration::from_secs(1);

/// Wait one [`WAIT_RETRY`]; `true` when a stop was requested meanwhile.
async fn stopped_while_waiting<F: Future<Output = ()>>(stop: &mut std::pin::Pin<Box<F>>) -> bool {
    tokio::select! {
        () = stop.as_mut() => true,
        () = tokio::time::sleep(WAIT_RETRY) => false,
    }
}

/// Listen for a stop request: SIGTERM, with which a service manager (launchd/systemd) stops the
/// daemon, or Ctrl-C (SIGINT) in an interactive run, so the graceful path and its final cost flush
/// run either way. The handlers are installed by this call, not when the future is first polled:
/// until they are, either signal ends the process by its default action.
fn stop_requested() -> std::io::Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        })
    }
    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        Ok(async move {
            ctrl_c.recv().await;
        })
    }
}

/// Draw the live per-session rollup. Called on the flush tick, not on ingest: a frame is a human
/// view, so its cadence belongs to the reader rather than to whatever rate Claude Code happens to
/// export at. Skipped entirely when stdout is not a terminal — under a service manager that
/// stream is a log, and a repainted table is not something to keep forever.
fn render(st: &AppState) {
    if !st.live {
        return;
    }
    // Refresh the change-gated index cache, then build the whole frame under the locks and release
    // them before any stdout I/O, so a slow/blocked terminal can never stall OTLP ingestion. The
    // index cache is taken before the accumulator — the one lock order this and `persist` share.
    let out = {
        let mut index = lock(&st.index_cache);
        index.refresh();
        let acc = lock(&st.acc);
        let mut rows = String::new();
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
            rows.push_str(&row(sid, &label, totals));
            rows.push_str(&agent_rows(totals));
        }
        // A frame with no rows says nothing the header hasn't; drawing it every tick would fill an
        // idle receiver's terminal with empty tables.
        if rows.is_empty() {
            return;
        }
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
        out.push_str(&rows);
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

/// How the receiver describes its scope at startup, stated in the terms `passes_filter` decides
/// by: a receiver that announced one scope while admitting another would misdescribe every row
/// shown under it.
fn scope_label(st: &AppState) -> &'static str {
    if st.show_all {
        "all projects"
    } else if st.project_filter.is_none() && st.current_key.is_none() {
        "all projects — no repository here to scope to"
    } else {
        "this project only"
    }
}

/// Indented per-subagent breakdown, shown only when a real subagent is present, so
/// single-agent sessions stay uncluttered (`main` / `(unattributed)` only → hidden).
fn agent_rows(t: &SessionTotals) -> String {
    let agents = t.by_agent();
    // Show the breakdown only when a real (named) subagent is present — hide it when
    // everything is top-level (`main` / `(unattributed)`), however many such buckets.
    let only_top_level = agents.keys().all(|a| a == "main" || a == UNATTRIBUTED);
    if only_top_level {
        return String::new();
    }
    let mut out = String::new();
    for (agent, spend) in &agents {
        out.push_str(&format!(
            "  └ {:<27} {:>9} {:>9.4}\n",
            truncate(agent, 27),
            spend.tokens,
            spend.cost_usd
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

/// Report what this receiver's throttles hold back, per `when`: on each flush, the occurrences that
/// have waited an interval since their condition's last note; as it stops, all of them.
fn note_tallies(st: &AppState, when: Tally) {
    for (bodies, signal) in [
        (&st.undecodable.metrics, "metrics"),
        (&st.undecodable.logs, "logs"),
    ] {
        if let Some(count) = bodies.tally(when) {
            eprintln!("hatel: undecodable OTLP {signal} bodies: {count} so far");
        }
    }
    for (writes, store) in [
        (&st.failed_writes.cost, "cost snapshot"),
        (&st.failed_writes.index, "session index"),
    ] {
        if let Some(count) = writes.tally(when) {
            eprintln!("hatel: {store} writes failed: {count} so far");
        }
    }
    if let Some(exporter) = &st.exporter {
        exporter.note_tally(when);
    }
}

/// Whether the periodic retention sweep is due, `every` seconds after the `last` one. Timed on the
/// wall clock, as retention itself is: a monotonic clock can stop while the machine sleeps, which on
/// a laptop would stretch a day between sweeps into several. A clock stepped back sweeps at once
/// rather than waiting out the step.
fn sweep_due(now: i64, last: i64, every: i64) -> bool {
    now < last || now - last >= every
}

/// The configuration a step that deletes records applies: the receiver's own, with the longer of
/// its retention and the one `file` sets now. Raising `retention_days` therefore keeps records from
/// the next sweep on, and through the stop of the restart that applies every other setting;
/// lowering it waits for that restart, as every other setting does. `Err` when the file cannot be
/// read now, since a horizon the file cannot confirm deletes nothing.
fn horizon(
    running: &Config,
    file: Option<&Path>,
    env: fn(&str) -> Option<std::ffi::OsString>,
) -> hatel_core::Result<Config> {
    let settings = file.map_or_else(|| Ok(Settings::default()), Settings::read)?;
    let mut cfg = running.clone();
    cfg.retention_days = cfg
        .retention_days
        .max(Config::from_settings_in(&settings, &env).retention_days);
    Ok(cfg)
}

/// The retention sweep: the [`horizon`] applied to every record store at once — the ledger and the
/// session index (`prune_ledger`), and the cost snapshot. A session last heard before the horizon
/// leaves the snapshot and this receiver's memory together, so one heard again counts only what it
/// reports from then on.
fn sweep(st: &AppState) {
    let cfg = match horizon(&st.cfg, st.config_file.as_deref(), st.env) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("hatel: retention sweep skipped, the configuration file cannot be read: {e}");
            return;
        }
    };
    let retention = cfg.retention(hatel_core::now_epoch());
    prune_ledger(&cfg, retention);
    let mut costs = lock(&st.costs);
    let Costs { snapshot, baseline } = &mut *costs;
    let checkpoint = snapshot.checkpoint(retention.cutoff);
    note_failed_write(&st.failed_writes.cost, "cost snapshot", checkpoint);
    baseline.retain(|sid, _| snapshot.rows().contains_key(sid));
    lock(&st.acc).forget_unheard_since(retention.cutoff);
}

/// Apply `retention` to the ledger and the session index.
///
/// Pruning the session index is safe even though it is the project-attribution join table.
/// Rotation only moves lines between its files, all of which attribution reads, and a file goes
/// only once its NEWEST line is past the horizon — which `renew_index` keeps from happening to a
/// session still sending telemetry, however long ago it started. Once a session goes unheard, its
/// project lasts the horizon less at most one rotation span; a session unheard that long — ended,
/// or running while no receiver listened — cannot be told apart from an ended one, and expires.
/// Tool records bake in their project label at write time, and a cost row keeps the project it
/// carries once the index no longer holds its session (`persist_cost`), so neither loses its
/// attribution to this prune.
fn prune_ledger(cfg: &Config, retention: Retention) {
    let removed = hatel_core::sink::prune(cfg, retention);
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
    // The session index is sink-independent, so it is pruned on the same horizon regardless of
    // which sink holds the records.
    let index_removed = SessionIndex::new(cfg.state_dir.clone()).prune(retention);
    if index_removed > 0 {
        eprintln!(
            "hatel: retention — removed {index_removed} archived session-index file(s) older than {} days",
            cfg.retention_days
        );
    }
}

/// The receiver's last write before it exits: the final totals and renewals, then a checkpoint, so
/// a stopped receiver leaves every cost row in the snapshot file alone — the one file a reader that
/// predates the changes file reads.
fn flush_on_stop(st: &AppState) {
    persist_cost(st);
    renew_index(st);
    // A file that cannot be read now leaves the rows past the horizon to the next sweep.
    let retain_since = horizon(&st.cfg, st.config_file.as_deref(), st.env)
        .map_or(i64::MIN, |cfg| {
            cfg.retention(hatel_core::now_epoch()).cutoff
        });
    let checkpoint = lock(&st.costs).snapshot.checkpoint(retain_since);
    note_failed_write(&st.failed_writes.cost, "cost snapshot", checkpoint);
}

/// Renew the session-index attribution of every session this receiver has heard from long enough
/// after it was last written down (`SessionIndexCache::due_renewal`). Run on the flush, a renewal
/// lands within one flush of the activity that made it due, so a restart cannot forget that
/// activity before it is written down.
fn renew_index(st: &AppState) {
    let due = {
        let mut index = lock(&st.index_cache);
        index.refresh();
        let acc = lock(&st.acc);
        index.due_renewal(
            acc.sessions()
                .iter()
                .map(|(sid, t)| (sid.as_str(), t.last_seen().as_second())),
            st.cfg.rotation_span_secs(),
        )
    };
    let index = SessionIndex::new(st.cfg.state_dir.clone());
    for (sid, row) in &due {
        let renewal = index.renew(sid, row, st.cfg.rotate_bytes);
        note_failed_write(&st.failed_writes.index, "session index", renewal);
    }
}

fn persist_cost(st: &AppState) {
    let mut costs = lock(&st.costs);
    let mut index = lock(&st.index_cache);
    index.refresh();
    let acc = lock(&st.acc);
    let no_counts = BTreeMap::new();
    let no_spend = BTreeMap::new();
    let rows: Vec<CostRow> = acc
        .sessions()
        .iter()
        .map(|(sid, t)| {
            // The pre-restart baseline is added per metric, and only where that metric
            // is delta — a cumulative metric already reports its full total, so adding
            // it would double-count. Per-metric (not per-session) keeps a mixed-
            // temporality session correct. The dimensional breakdowns apply the same
            // rule per bucket key (`cost::merge_counts` / `merge_spend`), so a model
            // or subagent used only before the restart keeps its spend.
            let base = costs.baseline.get(sid);
            let add = |is_delta: bool, pick: fn(&CostRow) -> f64| -> f64 {
                if is_delta {
                    base.map_or(0.0, pick)
                } else {
                    0.0
                }
            };
            // The index decides a session's project. One it does not hold — its start not landed
            // yet, or its lines expired before this row — keeps the project its row carries, since
            // no longer knowing a session's project is not a change of it.
            let project = match index.get(sid) {
                Some(r) => r.project_label.clone(),
                None if index.contains(sid) => String::new(),
                None => costs
                    .snapshot
                    .rows()
                    .get(sid)
                    .map(|r| r.project.clone())
                    .unwrap_or_default(),
            };
            CostRow {
                session_id: sid.clone(),
                project,
                tokens: t.tokens() + add(t.tokens_is_delta(), |b| b.tokens as f64) as i64,
                cost_usd: t.cost() + add(t.cost_is_delta(), |b| b.cost_usd),
                active_time_s: t.active_time_s()
                    + add(t.active_time_is_delta(), |b| b.active_time_s),
                lines: t.lines() + add(t.lines_is_delta(), |b| b.lines as f64) as i64,
                tokens_by_type: cost::merge_counts(
                    base.map_or(&no_counts, |b| &b.tokens_by_type),
                    t.tokens_by_type(),
                    t.tokens_is_delta(),
                ),
                by_model: cost::merge_spend(
                    base.map_or(&no_spend, |b| &b.by_model),
                    t.by_model(),
                    t.tokens_is_delta(),
                    t.cost_is_delta(),
                ),
                by_agent: cost::merge_spend(
                    base.map_or(&no_spend, |b| &b.by_agent),
                    t.by_agent(),
                    t.tokens_is_delta(),
                    t.cost_is_delta(),
                ),
                ts: t.last_seen().to_string(),
            }
        })
        .collect();
    // Release what ingestion and the live view share before the write; the snapshot's own lock is
    // the flush's alone.
    drop(acc);
    drop(index);
    let record = costs.snapshot.record(rows);
    note_failed_write(&st.failed_writes.cost, "cost snapshot", record);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A minimal receiver state over a scratch dir — just enough for the persist path.
    fn test_state(dir: &Path) -> AppState {
        let cfg = Config {
            sink: hatel_core::SinkKind::Jsonl,
            state_dir: dir.to_path_buf(),
            ledger_dir: dir.join("ledger"),
            plugins: vec![],
            plugin_source: hatel_core::config::PluginSource::ConfigFile,
            rotate_bytes: 10 * 1024 * 1024,
            retention_days: 90,
            disabled: false,
            strict: false,
        };
        let registry = Arc::new(build_registry(&cfg).unwrap());
        AppState {
            config_file: Some(dir.join("config.toml")),
            env: |_| None,
            acc: Arc::new(Mutex::new(Accumulator::default())),
            tracked: Arc::new(registry.tracked_metrics.clone()),
            counted: Arc::new(registry.counted_events.clone()),
            index_cache: Arc::new(Mutex::new(SessionIndexCache::new(dir.to_path_buf()))),
            cfg: Arc::new(cfg),
            costs: Arc::new(Mutex::new(Costs::load(dir))),
            current_key: None,
            project_filter: None,
            show_all: true,
            live: false,
            exporter: None,
            undecodable: Arc::default(),
            failed_writes: Arc::default(),
        }
    }

    #[test]
    fn the_announced_scope_is_the_one_applied() {
        // Whatever combination the flags and the current directory produce, the banner claims a
        // wide scope exactly when the filter admits a project it knows nothing about.
        let dir = tempfile::tempdir().unwrap();
        for show_all in [false, true] {
            for project_filter in [None, Some("acme".to_string())] {
                for current_key in [None, Some("/k/acme".to_string())] {
                    let mut st = test_state(dir.path());
                    st.show_all = show_all;
                    st.project_filter = project_filter.clone();
                    st.current_key = current_key.clone();
                    assert_eq!(
                        scope_label(&st).starts_with("all projects"),
                        passes_filter(&st, "/k/unrelated", "unrelated"),
                        "show_all={show_all} filter={project_filter:?} key={current_key:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn persist_cost_merges_dimensional_baselines_across_restart() {
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        // A session persisted by the previous receiver run: 100 tokens (90 cacheRead /
        // 10 input), all on opus.
        let base_row = CostRow {
            session_id: "S1".into(),
            tokens: 100,
            cost_usd: 1.0,
            tokens_by_type: [("cacheRead".to_string(), 90), ("input".to_string(), 10)]
                .into_iter()
                .collect(),
            by_model: [(
                "opus".to_string(),
                cost::Spend {
                    tokens: 100,
                    cost_usd: 1.0,
                },
            )]
            .into_iter()
            .collect(),
            ts: hatel_core::now_iso_utc(),
            ..CostRow::default()
        };
        lock(&st.costs).baseline = [("S1".to_string(), base_row)].into_iter().collect();
        // Post-restart delta points: more opus cacheRead tokens, and cost on a model
        // the baseline never saw.
        lock(&st.acc).update_metrics(
            vec![
                MetricPoint {
                    name: "token.usage".into(),
                    value: 50.0,
                    session_id: "S1".into(),
                    series: vec![
                        ("model".into(), "opus".into()),
                        ("type".into(), "cacheRead".into()),
                    ],
                    delta: true,
                },
                MetricPoint {
                    name: "cost.usage".into(),
                    value: 0.5,
                    session_id: "S1".into(),
                    series: vec![("model".into(), "haiku".into())],
                    delta: true,
                },
            ],
            jiff::Timestamp::now(),
        );
        persist_cost(&st);
        let rows = cost::read_snapshot(&st.cfg.state_dir);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.tokens, 150, "scalar baseline added to the delta total");
        assert!((row.cost_usd - 1.5).abs() < 1e-9);
        assert_eq!(
            row.tokens_by_type.get("cacheRead"),
            Some(&140),
            "per-key delta merge: baseline 90 + current 50"
        );
        assert_eq!(
            row.tokens_by_type.get("input"),
            Some(&10),
            "a bucket only in the baseline keeps its pre-restart spend"
        );
        assert_eq!(
            row.by_model.get("opus"),
            Some(&cost::Spend {
                tokens: 150,
                cost_usd: 1.0
            })
        );
        assert_eq!(
            row.by_model.get("haiku"),
            Some(&cost::Spend {
                tokens: 0,
                cost_usd: 0.5
            }),
            "a model first seen after the restart needs no baseline"
        );
        assert_eq!(
            row.by_agent.get(UNATTRIBUTED),
            Some(&cost::Spend {
                tokens: 50,
                cost_usd: 0.5
            }),
            "agentless series are recorded as such, never guessed"
        );
    }

    #[test]
    fn a_session_still_heard_from_keeps_its_project_past_the_horizon_of_its_start() {
        // Both started past the 90-day horizon; the receiver hears from `live` now and never from
        // `quiet`. The flush renews what it heard, so the sweep after it expires only `quiet`.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        let now = jiff::Timestamp::now();
        let started = now - jiff::SignedDuration::from_hours(24 * 100);
        let line = |sid: &str| {
            format!(
                "{{\"session_id\":\"{sid}\",\"project_key\":\"/k/{sid}\",\"project_label\":\"{sid}\",\"ts\":\"{started}\"}}\n"
            )
        };
        let archive = dir.path().join("session_index.jsonl.20260101.1");
        std::fs::write(&archive, [line("live"), line("quiet")].concat()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 86_400),
            )
            .unwrap();
        lock(&st.acc).update_metrics(
            vec![MetricPoint {
                name: "cost.usage".into(),
                value: 0.5,
                session_id: "live".into(),
                series: vec![],
                delta: true,
            }],
            now,
        );
        renew_index(&st);
        sweep(&st);
        let map = SessionIndex::new(dir.path().to_path_buf()).load();
        assert!(!archive.exists(), "the expired starts are gone");
        assert_eq!(
            map.get("live").map(|r| r.project_label.as_str()),
            Some("live")
        );
        assert!(!map.contains_key("quiet"), "a quiet session expires");
    }

    #[test]
    fn the_sweep_runs_once_its_interval_has_passed_or_the_clock_stepped_back() {
        assert!(!sweep_due(1_000, 1_000, 86_400));
        assert!(sweep_due(1_000 + 86_400, 1_000, 86_400));
        assert!(
            sweep_due(1_000, 1_000 + 365 * 86_400, 86_400),
            "a clock corrected back by a year does not stop the sweep for a year"
        );
    }

    #[test]
    fn a_cost_row_keeps_its_project_once_the_index_forgets_its_session() {
        // Every flush recomputes every session the receiver holds. `kept` loses its index lines
        // while its cost row is still retained; `moved` resumes outside any repository.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        let index = SessionIndex::new(dir.path().to_path_buf());
        let a = hatel_core::ProjectRef {
            key: "/k/a".into(),
            label: "a".into(),
        };
        index.record("kept", Some(&a), 1 << 20);
        index.record("moved", Some(&a), 1 << 20);
        let heard = |sid: &str| MetricPoint {
            name: "cost.usage".into(),
            value: 0.5,
            session_id: sid.into(),
            series: vec![],
            delta: true,
        };
        lock(&st.acc).update_metrics(vec![heard("kept"), heard("moved")], jiff::Timestamp::now());
        persist_cost(&st);
        std::fs::remove_file(dir.path().join("session_index.jsonl")).unwrap();
        index.record("moved", None, 1 << 20);
        persist_cost(&st);
        let project = |sid: &str| {
            cost::read_snapshot(dir.path())
                .into_iter()
                .find(|r| r.session_id == sid)
                .map(|r| r.project)
        };
        assert_eq!(project("kept").as_deref(), Some("a"));
        assert_eq!(project("moved").as_deref(), Some(""));
    }

    #[test]
    fn a_sweep_forgets_a_session_past_retention_so_its_return_counts_anew() {
        // S1 was persisted by an earlier run and last heard by this one, both past the 90-day
        // horizon. Once the sweep expires its row, a point it sends again is all it counts: no
        // earlier total comes back from the baseline, from this run's memory, or from disk.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let long_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 100);
        let mut earlier = cost::Snapshot::load(dir.path());
        earlier
            .record([CostRow {
                session_id: "S1".into(),
                tokens: 500,
                ts: long_ago.to_string(),
                ..CostRow::default()
            }])
            .unwrap();
        let st = test_state(dir.path());
        let tokens = |value: f64, at: jiff::Timestamp| {
            lock(&st.acc).update_metrics(
                vec![MetricPoint {
                    name: "token.usage".into(),
                    value,
                    session_id: "S1".into(),
                    series: vec![],
                    delta: true,
                }],
                at,
            );
        };
        tokens(10.0, long_ago);
        sweep(&st);
        assert!(
            cost::read_snapshot(dir.path()).is_empty(),
            "the expired row is gone"
        );
        tokens(7.0, jiff::Timestamp::now());
        persist_cost(&st);
        let rows = cost::read_snapshot(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens, 7);
    }

    #[test]
    fn a_session_heard_past_the_horizon_of_its_baseline_keeps_its_earlier_total() {
        // This receiver loaded S1's row 100 days ago, when it was fresh, and has heard S1 since.
        // The flush dates the row by that, so the sweep keeps it, and with it what S1 counted
        // before this receiver started.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let long_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 100);
        let mut earlier = cost::Snapshot::load(dir.path());
        earlier
            .record([CostRow {
                session_id: "S1".into(),
                tokens: 500,
                ts: long_ago.to_string(),
                ..CostRow::default()
            }])
            .unwrap();
        let st = test_state(dir.path());
        let tokens = |value: f64| {
            lock(&st.acc).update_metrics(
                vec![MetricPoint {
                    name: "token.usage".into(),
                    value,
                    session_id: "S1".into(),
                    series: vec![],
                    delta: true,
                }],
                jiff::Timestamp::now(),
            );
            persist_cost(&st);
        };
        tokens(7.0);
        sweep(&st);
        tokens(3.0);
        let rows = cost::read_snapshot(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens, 510);
    }

    #[test]
    fn the_horizon_is_the_longer_of_the_running_retention_and_the_files_now() {
        let dir = tempfile::tempdir().unwrap();
        let running = test_state(dir.path()).cfg;
        let file = dir.path().join("config.toml");
        let days = |text: &str| {
            std::fs::write(&file, text).unwrap();
            horizon(&running, Some(&file), |_| None).map(|cfg| cfg.retention_days)
        };
        assert_eq!(days("[storage]\nretention_days = 365\n").unwrap(), 365);
        assert_eq!(
            days("[storage]\nretention_days = 30\n").unwrap(),
            90,
            "a shorter retention waits for the restart"
        );
        assert!(days("[storage\n").is_err());
        std::fs::write(
            &file,
            "[storage]\nstate_dir = \"/elsewhere\"\nretention_days = 365\n",
        )
        .unwrap();
        let moved = horizon(&running, Some(&file), |_| None).unwrap();
        assert_eq!(
            moved.state_dir, running.state_dir,
            "the store stays the receiver's"
        );
    }

    #[test]
    fn a_sweep_or_a_stop_deletes_nothing_the_file_now_keeps_or_cannot_say() {
        let dir = tempfile::tempdir().unwrap();
        let long_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 100);
        cost::Snapshot::load(dir.path())
            .record([CostRow {
                session_id: "S1".into(),
                tokens: 500,
                ts: long_ago.to_string(),
                ..CostRow::default()
            }])
            .unwrap();
        let st = test_state(dir.path());
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[storage]\nretention_days = 365\n").unwrap();
        sweep(&st);
        assert_eq!(
            cost::read_snapshot(dir.path()).len(),
            1,
            "the file keeps a year"
        );
        std::fs::write(&config, "[storage\n").unwrap();
        sweep(&st);
        flush_on_stop(&st);
        assert_eq!(
            cost::read_snapshot(dir.path()).len(),
            1,
            "a file that cannot be read deletes nothing"
        );
        std::fs::remove_file(&config).unwrap();
        sweep(&st);
        assert!(
            cost::read_snapshot(dir.path()).is_empty(),
            "the running 90 days apply once the file keeps no longer"
        );
    }

    #[test]
    fn a_stopped_receiver_leaves_every_row_in_the_snapshot_file() {
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        lock(&st.acc).update_metrics(
            vec![MetricPoint {
                name: "token.usage".into(),
                value: 7.0,
                session_id: "S1".into(),
                series: vec![],
                delta: true,
            }],
            jiff::Timestamp::now(),
        );
        flush_on_stop(&st);
        assert!(!dir.path().join("cost_changes.jsonl").exists());
        let snapshot = std::fs::read_to_string(dir.path().join("cost_snapshot.jsonl")).unwrap();
        let rows: Vec<CostRow> = snapshot
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].session_id.as_str(), rows[0].tokens), ("S1", 7));
    }

    #[test]
    fn a_store_that_stays_unwritable_is_noted_through_its_throttle_and_retried() {
        // A non-empty directory where the changes file goes makes its rename fail, whoever runs.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let changes = dir.path().join("cost_changes.jsonl");
        std::fs::create_dir_all(changes.join("blocker")).unwrap();
        let st = test_state(dir.path());
        lock(&st.acc).update_metrics(
            vec![MetricPoint {
                name: "cost.usage".into(),
                value: 0.5,
                session_id: "S1".into(),
                series: vec![],
                delta: true,
            }],
            jiff::Timestamp::now(),
        );
        persist_cost(&st);
        persist_cost(&st);
        assert_eq!(st.failed_writes.cost.count(), 2);
        std::fs::remove_dir_all(&changes).unwrap();
        persist_cost(&st);
        assert!(changes.is_file());
    }

    #[test]
    fn a_stopping_receiver_reports_every_tally_its_throttles_hold_back() {
        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        let now = Instant::now();
        let throttles = [
            &st.undecodable.metrics,
            &st.undecodable.logs,
            &st.failed_writes.cost,
            &st.failed_writes.index,
        ];
        for t in throttles {
            t.occur(now);
            t.occur(now);
        }
        note_tallies(&st, Tally::Stopping);
        for t in throttles {
            assert_eq!(t.tally(Tally::Stopping), None, "already reported");
        }
    }

    #[test]
    fn a_flush_in_which_no_session_changed_writes_nothing() {
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        lock(&st.acc).update_metrics(
            vec![MetricPoint {
                name: "cost.usage".into(),
                value: 0.5,
                session_id: "S1".into(),
                series: vec![],
                delta: true,
            }],
            jiff::Timestamp::now(),
        );
        persist_cost(&st);
        let changes = dir.path().join("cost_changes.jsonl");
        std::fs::remove_file(&changes).unwrap();
        persist_cost(&st);
        assert!(!changes.exists());
    }

    #[test]
    fn a_flush_dates_each_session_by_when_it_was_last_heard_from() {
        // Every flush recomputes every session the receiver holds. Were a row dated by the flush, a
        // session silent for weeks would sit inside every report window and never expire.
        use crate::otlp::decode::MetricPoint;

        let dir = tempfile::tempdir().unwrap();
        let st = test_state(dir.path());
        let heard = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 5);
        lock(&st.acc).update_metrics(
            vec![MetricPoint {
                name: "cost.usage".into(),
                value: 0.5,
                session_id: "S1".into(),
                series: vec![],
                delta: true,
            }],
            heard,
        );
        persist_cost(&st);
        persist_cost(&st);
        let rows = cost::read_snapshot(&st.cfg.state_dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, heard.to_string());
    }
}
