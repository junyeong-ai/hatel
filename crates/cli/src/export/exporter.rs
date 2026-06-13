//! The export runtime: a bounded queue, a single drain task, and an HTTP client that forwards
//! each received OTLP body to every configured destination. Best-effort and fail-open — a slow
//! or unreachable downstream never blocks ingestion (the queue drops and counts under pressure)
//! and is never retried (a retry would inflate downstream delta counters). The API takes only
//! `bytes::Bytes` + `OtlpSignal`, never an axum type, so a future ledger→OTLP-logs exporter can
//! reuse the same `enqueue`.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use hatel_core::{ExportMode, ExportTarget, SessionIndexCache};

use super::transform::{enrich_logs, enrich_metrics, session_ids};

/// Queue depth (slot backstop) before new batches are dropped. The memory bound is `MAX_QUEUED_BYTES`
/// below; this caps slot count so the channel itself stays small.
const EXPORT_QUEUE_CAP: usize = 1024;
/// Memory ceiling for queued bodies — the queue is bounded by *bytes*, not just slot count, so a
/// dead downstream can't accumulate unbounded memory even if individual bodies are large.
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;
/// Per-request timeout when a target doesn't set one.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Cap on bodies parked awaiting a session-index catch-up (see `Deferred`). Their bytes stay
/// reserved in `queued_bytes`, so memory is doubly bounded; `DEFER_TIMEOUT` bounds how long a
/// slot can stay held, so a never-indexed session can't monopolize the park list.
const MAX_DEFERRED: usize = 64;
/// How long a deferred body waits for *its own* sessions to reach the index before it is dropped
/// for the filtered destinations. The race it absorbs (an OTLP batch beating the SessionStart
/// hook's index append) resolves in seconds; minutes is generous, and past that the session is
/// evidently never going to be indexed (it predates the wiring). The receiver's ledger absorbs
/// the same race with `serve::MAX_TOOL_DEFERRALS` — cycle-counted because its flush loop has a
/// fixed cadence, and fail-open because local data outranks attribution; here egress privacy
/// outranks delivery, so an unresolved body fails closed.
const DEFER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Cap on establishing the TCP/TLS connection, so a downstream that accepts the socket but never
/// responds can't pin the drain task past this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Re-log the cumulative drop count once per this many drops (throttled, not per-drop spam).
const DROP_LOG_EVERY: u64 = 100;

/// Which OTLP signal a body is — selects the downstream path and the enrich walker. "Signal" is the
/// OTel-native term (metrics / logs / traces); kept distinct from the registry's `Kind` vocabulary.
#[derive(Clone, Copy)]
pub enum OtlpSignal {
    Metrics,
    Logs,
}

impl OtlpSignal {
    fn path(self) -> &'static str {
        match self {
            OtlpSignal::Metrics => "/v1/metrics",
            OtlpSignal::Logs => "/v1/logs",
        }
    }
}

/// One received body queued for forwarding. `content_type`/`content_encoding` are the inbound
/// headers, preserved for raw forwarding (so a compressed or protobuf tee stays byte-faithful);
/// enriched re-sends uncompressed `application/json`.
struct Outbound {
    signal: OtlpSignal,
    body: Bytes,
    content_type: Option<String>,
    content_encoding: Option<String>,
}

/// A cheap-clone handle: enqueue bodies and signal drain. Cloned into the receiver's request
/// state; dropping all clones (or calling `shutdown`) ends the drain task.
#[derive(Clone)]
pub struct Exporter {
    tx: mpsc::Sender<Outbound>,
    dropped: Arc<AtomicU64>,
    /// Bytes currently queued (added on enqueue, subtracted as the drain task consumes), so the
    /// queue is memory-bounded, not just slot-bounded.
    queued_bytes: Arc<AtomicUsize>,
    shutdown: Arc<Notify>,
}

impl Exporter {
    /// Build the client, channel, and drain task. Returns the handle plus the task's
    /// `JoinHandle` so the receiver can await a bounded drain on shutdown.
    pub fn spawn(targets: Vec<ExportTarget>, state_dir: PathBuf) -> (Exporter, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(EXPORT_QUEUE_CAP);
        let dropped = Arc::new(AtomicU64::new(0));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(Notify::new());
        let worker = Worker::new(targets, state_dir, queued_bytes.clone());
        let handle = tokio::spawn(worker.run(rx, shutdown.clone()));
        (
            Exporter {
                tx,
                dropped,
                queued_bytes,
                shutdown,
            },
            handle,
        )
    }

