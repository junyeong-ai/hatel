//! The export runtime: a bounded queue, a single drain task, and an HTTP client that forwards
//! each received OTLP body to every configured destination. Best-effort and fail-open — a slow
//! or unreachable downstream never blocks ingestion (the queue drops and counts under pressure)
//! and is never retried (a retry would inflate downstream delta counters). The API takes only
//! `bytes::Bytes` + `OtlpSignal`, never an axum type, so a future ledger→OTLP-logs exporter can
//! reuse the same `enqueue`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use hatel_core::{ExportMode, ExportTarget, SessionIndex};

use super::transform::{enrich_logs, enrich_metrics, session_ids};

/// Queue depth (slot backstop) before new batches are dropped. The memory bound is `MAX_QUEUED_BYTES`
/// below; this caps slot count so the channel itself stays small.
const EXPORT_QUEUE_CAP: usize = 1024;
/// Memory ceiling for queued bodies — the queue is bounded by *bytes*, not just slot count, so a
/// dead downstream can't accumulate unbounded memory even if individual bodies are large.
const MAX_QUEUED_BYTES: usize = 256 * 1024 * 1024;
/// Per-request timeout when a target doesn't set one.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
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
        let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if n % DROP_LOG_EVERY == 1 {
            eprintln!(
                "hatel: export queue full — dropped {n} batch(es) so far (downstream slow/unreachable)"
            );
        }
    }

    /// Tell the drain task to flush what's queued and exit. Pair with awaiting the `JoinHandle`
    /// under a timeout so a dead downstream can't hang shutdown.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }
}

