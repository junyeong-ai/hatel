//! Export configuration — the collector's egress destinations. Parsed from a small TOML
//! file (`$HATEL_CONFIG`, else `<config-dir>/hatel/config.toml`) that only the receiver
//! (`serve`/`doctor`/`init`) reads — never the hook. Each `[[export]]` entry is one
//! downstream OTLP collector and the transform applied on the way there.
//!
//! A/B selection is modelled as a per-destination transform, not two toggles: `raw` forwards
//! the incoming OTLP byte-verbatim; `enriched` injects the `project` label (joined from
//! `session.id`). Two destinations with one transform each compose cleanly; the same endpoint
//! with both transforms would double-count delta metrics downstream, so a duplicate endpoint is
//! rejected at load rather than silently run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// Whether a *known* project is forwarded to this destination. The unknown-project case
    /// (session not yet joined) is decided by the caller, which fails closed for a filtered target.
    pub fn allows(&self, project: &str) -> bool {
        match self {
            ProjectFilter::All => true,
            ProjectFilter::Only(set) => set.contains(project),
            ProjectFilter::Except(set) => !set.contains(project),
        }
    }

    /// Whether this destination restricts by project at all (used to decide if a batch's project
    /// must be resolved before forwarding).
    pub fn is_filtered(&self) -> bool {
        !matches!(self, ProjectFilter::All)
    }
}

/// One validated downstream destination.
#[derive(Debug, Clone)]
pub struct ExportTarget {
    /// OTLP/HTTP base endpoint; `/v1/metrics` and `/v1/logs` are appended per signal.
    pub endpoint: String,
    pub mode: ExportMode,
    /// Which projects reach this destination (default `All`).
    pub filter: ProjectFilter,
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

// ── the raw TOML shapes ──

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    export: Vec<TargetRaw>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetRaw {
    endpoint: String,
    mode: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Allow-list: forward only these projects. Mutually exclusive with `exclude_projects`.
    #[serde(default)]
    projects: BTreeSet<String>,
    /// Exclude-list: forward every project but these. Mutually exclusive with `projects`.
    #[serde(default)]
    exclude_projects: BTreeSet<String>,
}

impl ExportConfig {
    /// Load from `$HATEL_CONFIG`, else `<config-dir>/hatel/config.toml`. A missing file means
    /// export is simply off (empty), never an error. A present-but-broken file is a hard error —
    /// the receiver should fail fast on a misconfiguration rather than silently drop a destination
    /// the operator asked for. This is only ever called by the receiver, so a bad export config
    /// can never affect the hook.
    pub fn load() -> Result<ExportConfig> {
        let Some(path) = config_path() else {
            return Ok(ExportConfig::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ExportConfig::default());
            }
            Err(e) => {
                return Err(Error::Io(format!(
                    "read export config {}: {e}",
                    path.display()
                )));
            }
        };
        Self::parse(&text, &path.display().to_string())
    }