    /// Queue a received body for all destinations — non-blocking. A full queue (by slot count or
    /// memory) drops the batch and counts it: a permanent downstream undercount, surfaced to
    /// stderr, never a blocked tool call.
    pub fn enqueue(
        &self,
        signal: OtlpSignal,
        body: Bytes,
        content_type: Option<String>,
        content_encoding: Option<String>,
    ) {
        let len = body.len();
        // Atomically reserve the bytes, or drop if it would breach the memory ceiling — a hard
        // bound even under concurrent enqueues (no check-then-add race).
        let reserved =
            self.queued_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    (cur.saturating_add(len) <= MAX_QUEUED_BYTES).then(|| cur + len)
                });
        if reserved.is_err() {
            self.record_drop();
            return;
        }
        let out = Outbound {
            signal,
            body,
            content_type,
            content_encoding,
        };
        if self.tx.try_send(out).is_err() {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed); // release: nothing was queued
            self.record_drop();
        }
    }

    fn record_drop(&self) {
        let before = self.dropped.fetch_add(1, Ordering::Relaxed);
        if crate::throttle::should_log(before, 1, DROP_LOG_EVERY) {
            eprintln!(
                "hatel: export queue full — dropped {} batch(es) so far (downstream slow/unreachable)",
                before + 1
            );
        }
    }

    /// Tell the drain task to flush what's queued and exit. Pair with awaiting the `JoinHandle`
    /// under a timeout so a dead downstream can't hang shutdown.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }
}

/// Resolution of a batch's project set, for project-filtered destinations.
#[derive(Debug)]
enum BatchProjects {
    /// Every `session.id` in the body resolved to a project.
    Resolved(BTreeSet<(String, String)>),
    /// The body carries session ids but the listed ones aren't in the index yet — the
    /// SessionStart hook's index append may simply not have landed, so the batch is worth
    /// parking until *those* sessions appear rather than dropping it forever.
    Unindexed(BTreeSet<String>),
    /// Structurally unattributable — not JSON, or no `session.id` anywhere. No index catch-up
    /// can resolve it, so a filtered destination fails closed immediately.
    Unattributable,
}

/// One body parked because a `session.id` it carries wasn't in the index yet. Its bytes stay
/// reserved in `queued_bytes` until it is finally forwarded or dropped.
struct Deferred {
    out: Outbound,
    /// The session ids that failed to resolve at deferral time. The body is forwarded once ALL
    /// of them have reached the index — an unrelated session's append releases nothing — and is
    /// dropped (fail closed, counted) once `DEFER_TIMEOUT` passes without them, or on the
    /// shutdown pass.
    missing: BTreeSet<String>,
    /// On tokio's clock, so the run loop can sleep precisely until the earliest deadline and
    /// tests can drive the timeout deterministically with a paused clock.
    deferred_at: tokio::time::Instant,
}

/// Which destinations a forwarding pass addresses. A fresh, fully-resolved body goes to every
/// target (`All`); a body parked while its session was unindexed reaches the unfiltered targets
/// immediately (`UnfilteredOnly`) and its retry then addresses only the filtered ones
/// (`FilteredOnly`) — so no target ever receives the same body twice.
#[derive(Clone, Copy)]
enum TargetSet {
    All,
    UnfilteredOnly,
    FilteredOnly,
}

struct Worker {
    targets: Vec<ExportTarget>,
    client: reqwest::Client,
    index: SessionIndexCache,
    queued_bytes: Arc<AtomicUsize>,
    /// Whether any target enriches or filters by project — both need the session→project map, so
    /// either gates the per-batch index refresh; a config with neither does no session-index I/O.
    index_needed: bool,
    /// Endpoints whose enrich-skip has already been logged, so a steady non-JSON stream warns once
    /// per destination rather than once per batch.
    enrich_skip_warned: HashSet<String>,
    /// Bodies awaiting their sessions' index arrival before the filtered targets can have them —
    /// each forwarded once its `missing` set resolves, or dropped at its deadline (see `Deferred`).
    deferred: Vec<Deferred>,
    /// Batches dropped for the filtered destination(s) because their session never appeared in
    /// the index — surfaced to stderr like the queue's drop accounting.
    unresolved_dropped: u64,
}