struct Worker {
    targets: Vec<ExportTarget>,
    client: reqwest::Client,
    index: IndexCache,
    queued_bytes: Arc<AtomicUsize>,
    /// Whether any target enriches or filters by project — both need the session→project map, so
    /// either gates the per-batch index refresh; a config with neither does no session-index I/O.
    index_needed: bool,
    /// Endpoints whose enrich-skip has already been logged, so a steady non-JSON stream warns once
    /// per destination rather than once per batch.
    enrich_skip_warned: HashSet<String>,
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
            index: IndexCache::new(state_dir),
            queued_bytes,
            index_needed,
            enrich_skip_warned: HashSet::new(),
        }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Outbound>, shutdown: Arc<Notify>) {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(out) => self.handle(out).await,
                    None => break, // all senders dropped
                },
                _ = shutdown.notified() => break,
            }
        }
        // Drain whatever is already queued before exiting (the caller bounds total time).
        while let Ok(out) = rx.try_recv() {
            self.handle(out).await;
        }
    }

    /// Forward one queued body, then release its bytes from the memory accounting.
    async fn handle(&mut self, out: Outbound) {
        let len = out.body.len();
        self.forward(out).await;
        self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
    }

    async fn forward(&mut self, out: Outbound) {
        // Refresh the session→project map only when something actually enriches or filters, and
        // only when the index file changed since last load (mtime-gated) — no per-batch file read,
        // no read storm on a session that never appears in the index.
        if self.index_needed {
            self.index.refresh();
        }
        // Resolve the batch's project(s) once, for the destinations that filter by project. `Some`
        // only when every session.id in the body is known; `None` when the project can't be fully
        // determined (a session is unknown, the body carries none, or it isn't JSON) — a filtered
        // destination then fails closed, never leaking an unattributable batch.
        let batch_projects = self
            .targets
            .iter()
            .any(|t| t.filter.is_filtered())
            .then(|| self.resolve_batch_projects(&out.body, out.signal))
            .flatten();
        // Disjoint field borrows: the loop reads targets/client/index and mutates the warned set.
        let index = &self.index;
        let client = &self.client;
        let targets = &self.targets;
        let warned = &mut self.enrich_skip_warned;
        for target in targets {
            if target.filter.is_filtered() {
                let allowed = batch_projects
                    .as_ref()
                    .is_some_and(|ps| ps.iter().all(|p| target.filter.allows(p)));
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
                    let resolve = |sid: &str| index.get(sid);
                    let transformed = match out.signal {
                        OtlpSignal::Metrics => enrich_metrics(&out.body, &resolve),
                        OtlpSignal::Logs => enrich_logs(&out.body, &resolve),
                    };
                    match transformed {
                        // Enriched output is freshly serialized JSON — no inbound encoding carries over.
                        Ok(bytes) => Some((bytes.into(), "application/json", None)),
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

    /// The set of project labels a batch belongs to, for project-filtered destinations. `None`
    /// when the batch can't be attributed — it isn't JSON, carries no `session.id` at all, or one
    /// of its `session.id`s isn't yet in the index — so a filtered destination fails closed rather
    /// than forward an unattributable (possibly out-of-scope) batch. Self-heals: the next batch
    /// after the index catches up resolves and forwards. Resolution is over the `session.id`s
    /// actually present; in practice every Claude Code datapoint carries one and a single export
    /// body is a single session, so the set is one project the filter then accepts or rejects.
    fn resolve_batch_projects(&self, body: &[u8], signal: OtlpSignal) -> Option<BTreeSet<String>> {
        let ids = session_ids(body, matches!(signal, OtlpSignal::Metrics))?;
        if ids.is_empty() {
            return None;
        }
        let mut projects = BTreeSet::new();
        for sid in &ids {
            projects.insert(self.index.get(sid)?); // any unknown session → None (fail closed)
        }
        Some(projects)
    }
}

/// A session→project-label map cached from `session_index.jsonl`, reloaded only when the file's
/// mtime advances. The index is append-only so its mtime increases on each new session; gating on
/// it means a steady stream costs no I/O while a brand-new session is picked up on the next batch.
struct IndexCache {
    state_dir: PathBuf,
    mtime: Option<SystemTime>,
    map: HashMap<String, String>,
}

impl IndexCache {
    fn new(state_dir: PathBuf) -> Self {
        IndexCache {
            state_dir,
            mtime: None,
            map: HashMap::new(),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.state_dir.join("session_index.jsonl")
    }

    fn refresh(&mut self) {
        let file_len = match std::fs::metadata(self.index_path()) {
            Ok(meta) => {
                let modified = meta.modified().ok();
                if modified == self.mtime {
                    return; // unchanged since last load (or both unreadable) — keep the map
                }
                self.mtime = modified; // advance regardless, so we never re-read the same revision
                meta.len()
            }
            // The index was reset/removed — drop the stale map rather than inject obsolete labels.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.mtime = None;
                self.map.clear();
                return;
            }
            // A transient error (permissions, I/O) — keep what we have rather than wrongly clearing.
            Err(_) => return,
        };
        let loaded: HashMap<String, String> = SessionIndex::new(self.state_dir.clone())
            .load()
            .into_iter()
            .filter(|(_, row)| !row.project_label.is_empty())
            .map(|(sid, row)| (sid, row.project_label))
            .collect();
        // Replace the map when the load produced rows, or when the file is genuinely empty. A
        // non-empty file that yields nothing is a transient read race — keep the prior labels
        // rather than clobbering good attribution with an empty snapshot.
        if !loaded.is_empty() || file_len == 0 {
            self.map = loaded;
        }
    }

    /// The project label for a session, or `None` when unknown (never fabricated).
    fn get(&self, session_id: &str) -> Option<String> {
        self.map.get(session_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn index_cache_keeps_labels_when_a_nonempty_file_reads_empty() {
        // A non-empty index that yields no labelled rows is treated as a transient read race — the
        // prior good labels are kept rather than clobbered with an empty snapshot. (Distinct from a
        // removed file, which clears; see the test above.)
        let dir = std::env::temp_dir().join(format!("ht-idxkeep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_index.jsonl");
        std::fs::write(
            &path,
            "{\"session_id\":\"S1\",\"project_key\":\"/k/a\",\"project_label\":\"a\"}\n",
        )
        .unwrap();
        let mut cache = IndexCache::new(dir.clone());
        cache.refresh();
        assert_eq!(cache.get("S1").as_deref(), Some("a"));
        // Rewrite to a non-empty file whose only row has an empty label (yields nothing after the
        // filter), and advance mtime — the guard keeps the prior labels.
        std::fs::write(
            &path,
            "{\"session_id\":\"S9\",\"project_key\":\"/k/\",\"project_label\":\"\"}\n",
        )
        .unwrap();
        filetime_bump(&path);
        cache.refresh();
        assert_eq!(
            cache.get("S1").as_deref(),
            Some("a"),
            "prior labels kept on an empty read"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn index_cache_drops_stale_map_when_index_is_removed() {
        let dir = std::env::temp_dir().join(format!("ht-idxdel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_index.jsonl");
        std::fs::write(
            &path,
            "{\"session_id\":\"S1\",\"project_key\":\"/k/a\",\"project_label\":\"a\"}\n",
        )
        .unwrap();
        let mut cache = IndexCache::new(dir.clone());
        cache.refresh();
        assert_eq!(cache.get("S1").as_deref(), Some("a"));
        // A state reset removes the index — the cache must not keep injecting the old label.
        std::fs::remove_file(&path).unwrap();
        cache.refresh();
        assert_eq!(
            cache.get("S1"),
            None,
            "stale label dropped after index removal"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn index_cache_reloads_only_when_mtime_advances() {
        let dir = std::env::temp_dir().join(format!("ht-idxcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_index.jsonl");
        let line = |sid: &str, label: &str| {
            format!(
                "{{\"session_id\":\"{sid}\",\"project_key\":\"/k/{label}\",\"project_label\":\"{label}\"}}\n"
            )
        };
        std::fs::write(&path, line("S1", "alpha")).unwrap();

        let mut cache = IndexCache::new(dir.clone());
        cache.refresh();
        assert_eq!(cache.get("S1").as_deref(), Some("alpha"));
        assert_eq!(cache.get("S2"), None, "unknown session is not fabricated");

        // Append a new session and bump mtime; a refresh must pick it up.
        std::fs::write(
            &path,
            format!("{}{}", line("S1", "alpha"), line("S2", "beta")),
        )
        .unwrap();
        filetime_bump(&path);
        cache.refresh();
        assert_eq!(cache.get("S2").as_deref(), Some("beta"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Force the mtime forward so the test doesn't depend on filesystem timestamp granularity.
    fn filetime_bump(path: &std::path::Path) {
        use std::time::Duration;
        let future = SystemTime::now() + Duration::from_secs(10);
        // set_modified is stable via File::set_modified.
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(future).unwrap();
    }

    #[test]
    fn batch_projects_resolves_known_and_fails_closed_on_unknown() {
        let dir = std::env::temp_dir().join(format!("ht-filter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        // Known session → its project; the allow/exclude decision is then plain set logic.
        let known = worker.resolve_batch_projects(&body("S1"), OtlpSignal::Metrics);
        assert_eq!(known, Some(BTreeSet::from(["alpha".to_string()])));
        // Unknown session, no session.id, and non-JSON all fail closed (None).
        assert_eq!(
            worker.resolve_batch_projects(&body("GHOST"), OtlpSignal::Metrics),
            None,
            "unknown session fails closed"
        );
        assert_eq!(
            worker.resolve_batch_projects(b"\x00proto", OtlpSignal::Metrics),
            None,
            "non-JSON fails closed"
        );
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

        let dir = std::env::temp_dir().join(format!("ht-export-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