    /// Parse and validate one config file's text. Validation is loud: an empty endpoint, an
    /// unknown mode, or a duplicate endpoint (which would double-count) is rejected here.
    fn parse(text: &str, path: &str) -> Result<ExportConfig> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| Error::ExportParse {
            path: path.to_string(),
            source: e,
        })?;
        let mut targets = Vec::with_capacity(file.export.len());
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for raw in file.export {
            let endpoint = normalize_endpoint(&raw.endpoint);
            if endpoint.is_empty() {
                return Err(Error::InvalidExport(
                    "an [[export]] target has an empty endpoint".to_string(),
                ));
            }
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
            let filter = match (raw.projects.is_empty(), raw.exclude_projects.is_empty()) {
                (true, true) => ProjectFilter::All,
                (false, true) => ProjectFilter::Only(raw.projects),
                (true, false) => ProjectFilter::Except(raw.exclude_projects),
                (false, false) => {
                    return Err(Error::InvalidExport(format!(
                        "export endpoint {endpoint:?}: set either `projects` (allow-list) or \
                         `exclude_projects`, not both"
                    )));
                }
            };
            targets.push(ExportTarget {
                endpoint,
                mode,
                filter,
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

    /// Write the config to `$HATEL_CONFIG`, else `<config-dir>/hatel/config.toml`. The file may
    /// hold downstream auth headers, so it is written owner-only (`0o600`) via a temp file and an
    /// atomic rename — never a partial or world-readable file. Used by `init --insert`. Returns
    /// the path written.
    pub fn save(&self) -> Result<PathBuf> {
        let path =
            config_path().ok_or_else(|| Error::Io("no config directory for hatel".to_string()))?;
        let body = self.to_toml()?;
        write_private_atomic(&path, &body)
            .map_err(|e| Error::Io(format!("write {}: {e}", path.display())))?;
        Ok(path)
    }

    fn to_toml(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Out<'a> {
            export: Vec<TargetOut<'a>>,
        }
        #[derive(Serialize)]
        struct TargetOut<'a> {
            endpoint: &'a str,
            mode: &'a str,
            #[serde(skip_serializing_if = "BTreeSet::is_empty")]
            projects: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "BTreeSet::is_empty")]
            exclude_projects: &'a BTreeSet<String>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            headers: &'a BTreeMap<String, String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            timeout_ms: &'a Option<u64>,
        }
        const EMPTY: &BTreeSet<String> = &BTreeSet::new();
        let out = Out {
            export: self
                .targets
                .iter()
                .map(|t| {
                    let (projects, exclude_projects) = match &t.filter {
                        ProjectFilter::All => (EMPTY, EMPTY),
                        ProjectFilter::Only(s) => (s, EMPTY),
                        ProjectFilter::Except(s) => (EMPTY, s),
                    };
                    TargetOut {
                        endpoint: &t.endpoint,
                        mode: t.mode.as_str(),
                        projects,
                        exclude_projects,
                        headers: &t.headers,
                        timeout_ms: &t.timeout_ms,
                    }
                })
                .collect(),
        };
        toml::to_string(&out).map_err(|e| Error::Io(format!("serialize export config: {e}")))
    }
}