impl Worker {
    fn new(targets: Vec<ExportTarget>, state_dir: PathBuf, queued_bytes: Arc<AtomicUsize>) -> Self {
        // The only way this fails is a broken TLS backend (the rustls provider is compiled in), so
        // a fallback `Client::new()` would fail identically — fail loud instead of pretending.
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client (rustls) initialises");
        let index_needed = targets
            .iter()
            .any(|t| t.mode == ExportMode::Enriched || t.filter.is_filtered());
        Worker {
            targets,
            client,
            index: SessionIndexCache::new(state_dir),
            queued_bytes,
            index_needed,
            enrich_skip_warned: HashSet::new(),
            deferred: Vec::new(),
            unresolved_dropped: 0,
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Outbound>, shutdown: Arc<Notify>) {
        loop {
            // Wake at the earliest park deadline even with no inbound traffic — a timed-out
            // parked body's reserved bytes must not hold the queue budget hostage on an idle
            // stream. No deadline (nothing parked) sleeps forever; no polling either way.
            let deadline = self
                .deferred
                .iter()
                .map(|d| d.deferred_at + DEFER_TIMEOUT)
                .min();
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(out) => self.handle(out).await,
                    None => break, // all senders dropped
                },
                _ = shutdown.notified() => break,
                _ = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.index_needed {
                        self.index.refresh();
                    }
                    self.retry_deferred(false).await;
                }
            }
        }
        // Drain whatever is already queued before exiting (the caller bounds total time).
        while let Ok(out) = rx.try_recv() {
            self.handle(out).await;
        }
        // Final disposition for parked bodies: one last look at the index, then forward or fail
        // closed — nothing stays parked past shutdown.
        if self.index_needed {
            self.index.refresh();
        }
        self.retry_deferred(true).await;
    }

    /// Process one queued body: refresh the index, retry anything parked against an older index,
    /// then forward this body — parking it instead when a filtered destination needs a project the
    /// index doesn't know *yet*. Bytes are released only on final disposition (forwarded/dropped),
    /// never while parked, so the queue's memory ceiling stays a hard bound.
    async fn handle(&mut self, out: Outbound) {
        // Refresh the session→project map only when something actually enriches or filters, and
        // only when the index file changed since last load (mtime-gated) — no per-batch file read,
        // no read storm on a session that never appears in the index.
        if self.index_needed {
            self.index.refresh();
        }
        self.retry_deferred(false).await;
        if !self.targets.iter().any(|t| t.filter.is_filtered()) {
            self.forward(&out, TargetSet::All, None).await;
            self.release(&out);
            return;
        }
        match self.resolve_batch_projects(&out.body, out.signal) {
            BatchProjects::Resolved(projects) => {
                self.forward(&out, TargetSet::All, Some(&projects)).await;
                self.release(&out);
            }
            BatchProjects::Unindexed(missing) if self.deferred.len() < MAX_DEFERRED => {
                // The OTLP batch raced the SessionStart hook's index append. The unfiltered
                // targets get the body now; the filtered pass is parked until the missing
                // sessions reach the index, instead of being dropped forever.
                self.forward(&out, TargetSet::UnfilteredOnly, None).await;
                self.deferred.push(Deferred {
                    out,
                    missing,
                    deferred_at: tokio::time::Instant::now(),
                });
            }
            BatchProjects::Unindexed(_) => {
                // Park list full — fail closed for the filtered targets, visibly.
                self.forward(&out, TargetSet::UnfilteredOnly, None).await;
                self.record_unresolved_drop();
                self.release(&out);
            }
            BatchProjects::Unattributable => {
                // Not JSON, or no session.id anywhere — no index catch-up can change that, so the
                // filtered targets fail closed now (`doctor` explains a wrong-protocol setup).
                self.forward(&out, TargetSet::UnfilteredOnly, None).await;
                self.release(&out);
            }
        }
    }

    /// Dispose of parked bodies whose wait is over. A body is resolved and forwarded once ALL of
    /// its missing sessions have reached the index — an unrelated session's append never burns
    /// its chance — and is dropped for the filtered targets (fail closed, counted) once
    /// `DEFER_TIMEOUT` passes without them, or unconditionally on the shutdown `final_pass`.
    /// Everything else stays parked, bytes still reserved.
    async fn retry_deferred(&mut self, final_pass: bool) {
        if self.deferred.is_empty() {
            return;
        }
        for d in std::mem::take(&mut self.deferred) {
            let ready = d.missing.iter().all(|sid| self.index.contains(sid));
            if !final_pass && !ready && d.deferred_at.elapsed() < DEFER_TIMEOUT {
                self.deferred.push(d);
                continue;
            }
            match self.resolve_batch_projects(&d.out.body, d.out.signal) {
                BatchProjects::Resolved(projects) => {
                    self.forward(&d.out, TargetSet::FilteredOnly, Some(&projects))
                        .await;
                }
                _ => self.record_unresolved_drop(),
            }
            self.release(&d.out);
        }
    }

    /// Release a body's bytes from the queue's memory accounting — called exactly once per body,
    /// at its final disposition.
    fn release(&self, out: &Outbound) {
        self.queued_bytes
            .fetch_sub(out.body.len(), Ordering::Relaxed);
    }

    /// Count a batch the filtered destination(s) never received because its session never made it
    /// into the index; log the first and then once per `DROP_LOG_EVERY` (mirrors `record_drop`).
    fn record_unresolved_drop(&mut self) {
        let before = self.unresolved_dropped;
        self.unresolved_dropped += 1;
        if crate::throttle::should_log(before, 1, DROP_LOG_EVERY) {
            eprintln!(
                "hatel: {} batch(es) so far had no indexed session — dropped for the \
                 project-filtered destination(s) (fail closed)",
                self.unresolved_dropped
            );
        }
    }

    /// Forward one body to the destinations selected by `set`. `projects` is the batch's resolved
    /// project set; a filtered destination forwards only when every project passes its filter and
    /// fails closed when the set is absent.
    async fn forward(
        &mut self,
        out: &Outbound,
        set: TargetSet,
        projects: Option<&BTreeSet<(String, String)>>,
    ) {
        // Disjoint field borrows: the loop reads targets/client/index and mutates the warned set.
        let index = &self.index;
        let client = &self.client;
        let targets = &self.targets;
        let warned = &mut self.enrich_skip_warned;
        // The enriched body is identical for every enriched target — the project label is resolved
        // from the index, not the target — so it is built at most once per forward and reused.
        let mut enriched: Option<Result<Bytes, String>> = None;
        for target in targets {
            let filtered = target.filter.is_filtered();
            let addressed = match set {
                TargetSet::All => true,
                TargetSet::UnfilteredOnly => !filtered,
                TargetSet::FilteredOnly => filtered,
            };
            if !addressed {
                continue;
            }
            if filtered {
                let allowed = projects.is_some_and(|ps| {
                    ps.iter()
                        .all(|(label, key)| target.filter.allows(label, key))
                });
                if !allowed {
                    continue; // project not allowed here, or undeterminable → fail closed
                }
            }
            let url = format!("{}{}", target.endpoint, out.signal.path());
            // (payload, content-type, content-encoding-to-forward)
            let prepared: Option<(reqwest::Body, &str, Option<&str>)> = match target.mode {
                ExportMode::Raw => Some((
                    out.body.clone().into(),
                    out.content_type.as_deref().unwrap_or("application/json"),
                    out.content_encoding.as_deref(),
                )),
                ExportMode::Enriched => {
                    let body = enriched.get_or_insert_with(|| {
                        let resolve = |sid: &str| index.label(sid);
                        match out.signal {
                            OtlpSignal::Metrics => enrich_metrics(&out.body, &resolve),
                            OtlpSignal::Logs => enrich_logs(&out.body, &resolve),
                        }
                        .map(Bytes::from)
                    });
                    match body {
                        // Enriched output is freshly serialized JSON — no inbound encoding carries over.
                        Ok(bytes) => Some((bytes.clone().into(), "application/json", None)),
                        Err(e) => {
                            // Can't enrich (e.g. the inbound body isn't JSON). Skip this
                            // destination — never forward a non-enriched stream in its place. Warn
                            // once per endpoint so a steady non-JSON stream doesn't spam stderr.
                            if warned.insert(target.endpoint.clone()) {
                                eprintln!(
                                    "hatel: export enrich failed for {} ({e}) — skipping its batches \
                                     (enriched needs http/json; see `hatel doctor`)",
                                    target.endpoint
                                );
                            }
                            None
                        }
                    }
                }
            };
            let Some((payload, content_type, content_encoding)) = prepared else {
                continue;
            };
            let timeout = Duration::from_millis(target.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
            let mut req = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .timeout(timeout)
                .body(payload);
            if let Some(enc) = content_encoding {
                req = req.header(reqwest::header::CONTENT_ENCODING, enc);
            }
            for (k, v) in &target.headers {
                req = req.header(k, v);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    eprintln!(
                        "hatel: export to {} returned {}",
                        target.endpoint,
                        resp.status()
                    )
                }
                Err(e) => eprintln!("hatel: export to {} failed: {e}", target.endpoint),
            }
        }
    }

    /// The `(label, key)` of every project a batch belongs to, for project-filtered destinations.
    /// `Resolved` only when every `session.id` in the body is known; the two failure shapes are
    /// kept distinct because they behave differently — `Unindexed` (sessions not in the index
    /// *yet*, all of them listed) earns a park until those sessions appear, while
    /// `Unattributable` (not JSON, or no `session.id` at all) fails closed immediately.
    /// Resolution is over the `session.id`s actually present; in practice every Claude Code
    /// datapoint carries one and a single export body is a single session, so the set is one
    /// project the filter then accepts or rejects.
    fn resolve_batch_projects(&self, body: &[u8], signal: OtlpSignal) -> BatchProjects {
        let Some(ids) = session_ids(body, matches!(signal, OtlpSignal::Metrics)) else {
            return BatchProjects::Unattributable;
        };
        if ids.is_empty() {
            return BatchProjects::Unattributable;
        }
        let mut projects = BTreeSet::new();
        let mut missing = BTreeSet::new();
        for sid in ids {
            match self.index.project(&sid) {
                Some((label, key)) => {
                    projects.insert((label.to_string(), key.to_string()));
                }
                None => {
                    missing.insert(sid);
                }
            }
        }
        if missing.is_empty() {
            BatchProjects::Resolved(projects)
        } else {
            BatchProjects::Unindexed(missing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch state dir unique to this test (pid-scoped, tag-disambiguated).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ht-export-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stub downstream OTLP collector on a loopback port, recording every body it receives.
    async fn stub_collector() -> (
        std::net::SocketAddr,
        Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::Router;
        use axum::routing::post;
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = Router::new().route(
            "/v1/metrics",
            post({
                let received = received.clone();
                move |body: Bytes| {
                    let received = received.clone();
                    async move {
                        received.lock().unwrap().push(body.to_vec());
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, received, server)
    }

    #[test]
    fn enqueue_never_blocks_and_counts_drops_past_capacity() {
        // A receiver whose worker never drains: try_send fills the channel, then every further
        // enqueue is dropped and counted — and none of them block.
        let (tx, _rx) = mpsc::channel::<Outbound>(2);
        let exp = Exporter {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(Notify::new()),
        };
        for _ in 0..10 {
            exp.enqueue(OtlpSignal::Metrics, Bytes::from_static(b"{}"), None, None);
        }
        assert!(
            exp.dropped.load(Ordering::Relaxed) >= 8,
            "excess beyond capacity is dropped"
        );
    }

    // Force the mtime forward so the test doesn't depend on filesystem timestamp granularity.
    fn filetime_bump(path: &std::path::Path) {
        use std::time::{Duration, SystemTime};
        let future = SystemTime::now() + Duration::from_secs(10);
        // set_modified is stable via File::set_modified.
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(future).unwrap();
    }

    #[test]
    fn batch_projects_resolves_known_and_fails_closed_on_unknown() {
        let dir = scratch("filter");
        std::fs::write(
            dir.join("session_index.jsonl"),
            "{\"session_id\":\"S1\",\"project_key\":\"/k/alpha\",\"project_label\":\"alpha\"}\n",
        )
        .unwrap();
        let mut worker = Worker::new(vec![], dir.clone(), Arc::new(AtomicUsize::new(0)));
        worker.index.refresh();
        let body = |sid: &str| {
            serde_json::to_vec(&serde_json::json!({
                "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                    "sum": { "dataPoints": [{
                        "attributes": [{ "key": "session.id", "value": { "stringValue": sid } }]
                    }]}
                }]}]}]
            }))
            .unwrap()
        };
        // Known session → its (label, key); the allow/exclude decision is then plain set logic.
        match worker.resolve_batch_projects(&body("S1"), OtlpSignal::Metrics) {
            BatchProjects::Resolved(p) => assert_eq!(
                p,
                BTreeSet::from([("alpha".to_string(), "/k/alpha".to_string())])
            ),
            other => panic!("known session should resolve, got {other:?}"),
        }
        // An unknown-but-plausible session is `Unindexed` carrying exactly the missing ids; a
        // body with no session.id at all, or a non-JSON body, is `Unattributable` (fails closed
        // immediately).
        match worker.resolve_batch_projects(&body("GHOST"), OtlpSignal::Metrics) {
            BatchProjects::Unindexed(missing) => {
                assert_eq!(missing, BTreeSet::from(["GHOST".to_string()]))
            }
            other => panic!("unknown session is unindexed (parkable), got {other:?}"),
        }
        assert!(
            matches!(
                worker.resolve_batch_projects(b"\x00proto", OtlpSignal::Metrics),
                BatchProjects::Unattributable
            ),
            "non-JSON is unattributable (never retried)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mixed_project_batch_is_forwarded_only_when_every_project_passes() {
        // A batch spanning two sessions resolves to both projects; an allow-list missing one of
        // them rejects the whole batch (fail closed), so a disallowed session can never ride along
        // with an allowed one.
        let dir = scratch("mixed");
        std::fs::write(
            dir.join("session_index.jsonl"),
            "{\"session_id\":\"S1\",\"project_key\":\"/k/alpha\",\"project_label\":\"alpha\"}\n\
             {\"session_id\":\"S2\",\"project_key\":\"/k/beta\",\"project_label\":\"beta\"}\n",
        )
        .unwrap();
        let mut worker = Worker::new(vec![], dir.clone(), Arc::new(AtomicUsize::new(0)));
        worker.index.refresh();
        let body = serde_json::to_vec(&serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{ "sum": { "dataPoints": [
                { "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }] },
                { "attributes": [{ "key": "session.id", "value": { "stringValue": "S2" } }] }
            ]}}]}]}]
        }))
        .unwrap();
        let projects = match worker.resolve_batch_projects(&body, OtlpSignal::Metrics) {
            BatchProjects::Resolved(p) => p,
            other => panic!("both sessions are indexed, got {other:?}"),
        };
        assert_eq!(projects.len(), 2, "both sessions resolved");
        let only_alpha =
            hatel_core::ProjectFilter::Only(["alpha".to_string()].into_iter().collect());
        let passes = projects
            .iter()
            .all(|(label, key)| only_alpha.allows(label, key));
        assert!(
            !passes,
            "a batch with a disallowed project (beta) is rejected as a whole"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A metrics body carrying one datapoint for `sid`.
    fn metrics_body(sid: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": 1, "dataPoints": [{
                    "attributes": [{ "key": "session.id", "value": { "stringValue": sid } }],
                    "asInt": "10"
                }]}
            }]}]}]
        }))
        .unwrap()
    }

    fn outbound(body: Vec<u8>) -> Outbound {
        Outbound {
            signal: OtlpSignal::Metrics,
            body: Bytes::from(body),
            content_type: Some("application/json".to_string()),
            content_encoding: None,
        }
    }

    #[tokio::test]
    async fn an_unindexed_batch_is_parked_then_forwarded_after_the_index_catches_up() {
        let (addr, received, server) = stub_collector().await;
        let dir = scratch("park");
        let target = ExportTarget {
            endpoint: format!("http://{addr}"),
            mode: ExportMode::Raw,
            filter: hatel_core::ProjectFilter::Only(["alpha".to_string()].into_iter().collect()),
            headers: std::collections::BTreeMap::new(),
            timeout_ms: Some(2_000),
        };
        let qb = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::new(vec![target], dir.clone(), qb.clone());

        // The batch arrives before the SessionStart hook indexed S1 — it must be parked for the
        // filtered destination, not dropped.
        let first = metrics_body("S1");
        qb.fetch_add(first.len(), Ordering::Relaxed);
        worker.handle(outbound(first)).await;
        assert_eq!(worker.deferred.len(), 1, "unindexed batch parked");
        assert!(
            received.lock().unwrap().is_empty(),
            "nothing forwarded while unresolved"
        );

        // The SessionStart hook lands; the next batch triggers the parked retry.
        let path = dir.join("session_index.jsonl");
        std::fs::write(
            &path,
            "{\"session_id\":\"S1\",\"project_key\":\"/k/alpha\",\"project_label\":\"alpha\"}\n",
        )
        .unwrap();
        filetime_bump(&path);
        let second = metrics_body("S1");
        qb.fetch_add(second.len(), Ordering::Relaxed);
        worker.handle(outbound(second)).await;

        assert!(worker.deferred.is_empty(), "parked batch disposed");
        assert_eq!(
            received.lock().unwrap().len(),
            2,
            "the parked batch AND the fresh batch both reached the filtered destination"
        );
        assert_eq!(
            qb.load(Ordering::Relaxed),
            0,
            "all bytes released at disposition"
        );
        server.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unrelated_session_append_does_not_burn_a_parked_body() {
        let (addr, received, server) = stub_collector().await;
        let dir = scratch("unrelated");
        let allow: std::collections::BTreeSet<String> =
            ["alpha".to_string(), "beta".to_string()].into();
        let target = ExportTarget {
            endpoint: format!("http://{addr}"),
            mode: ExportMode::Raw,
            filter: hatel_core::ProjectFilter::Only(allow),
            headers: std::collections::BTreeMap::new(),
            timeout_ms: Some(2_000),
        };
        let qb = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::new(vec![target], dir.clone(), qb.clone());
        let reserve = |body: Vec<u8>| {
            qb.fetch_add(body.len(), Ordering::Relaxed);
            outbound(body)
        };

        // RACER's batch beats its SessionStart append — parked.
        worker.handle(reserve(metrics_body("RACER"))).await;
        assert_eq!(worker.deferred.len(), 1);

        // An UNRELATED session (OTHER) reaches the index first. Its batch forwards, but the
        // parked RACER body must STAY parked — an unrelated append must not burn its chance.
        let path = dir.join("session_index.jsonl");
        let other =
            "{\"session_id\":\"OTHER\",\"project_key\":\"/k/beta\",\"project_label\":\"beta\"}\n";
        std::fs::write(&path, other).unwrap();
        filetime_bump(&path);
        worker.handle(reserve(metrics_body("OTHER"))).await;
        assert_eq!(received.lock().unwrap().len(), 1, "OTHER forwarded");
        assert_eq!(
            worker.deferred.len(),
            1,
            "RACER still parked after an unrelated index advance"
        );
        assert_eq!(worker.unresolved_dropped, 0, "nothing dropped");

        // RACER's own append lands — the next batch flushes the parked body.
        std::fs::write(
            &path,
            format!(
                "{other}{}",
                "{\"session_id\":\"RACER\",\"project_key\":\"/k/alpha\",\"project_label\":\"alpha\"}\n"
            ),
        )
        .unwrap();
        filetime_bump(&path);
        worker.handle(reserve(metrics_body("OTHER"))).await;
        assert!(worker.deferred.is_empty(), "RACER disposed");
        assert_eq!(
            received.lock().unwrap().len(),
            3,
            "parked RACER and both OTHER batches all delivered"
        );
        assert_eq!(qb.load(Ordering::Relaxed), 0, "all bytes released");
        server.abort();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_worker_purges_timed_out_parked_bodies_by_itself() {
        // No traffic after the park and no shutdown: the run loop's deadline arm alone must
        // wake, fail the body closed, and release its reserved bytes — parked bodies can never
        // hold the queue budget hostage on an idle stream. (Paused clock: sleeps auto-advance.)
        let dir = scratch("idle");
        let target = ExportTarget {
            endpoint: "http://127.0.0.1:9".to_string(), // never contacted
            mode: ExportMode::Raw,
            filter: hatel_core::ProjectFilter::Only(["alpha".to_string()].into_iter().collect()),
            headers: std::collections::BTreeMap::new(),
            timeout_ms: Some(500),
        };
        let (exporter, handle) = Exporter::spawn(vec![target], dir.clone());
        exporter.enqueue(
            OtlpSignal::Metrics,
            Bytes::from(metrics_body("GHOST")),
            None,
            None,
        );
        assert!(
            exporter.queued_bytes.load(Ordering::Relaxed) > 0,
            "bytes reserved on enqueue"
        );
        // Cross the deadline; then poll briefly for the worker's own wakeup to release.
        tokio::time::sleep(DEFER_TIMEOUT + Duration::from_secs(2)).await;
        let mut released = false;
        for _ in 0..50 {
            if exporter.queued_bytes.load(Ordering::Relaxed) == 0 {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(released, "idle worker released the timed-out parked bytes");
        exporter.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn parked_batches_fail_closed_at_shutdown_when_never_indexed() {
        let dir = scratch("parkdrop");
        // The endpoint is never contacted: an unresolved batch is dropped, not forwarded.
        let target = ExportTarget {
            endpoint: "http://127.0.0.1:9".to_string(),
            mode: ExportMode::Raw,
            filter: hatel_core::ProjectFilter::Only(["alpha".to_string()].into_iter().collect()),
            headers: std::collections::BTreeMap::new(),
            timeout_ms: Some(500),
        };
        let qb = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::new(vec![target], dir.clone(), qb.clone());
        let body = metrics_body("GHOST");
        qb.fetch_add(body.len(), Ordering::Relaxed);
        worker.handle(outbound(body)).await;
        assert_eq!(worker.deferred.len(), 1, "parked while plausibly racing");

        // Shutdown pass: the session never appeared — fail closed, count it, release the bytes.
        worker.index.refresh();
        worker.retry_deferred(true).await;
        assert!(worker.deferred.is_empty(), "nothing stays parked");
        assert_eq!(worker.unresolved_dropped, 1, "the drop is counted");
        assert_eq!(qb.load(Ordering::Relaxed), 0, "bytes released");
        std::fs::remove_dir_all(&dir).ok();
    }

    // End-to-end: a real receiver→exporter→downstream hop. Spins a stub OTLP collector on a
    // loopback port, forwards an enriched metrics batch, and asserts the downstream received the
    // body with `project` injected (transform + queue + HTTP client all exercised together).
    #[tokio::test]
    async fn enriched_forward_injects_project_to_downstream() {
        use axum::Router;
        use axum::routing::post;
        use tokio::sync::oneshot;

        let dir = scratch("e2e");
        std::fs::write(
            dir.join("session_index.jsonl"),
            "{\"session_id\":\"S1\",\"project_key\":\"/k/alpha\",\"project_label\":\"alpha\"}\n",
        )
        .unwrap();

        // Stub downstream: hand the first received body back through a channel.
        let (tx, rx) = oneshot::channel::<Vec<u8>>();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/v1/metrics",
            post(move |body: Bytes| {
                let tx = tx.clone();
                async move {
                    if let Some(tx) = tx.lock().unwrap().take() {
                        let _ = tx.send(body.to_vec());
                    }
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let target = ExportTarget {
            endpoint: format!("http://{addr}"),
            mode: ExportMode::Enriched,
            filter: hatel_core::ProjectFilter::All,
            headers: std::collections::BTreeMap::new(),
            timeout_ms: Some(2_000),
        };
        let (exporter, handle) = Exporter::spawn(vec![target], dir.clone());

        let body = serde_json::to_vec(&serde_json::json!({
            "resourceMetrics": [{ "scopeMetrics": [{ "metrics": [{
                "name": "claude_code.token.usage",
                "sum": { "aggregationTemporality": 1, "dataPoints": [{
                    "attributes": [{ "key": "session.id", "value": { "stringValue": "S1" } }],
                    "asInt": "10"
                }]}
            }]}]}]
        }))
        .unwrap();
        exporter.enqueue(
            OtlpSignal::Metrics,
            Bytes::from(body),
            Some("application/json".to_string()),
            None,
        );

        let received = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("downstream received a body within the timeout")
            .unwrap();
        exporter.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.abort();
        std::fs::remove_dir_all(&dir).ok();

        let v: serde_json::Value = serde_json::from_slice(&received).unwrap();
        let dp = &v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0];
        let project = dp["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "project")
            .and_then(|a| a["value"]["stringValue"].as_str());
        assert_eq!(
            project,
            Some("alpha"),
            "downstream got the project-enriched body"
        );
    }
}
