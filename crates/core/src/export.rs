//! Export configuration — the collector's egress destinations, validated from the `[[export]]`
//! section of the configuration file (see [`crate::settings`]). Writers reserve delivery rows
//! with local records; the receiver performs network delivery. Each entry selects native
//! signals, local Kinds and the transform for one downstream OTLP collector.
//!
//! A/B selection is modelled as a per-destination transform, not two toggles: `raw` forwards
//! the incoming OTLP byte-verbatim; `enriched` injects the `project` label (joined from
//! `session.id`). Two destinations with one transform each compose cleanly; the same endpoint
//! with both transforms would double-count delta metrics downstream, so a duplicate endpoint is
//! rejected at load rather than silently run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::settings::{ExportTargetRaw, Settings};
use crate::{Error, Result};

/// The transform applied to a destination's stream. `Raw` is the absence of a transform
/// (byte-verbatim forward); `Enriched` injects the project label per datapoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportMode {
    Raw,
    Enriched,
}

impl ExportMode {
    /// Parse the TOML `mode` value. Mirrors `SinkKind::parse` — an unknown value is `None`,
    /// surfaced as a loud config error rather than a silent default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "raw" => Some(Self::Raw),
            "enriched" => Some(Self::Enriched),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Enriched => "enriched",
        }
    }
}

/// Which projects a destination accepts. A destination forwards a batch only when its project
/// (joined from `session.id`) passes this filter — so a project's telemetry can be kept off a
/// downstream (e.g. a personal project off the corporate collector). The type encodes the
/// "at most one of allow/exclude" invariant: a config setting both is rejected at load.
///
/// An entry matches a project by its display **label** (the git-root basename, e.g. `my-app`)
/// or its unique **key** (the absolute git-root path, as the filesystem resolves it) — so two
/// repositories that share a basename can be told apart by writing the path. Matching on the key
/// never weakens privacy: the key is
/// only read here, for the local forward/skip decision, and is never part of an egressed body
/// (enrichment injects the label alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectFilter {
    /// No filter — forward every project (the default).
    All,
    /// Forward only these projects (allow-list; secure-by-default — a new project is excluded
    /// until listed).
    Only(BTreeSet<String>),
    /// Forward every project except these (exclude-list).
    Except(BTreeSet<String>),
}

impl ProjectFilter {
    /// Whether a *known* project (identified by both its `label` and unique `key`) is forwarded to
    /// this destination — an entry matching either identifier counts. The unknown-project case
    /// (session not yet joined) is decided by the caller, which fails closed for a filtered target.
    pub fn allows(&self, label: &str, key: &str) -> bool {
        let listed = |set: &BTreeSet<String>| set.contains(label) || set.contains(key);
        match self {
            ProjectFilter::All => true,
            ProjectFilter::Only(set) => listed(set),
            ProjectFilter::Except(set) => !listed(set),
        }
    }

    /// Whether this destination restricts by project at all (used to decide if a batch's project
    /// must be resolved before forwarding).
    pub fn is_filtered(&self) -> bool {
        !matches!(self, ProjectFilter::All)
    }

    fn describe(&self) -> Option<String> {
        match self {
            ProjectFilter::All => None,
            ProjectFilter::Only(set) => Some(format!("projects only: {}", join(set))),
            ProjectFilter::Except(set) => Some(format!("projects except: {}", join(set))),
        }
    }
}

/// Which Claude accounts a destination accepts, named by sign-in email. Claude Code reports the
/// signed-in account's email as `user.email` on its native datapoints and log records, so a batch
/// passes only when every record names a listed account. Local records carry no account, which is
/// why a destination that filters accounts takes native signals only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountFilter {
    /// No filter — forward every account (the default).
    All,
    /// Forward only these accounts.
    Only(BTreeSet<String>),
}

impl AccountFilter {
    /// Whether an account, by its sign-in email, is forwarded to this destination. Addresses
    /// compare ignoring ASCII case, so a capitalization difference between the listed and the
    /// reported address cannot silently stop a destination's flow.
    pub fn allows(&self, email: &str) -> bool {
        match self {
            AccountFilter::All => true,
            AccountFilter::Only(set) => set.iter().any(|listed| listed.eq_ignore_ascii_case(email)),
        }
    }