/// Normalize an endpoint to its canonical form: trim surrounding whitespace and any trailing
/// slashes, so equivalent spellings (`http://x:4318` and `http://x:4318/`) compare equal — for
/// dedup at load, for the URL the exporter builds, and for `doctor`'s route comparison.
pub fn normalize_endpoint(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

/// Write `body` to `path` owner-only and atomically: a temp sibling created `0o600`, fsynced,
/// then renamed over the target — so a crash can't leave a partial file and the secrets a config
/// may hold never sit world-readable. Mirrors `init`'s settings writer and `cost`'s snapshot
/// writer.
fn write_private_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        // Re-assert 0o600 after open: the `mode` above only applies when the file is freshly
        // created, so reusing a leftover temp (same-pid crash) would keep its old mode otherwise.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// The config file path: `$HATEL_CONFIG` (an empty value is treated as unset), else the XDG
/// config dir. Distinct from the *state* dir (the ledger/db live there) — config belongs in the
/// config dir.
fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HATEL_CONFIG").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    use etcetera::BaseStrategy as _;
    etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.config_dir().join("hatel").join("config.toml"))
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
    use super::*;

    #[test]
    fn parses_a_two_target_file() {
        let cfg = ExportConfig::parse(
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
            "<test>",
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
        assert!(
            ExportConfig::parse("", "<test>")
                .unwrap()
                .targets
                .is_empty()
        );
    }

    #[test]
    fn unknown_mode_is_a_hard_error() {
        let err = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"tee\"\n",
            "<test>",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn empty_endpoint_is_a_hard_error() {
        let err = ExportConfig::parse("[[export]]\nendpoint = \"\"\nmode = \"raw\"\n", "<test>");
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn duplicate_endpoint_is_rejected_to_prevent_double_count() {
        let err = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n\
             [[export]]\nendpoint = \"http://x:4318\"\nmode = \"enriched\"\n",
            "<test>",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn trailing_slash_does_not_evade_duplicate_detection() {
        // `http://x:4318` and `http://x:4318/` resolve to the same destination — they must dedup.
        let err = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://x:4318\"\nmode = \"raw\"\n\
             [[export]]\nendpoint = \"http://x:4318/\"\nmode = \"enriched\"\n",
            "<test>",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
        // and a single normalized endpoint is stored without the trailing slash
        let cfg = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://x:4318/\"\nmode = \"raw\"\n",
            "<test>",
        )
        .unwrap();
        assert_eq!(cfg.targets[0].endpoint, "http://x:4318");
    }

    #[test]
    fn malformed_toml_is_an_error_not_empty() {
        assert!(matches!(
            ExportConfig::parse("[[export]\nendpoint =", "<test>"),
            Err(Error::ExportParse { .. })
        ));
    }

    #[test]
    fn upsert_replaces_by_endpoint() {
        let mut cfg = ExportConfig::default();
        cfg.upsert(ExportTarget {
            endpoint: "http://x:4318".into(),
            mode: ExportMode::Raw,
            filter: ProjectFilter::All,
            headers: BTreeMap::new(),
            timeout_ms: None,
        });
        cfg.upsert(ExportTarget {
            endpoint: "http://x:4318".into(),
            mode: ExportMode::Enriched,
            filter: ProjectFilter::All,
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
            filter: ProjectFilter::All,
            headers: parse_otlp_headers("authorization=tok, x-team=core"),
            timeout_ms: Some(3000),
        });
        let back = ExportConfig::parse(&cfg.to_toml().unwrap(), "<roundtrip>").unwrap();
        assert_eq!(back.targets.len(), 1);
        assert_eq!(back.targets[0].endpoint, "http://corp:4318");
        assert_eq!(back.targets[0].mode, ExportMode::Enriched);
        assert_eq!(back.targets[0].headers.get("authorization").unwrap(), "tok");
        assert_eq!(back.targets[0].headers.get("x-team").unwrap(), "core");
        assert_eq!(back.targets[0].timeout_ms, Some(3000));
    }

    #[test]
    fn allow_list_round_trips_and_filters() {
        let cfg = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"enriched\"\n\
             projects = [\"work-a\", \"work-b\"]\n",
            "<test>",
        )
        .unwrap();
        let f = &cfg.targets[0].filter;
        assert!(f.is_filtered());
        assert!(f.allows("work-a") && f.allows("work-b"));
        assert!(!f.allows("personal"));
        // survives a serialize/parse round-trip
        let back = ExportConfig::parse(&cfg.to_toml().unwrap(), "<rt>").unwrap();
        assert_eq!(back.targets[0].filter, cfg.targets[0].filter);
    }

    #[test]
    fn exclude_list_forwards_all_but_named() {
        let cfg = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             exclude_projects = [\"personal\"]\n",
            "<test>",
        )
        .unwrap();
        let f = &cfg.targets[0].filter;
        assert!(f.allows("work-a"));
        assert!(!f.allows("personal"));
        let back = ExportConfig::parse(&cfg.to_toml().unwrap(), "<rt>").unwrap();
        assert_eq!(back.targets[0].filter, cfg.targets[0].filter);
    }

    #[test]
    fn a_misspelled_filter_key_is_rejected_not_silently_ignored() {
        // `project` (singular) would otherwise deserialize to an empty allow-list → forward
        // everything, silently defeating the filter. An unknown key must fail loud.
        let err = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\nproject = [\"work\"]\n",
            "<test>",
        );
        assert!(matches!(err, Err(Error::ExportParse { .. })));
    }

    #[test]
    fn allow_and_exclude_together_is_rejected() {
        let err = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             projects = [\"a\"]\nexclude_projects = [\"b\"]\n",
            "<test>",
        );
        assert!(matches!(err, Err(Error::InvalidExport(_))));
    }

    #[test]
    fn no_filter_is_all_and_allows_everything() {
        let cfg = ExportConfig::parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n",
            "<test>",
        )
        .unwrap();
        let f = &cfg.targets[0].filter;
        assert_eq!(*f, ProjectFilter::All);
        assert!(!f.is_filtered());
        assert!(f.allows("anything"));
    }

    #[test]
    fn parse_otlp_headers_splits_pairs_and_trims() {
        let h = parse_otlp_headers("authorization=Bearer abc, tenant = acme ,broken");
        assert_eq!(h.get("authorization").unwrap(), "Bearer abc");
        assert_eq!(h.get("tenant").unwrap(), "acme");
        assert!(!h.contains_key("broken"), "a pair without `=` is skipped");
    }
}