    pub fn is_filtered(&self) -> bool {
        !matches!(self, AccountFilter::All)
    }

    fn describe(&self) -> Option<String> {
        match self {
            AccountFilter::All => None,
            AccountFilter::Only(set) => Some(format!("accounts only: {}", join(set))),
        }
    }
}

fn join(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// One validated downstream destination.
#[derive(Debug, Clone)]
pub struct ExportTarget {
    /// OTLP/HTTP base endpoint; `/v1/metrics` and `/v1/logs` are appended per signal.
    pub endpoint: String,
    pub mode: ExportMode,
    pub signals: BTreeSet<String>,
    pub kinds: BTreeSet<String>,
    pub retry_attempts: u32,
    pub queue_days: u32,
    pub queue_bytes: u64,
    /// Which projects reach this destination (default `All`).
    pub projects: ProjectFilter,
    /// Which Claude accounts reach this destination (default `All`).
    pub accounts: AccountFilter,
    /// Extra request headers (e.g. a downstream's `authorization`). Never logged by value.
    pub headers: BTreeMap<String, String>,
    /// Per-request timeout in milliseconds; `None` uses the receiver default.
    pub timeout_ms: Option<u64>,
}

/// The validated set of destinations. Empty = export off.
#[derive(Debug, Clone, Default)]
pub struct ExportConfig {
    pub targets: Vec<ExportTarget>,
}

impl ExportConfig {
    /// Validate the configuration file's `[[export]]` section. A missing file means export is
    /// simply off (empty), never an error.
    pub fn load() -> Result<ExportConfig> {
        Self::from_settings(&Settings::load()?)
    }

    /// Validate against settings already read — the same single-observation path [`crate::Config`]
    /// offers, so one command need not read the configuration file twice to see both views.
    pub fn from_settings(settings: &Settings) -> Result<ExportConfig> {
        Self::validate(settings.export.clone())
    }

    /// Turn file entries into destinations. Validation is loud: an empty endpoint, an unknown
    /// mode, or a duplicate endpoint (which would double-count) is rejected here.
    fn validate(raws: Vec<ExportTargetRaw>) -> Result<ExportConfig> {
        if raws.len() > 32 {
            return Err(Error::InvalidExport(
                "at most 32 destinations are supported".into(),
            ));
        }
        let mut targets = Vec::with_capacity(raws.len());
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for raw in raws {
            let endpoint = normalize_endpoint(&raw.endpoint);
            if endpoint.is_empty() {
                return Err(Error::InvalidExport(
                    "an [[export]] target has an empty endpoint".to_string(),
                ));
            }
            let parsed = url::Url::parse(&endpoint).map_err(|_| {
                Error::InvalidExport("endpoint must be an absolute HTTP(S) URL".into())
            })?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(Error::InvalidExport("endpoint must be HTTP(S), without userinfo, query or fragment; use headers for credentials".into()));
            }
            if raw.timeout_ms == Some(0) || raw.timeout_ms.is_some_and(|t| t > 300000) {
                return Err(Error::InvalidExport("timeout_ms must be 1..300000".into()));
            }
            for (name, value) in &raw.headers {
                http::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Error::InvalidExport("invalid HTTP header name".into()))?;
                http::HeaderValue::from_str(value)
                    .map_err(|_| Error::InvalidExport("invalid HTTP header value".into()))?;
            }
            let endpoint = parsed.to_string().trim_end_matches('/').to_string();
            let mode = ExportMode::parse(&raw.mode).ok_or_else(|| {
                Error::InvalidExport(format!(
                    "export endpoint {endpoint:?}: unknown mode {:?} (expected raw|enriched)",
                    raw.mode
                ))
            })?;
            if !seen.insert(endpoint.clone()) {
                return Err(Error::InvalidExport(format!(
                    "duplicate export endpoint {endpoint:?} — each destination takes one transform; \
                     two to the same endpoint would double-count delta metrics"
                )));
            }
            let projects = match (raw.projects, raw.exclude_projects) {
                (None, None) => ProjectFilter::All,
                (Some(allow), None) => {
                    ProjectFilter::Only(PROJECT_ENTRY.list(&endpoint, "projects", allow)?)
                }
                (None, Some(deny)) => ProjectFilter::Except(PROJECT_ENTRY.list(
                    &endpoint,
                    "exclude_projects",
                    deny,
                )?),
                (Some(_), Some(_)) => {
                    return Err(Error::InvalidExport(format!(
                        "export endpoint {endpoint:?}: set either `projects` (allow-list) or \
                         `exclude_projects`, not both"
                    )));
                }
            };
            let accounts = match raw.accounts {
                None => AccountFilter::All,
                Some(emails) => {
                    AccountFilter::Only(ACCOUNT_ENTRY.list(&endpoint, "accounts", emails)?)
                }
            };
            if accounts.is_filtered() && !raw.kinds.is_empty() {
                return Err(Error::InvalidExport(format!(
                    "export endpoint {endpoint:?}: `accounts` applies to native signals only — \
                     local records carry no Claude account, so send `kinds` to a destination \
                     without `accounts`"
                )));
            }
            let signals = raw.signals.unwrap_or_else(default_signals);
            if signals
                .iter()
                .any(|s| !matches!(s.as_str(), "metrics" | "logs"))
            {
                return Err(Error::InvalidExport(
                    "signals must contain only metrics or logs".into(),
                ));
            }
            if raw
                .kinds
                .iter()
                .any(|k| !crate::registry::is_valid_kind_name(k))
            {
                return Err(Error::InvalidExport(
                    "kinds must be exact registered names".into(),
                ));
            }
            if signals.is_empty() && raw.kinds.is_empty() {
                return Err(Error::InvalidExport(
                    "target selects no signals or kinds".into(),
                ));
            }
            let retry_attempts = raw.retry_attempts.unwrap_or(8);
            let queue_days = raw.queue_days.unwrap_or(7);
            let queue_bytes = raw.queue_bytes.unwrap_or(64 * 1024 * 1024);
            if !(1..=100).contains(&retry_attempts)
                || !(1..=90).contains(&queue_days)
                || !(1024..=1024 * 1024 * 1024).contains(&queue_bytes)
            {
                return Err(Error::InvalidExport("retry_attempts must be 1..100, queue_days 1..90 and queue_bytes 1024..1073741824".into()));
            }
            targets.push(ExportTarget {
                endpoint,
                mode,
                signals,
                kinds: raw.kinds,
                retry_attempts,
                queue_days,
                queue_bytes,
                projects,
                accounts,
                headers: raw.headers,
                timeout_ms: raw.timeout_ms,
            });
        }
        Ok(ExportConfig { targets })
    }

    /// Add or replace a destination by endpoint (idempotent — re-inserting the same endpoint
    /// updates its transform/headers rather than duplicating it). The endpoint is normalized so it
    /// dedups against an equivalent form (e.g. a trailing slash). Used by `init --insert`.
    pub fn upsert(&mut self, mut target: ExportTarget) {
        target.endpoint = normalize_endpoint(&target.endpoint);
        match self
            .targets
            .iter_mut()
            .find(|t| t.endpoint == target.endpoint)
        {
            Some(slot) => *slot = target,
            None => self.targets.push(target),
        }
    }

    /// Persist these destinations into the configuration file, leaving every other section as it
    /// was found. Used by `init --insert`. Returns the path written.
    pub fn save(&self) -> Result<PathBuf> {
        self.merged_into(Settings::load()?).save()
    }

    /// `settings` with its export section replaced by these destinations. Writing the file means
    /// rewriting all of it, so the sections this config does not own are carried through here
    /// rather than reconstructed — a writer that built a fresh file would silently drop them.
    fn merged_into(&self, mut settings: Settings) -> Settings {
        settings.export = self.to_raw();
        settings
    }

    /// This config alone as configuration-file text — the shape a test round-trips through.
    #[cfg(test)]
    fn to_toml(&self) -> String {
        let mut settings = Settings::default();
        settings.export = self.to_raw();
        toml::to_string_pretty(&settings).unwrap()
    }

    fn to_raw(&self) -> Vec<ExportTargetRaw> {
        self.targets
            .iter()
            .map(|t| {
                let (projects, exclude_projects) = match &t.projects {
                    ProjectFilter::All => (None, None),
                    ProjectFilter::Only(s) => (Some(s.clone()), None),
                    ProjectFilter::Except(s) => (None, Some(s.clone())),
                };
                let accounts = match &t.accounts {
                    AccountFilter::All => None,
                    AccountFilter::Only(s) => Some(s.clone()),
                };
                ExportTargetRaw {
                    endpoint: t.endpoint.clone(),
                    mode: t.mode.as_str().to_string(),
                    signals: (t.signals != default_signals()).then(|| t.signals.clone()),
                    kinds: t.kinds.clone(),
                    retry_attempts: (t.retry_attempts != 8).then_some(t.retry_attempts),
                    queue_days: (t.queue_days != 7).then_some(t.queue_days),
                    queue_bytes: (t.queue_bytes != 64 * 1024 * 1024).then_some(t.queue_bytes),
                    projects,
                    exclude_projects,
                    accounts,
                    timeout_ms: t.timeout_ms,
                    headers: t.headers.clone(),
                }
            })
            .collect()
    }
}

pub fn default_signals() -> BTreeSet<String> {
    ["metrics".into(), "logs".into()].into()
}

impl ExportTarget {
    pub fn allows_record(&self, env: &crate::Envelope) -> bool {
        self.kinds.contains(&env.kind)
            && !env.metadata.projection
            && !self.accounts.is_filtered()
            && match &env.metadata.project {
                Some(project) => self.projects.allows(&project.label, &project.key),
                None => !self.projects.is_filtered(),
            }
    }

    /// The destination's project and account policy — `projects only: a, b, accounts only: c` —
    /// or `None` when it forwards everything; one rendering, so `serve`'s forwarding line and
    /// `doctor` describe a destination the same way.
    pub fn describe_filters(&self) -> Option<String> {
        let parts: Vec<String> = [self.projects.describe(), self.accounts.describe()]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// What a filter list's entries name, and how one is recognized.
struct FilterEntry {
    expected: &'static str,
    valid: fn(&str) -> bool,
}

const PROJECT_ENTRY: FilterEntry = FilterEntry {
    expected: "a project label or git-root path",
    valid: |entry| !entry.trim().is_empty(),
};

const ACCOUNT_ENTRY: FilterEntry = FilterEntry {
    expected: "a sign-in email address",
    valid: is_email_address,
};

impl FilterEntry {
    /// A filter list as configured. A present-but-empty list is a config mistake, not a fail-open
    /// `All`, so an empty allow-list never silently forwards everything; an entry that cannot name
    /// what its key matches is rejected for the same reason, since it could only ever match by
    /// accident, or never.
    fn list(
        &self,
        endpoint: &str,
        key: &str,
        entries: BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        if entries.is_empty() {
            return Err(Error::InvalidExport(format!(
                "export endpoint {endpoint:?}: `{key}` is present but empty — list at least one \
                 entry, or remove the key (an empty list has no useful meaning)"
            )));
        }
        match entries.iter().find(|entry| !(self.valid)(entry)) {
            Some(entry) => Err(Error::InvalidExport(format!(
                "export endpoint {endpoint:?}: `{key}` entry {entry:?} is not {}",
                self.expected
            ))),
            None => Ok(entries),
        }
    }
}

/// Whether `entry` has the shape of an email address: a local part and a domain around its last
/// `@`, with no surrounding whitespace. Deliverability is not judged; the check refuses a domain,
/// a name or a padded copy, which could never equal the address Claude Code reports.
fn is_email_address(entry: &str) -> bool {
    entry.trim() == entry
        && entry
            .rsplit_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
}

/// Normalize an endpoint to its canonical form: trim surrounding whitespace and any trailing
/// slashes, so equivalent spellings (`http://x:4318` and `http://x:4318/`) compare equal — for
/// dedup at load, for the URL the exporter builds, and for `doctor`'s route comparison.
pub fn normalize_endpoint(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

/// Parse Claude Code's `OTEL_EXPORTER_OTLP_HEADERS` (`k1=v1,k2=v2`) into a header map — used by
/// `init --insert` to carry a corporate collector's auth onto the captured forward target. A
/// pair without `=` is skipped; whitespace around keys/values is trimmed.
pub fn parse_otlp_headers(raw: &str) -> BTreeMap<String, String> {
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Parse a configuration file's text and validate its export section — the two steps a real
    /// load performs, composed here so a test states its input as the file the operator writes.
    fn parse(text: &str) -> Result<ExportConfig> {
        ExportConfig::validate(Settings::parse(text, Path::new("config.toml"))?.export)
    }

    #[test]
    fn saving_destinations_keeps_the_sections_export_does_not_own() {
        // `init --insert` rewrites the whole file to change `[[export]]`. A plugin list written by
        // hand must survive that, or wiring up a forward target silently unregisters every custom
        // Kind — which reads as data loss long after the command that caused it.
        let before =
            Settings::parse("plugins = [\"/p/aix.toml\"]", Path::new("config.toml")).unwrap();
        let mut cfg = ExportConfig::default();
        cfg.upsert(ExportTarget {
            endpoint: "http://corp:4318".into(),
            mode: ExportMode::Enriched,
            signals: crate::export::default_signals(),
            kinds: Default::default(),
            retry_attempts: 8,
            queue_days: 7,
            queue_bytes: 64 * 1024 * 1024,
            projects: ProjectFilter::All,
            accounts: AccountFilter::All,
            headers: BTreeMap::new(),
            timeout_ms: None,
        });
        let after = cfg.merged_into(before);
        assert_eq!(after.plugins, vec![PathBuf::from("/p/aix.toml")]);
        assert_eq!(after.export.len(), 1);
        // …and still parses back, so the write is not merely retained but valid.
        let text = toml::to_string_pretty(&after).unwrap();
        let reread = Settings::parse(&text, Path::new("config.toml")).unwrap();
        assert_eq!(reread.plugins, vec![PathBuf::from("/p/aix.toml")]);
        assert_eq!(reread.export[0].endpoint, "http://corp:4318");
    }

    #[test]
    fn parses_a_two_target_file() {
        let cfg = parse(
            r#"
            [[export]]
            endpoint = "http://corp:4318"
            mode = "enriched"
            headers = { authorization = "tok" }

            [[export]]
            endpoint = "http://archive:4318"
            mode = "raw"
            timeout_ms = 2000
            "#,
        )
        .unwrap();
        assert_eq!(cfg.targets.len(), 2);
        assert_eq!(cfg.targets[0].endpoint, "http://corp:4318");
        assert_eq!(cfg.targets[0].mode, ExportMode::Enriched);
        assert_eq!(cfg.targets[0].headers.get("authorization").unwrap(), "tok");
        assert_eq!(cfg.targets[1].mode, ExportMode::Raw);
        assert_eq!(cfg.targets[1].timeout_ms, Some(2000));
    }

    #[test]
    fn empty_file_is_export_off() {
        assert!(parse("").unwrap().targets.is_empty());
    }

    #[test]
    fn unknown_mode_is_a_hard_error() {
        let err = parse("[[export]]\nendpoint = \"http://x:4318\"\nmode = \"tee\"\n");
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn empty_endpoint_is_a_hard_error() {
        let err = parse("[[export]]\nendpoint = \"\"\nmode = \"raw\"\n");
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn duplicate_endpoint_is_rejected_to_prevent_double_count() {
        let err = parse(
            "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n\
             [[export]]\nendpoint = \"http://x:4318\"\nmode = \"enriched\"\n",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn trailing_slash_does_not_evade_duplicate_detection() {
        // `http://x:4318` and `http://x:4318/` resolve to the same destination — they must dedup.
        let err = parse(
            "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n\
             [[export]]\nendpoint = \"http://x:4318/\"\nmode = \"enriched\"\n",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
        // and a single normalized endpoint is stored without the trailing slash
        let cfg = parse("[[export]]\nendpoint = \"http://x:4318/\"\nmode = \"raw\"\n").unwrap();
        assert_eq!(cfg.targets[0].endpoint, "http://x:4318");
    }

    #[test]
    fn malformed_toml_is_an_error_not_empty() {
        assert!(matches!(
            parse("[[export]\nendpoint ="),
            Err(Error::ConfigParse { .. })
        ));
    }

    #[test]
    fn upsert_replaces_by_endpoint() {
        let mut cfg = ExportConfig::default();
        cfg.upsert(ExportTarget {
            endpoint: "http://x:4318".into(),
            mode: ExportMode::Raw,
            signals: crate::export::default_signals(),
            kinds: Default::default(),
            retry_attempts: 8,
            queue_days: 7,
            queue_bytes: 64 * 1024 * 1024,
            projects: ProjectFilter::All,
            accounts: AccountFilter::All,
            headers: BTreeMap::new(),
            timeout_ms: None,
        });
        cfg.upsert(ExportTarget {
            endpoint: "http://x:4318".into(),
            mode: ExportMode::Enriched,
            signals: crate::export::default_signals(),
            kinds: Default::default(),
            retry_attempts: 8,
            queue_days: 7,
            queue_bytes: 64 * 1024 * 1024,
            projects: ProjectFilter::All,
            accounts: AccountFilter::All,
            headers: BTreeMap::new(),
            timeout_ms: None,
        });
        assert_eq!(cfg.targets.len(), 1, "same endpoint updates in place");
        assert_eq!(cfg.targets[0].mode, ExportMode::Enriched);
    }

    #[test]
    fn to_toml_round_trips() {
        let mut cfg = ExportConfig::default();
        cfg.upsert(ExportTarget {
            endpoint: "http://corp:4318".into(),
            mode: ExportMode::Enriched,
            signals: crate::export::default_signals(),
            kinds: Default::default(),
            retry_attempts: 8,
            queue_days: 7,
            queue_bytes: 64 * 1024 * 1024,
            projects: ProjectFilter::All,
            accounts: AccountFilter::All,
            headers: parse_otlp_headers("authorization=tok, x-team=core"),
            timeout_ms: Some(3000),
        });
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets.len(), 1);
        assert_eq!(back.targets[0].endpoint, "http://corp:4318");
        assert_eq!(back.targets[0].mode, ExportMode::Enriched);
        assert_eq!(back.targets[0].headers.get("authorization").unwrap(), "tok");
        assert_eq!(back.targets[0].headers.get("x-team").unwrap(), "core");
        assert_eq!(back.targets[0].timeout_ms, Some(3000));
    }

    #[test]
    fn allow_list_round_trips_and_filters() {
        let cfg = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"enriched\"\n\
             projects = [\"work-a\", \"work-b\"]\n",
        )
        .unwrap();
        let f = &cfg.targets[0].projects;
        assert!(f.is_filtered());
        assert!(f.allows("work-a", "/repos/work-a") && f.allows("work-b", "/x/work-b"));
        assert!(!f.allows("personal", "/repos/personal"));
        // survives a serialize/parse round-trip
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets[0].projects, cfg.targets[0].projects);
    }

    #[test]
    fn exclude_list_forwards_all_but_named() {
        let cfg = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             exclude_projects = [\"personal\"]\n",
        )
        .unwrap();
        let f = &cfg.targets[0].projects;
        assert!(f.allows("work-a", "/repos/work-a"));
        assert!(!f.allows("personal", "/repos/personal"));
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets[0].projects, cfg.targets[0].projects);
    }

    #[test]
    fn a_filter_entry_matches_label_or_unique_key() {
        // Two repos can share a basename; a path entry disambiguates by the unique key while a
        // bare label still matches every project with that basename.
        let cfg = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"enriched\"\n\
             projects = [\"/Users/me/work/api\"]\n",
        )
        .unwrap();
        let f = &cfg.targets[0].projects;
        // same label "api", different keys — only the listed key is allowed.
        assert!(f.allows("api", "/Users/me/work/api"));
        assert!(!f.allows("api", "/Users/me/personal/api"));
    }

    #[test]
    fn a_misspelled_filter_key_is_rejected_not_silently_ignored() {
        // `project` (singular) would otherwise deserialize to an empty allow-list → forward
        // everything, silently defeating the filter. An unknown key must fail loud.
        let err = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\nproject = [\"work\"]\n",
        );
        assert!(matches!(err, Err(Error::ConfigParse { .. })));
    }

    #[test]
    fn a_misspelled_top_level_export_key_is_rejected_not_silently_off() {
        // `[[exports]]` (plural) or any stray top-level key would otherwise leave `export` empty —
        // silently disabling forwarding the operator asked for. The whole file must fail loud.
        let err = parse("[[exports]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n");
        assert!(matches!(err, Err(Error::ConfigParse { .. })));
    }

    #[test]
    fn an_explicit_empty_filter_list_is_rejected_not_fail_open() {
        // `projects = []` must NOT silently become `All` (forward everything) — an empty allow-list
        // is a config mistake. Both an empty allow-list and an empty exclude-list are rejected.
        for key in ["projects", "exclude_projects", "accounts"] {
            let err = parse(&format!(
                "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n{key} = []\n"
            ));
            assert!(
                matches!(err, Err(Error::InvalidExport(_))),
                "{key} = [] should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn an_empty_filter_entry_is_rejected() {
        // `projects = [""]` (or whitespace) can never name a real project — reject it loudly
        // instead of carrying an entry that could only match a broken, label-less record.
        for entry in ["\"\"", "\"  \""] {
            let err = parse(&format!(
                "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\nprojects = [{entry}]\n"
            ));
            assert!(
                matches!(err, Err(Error::InvalidExport(_))),
                "projects = [{entry}] should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn an_account_allow_list_matches_sign_in_emails_ignoring_case() {
        let cfg = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             accounts = [\"Me@Company.example\"]\n",
        )
        .unwrap();
        let accounts = &cfg.targets[0].accounts;
        assert!(accounts.allows("me@company.example"));
        assert!(!accounts.allows("me@personal.example"));
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets[0].accounts, *accounts);
    }

    #[test]
    fn an_account_entry_that_is_no_address_or_a_local_kind_route_is_rejected() {
        // A domain, a padded copy or local Kinds (which carry no account) would otherwise leave
        // the destination silently matching nothing, or never deciding those records.
        for entry in [
            "accounts = [\"company.example\"]",
            "accounts = [\"@company.example\"]",
            "accounts = [\" me@company.example\"]",
            "accounts = [\"me@company.example\"]\nkinds = [\"check\"]",
        ] {
            let err = parse(&format!(
                "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n{entry}\n"
            ));
            assert!(matches!(err, Err(Error::InvalidExport(_))), "{entry}");
        }
    }

    #[test]
    fn allow_and_exclude_together_is_rejected() {
        let err = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             projects = [\"a\"]\nexclude_projects = [\"b\"]\n",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn no_filter_is_all_and_allows_everything() {
        let cfg = parse("[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n").unwrap();
        let f = &cfg.targets[0].projects;
        assert_eq!(*f, ProjectFilter::All);
        assert!(!f.is_filtered());
        assert!(f.allows("anything", "/any/where"));
    }

    #[test]
    fn parse_otlp_headers_splits_pairs_and_trims() {
        let h = parse_otlp_headers("authorization=Bearer abc, tenant = acme ,broken");
        assert_eq!(h.get("authorization").unwrap(), "Bearer abc");
        assert_eq!(h.get("tenant").unwrap(), "acme");
        assert!(!h.contains_key("broken"), "a pair without `=` is skipped");
    }
    #[test]
    fn signal_and_kind_selection_are_explicit_and_invalid_routes_are_rejected() {
        let selected = parse(
            "[[export]]\nendpoint='https://company'\nmode='raw'\nsignals=['metrics']\nkinds=[]\n",
        )
        .unwrap();
        assert_eq!(selected.targets[0].signals, ["metrics".into()].into());
        assert!(selected.targets[0].kinds.is_empty());
        for entry in [
            "signals=['metricz']",
            "signals=[]",
            "kinds=['*']",
            "timeout_ms=0",
            "retry_attempts=0",
        ] {
            assert!(
                parse(&format!(
                    "[[export]]\nendpoint='https://company'\nmode='raw'\n{entry}\n"
                ))
                .is_err(),
                "{entry}"
            );
        }
        for endpoint in [
            "file:///tmp/out",
            "https://user:secret@company",
            "https://company?token=secret",
            "relative",
        ] {
            assert!(parse(&format!("[[export]]\nendpoint={endpoint:?}\nmode='raw'\n")).is_err());
        }
    }
}
