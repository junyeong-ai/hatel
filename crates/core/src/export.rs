//! Export configuration — the collector's egress destinations, validated from the `[[export]]`
//! section of the configuration file (see [`crate::settings`]). Only the receiver
//! (`serve`/`doctor`/`init`) reads it — never the hook. Each entry is one downstream OTLP
//! collector and the transform applied on the way there.
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
/// or its unique **key** (the absolute git-root path) — so two repositories that share a basename
/// can be told apart by writing the path. Matching on the key never weakens privacy: the key is
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

    /// A human-readable summary for `serve`'s forwarding line and `doctor` — `only: a, b` /
    /// `except: c`, or `None` for an unfiltered destination — so both surfaces describe a
    /// destination's project policy the same way.
    pub fn describe(&self) -> Option<String> {
        let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
        match self {
            ProjectFilter::All => None,
            ProjectFilter::Only(set) => Some(format!("only: {}", list(set))),
            ProjectFilter::Except(set) => Some(format!("except: {}", list(set))),
        }
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

impl ExportConfig {
    /// Validate the configuration file's `[[export]]` section. A missing file means export is
    /// simply off (empty), never an error. Only ever called by the receiver, so a bad export
    /// destination can never affect the hook.
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
        let mut targets = Vec::with_capacity(raws.len());
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for raw in raws {
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
            // A present-but-empty list is a config mistake, not a fail-open `All`: reject it so an
            // empty allow-list never silently forwards everything. Absent (`None`) is no filter.
            // An empty/whitespace entry is rejected for the same reason — no real project has an
            // empty label or key, so the entry could only ever match by accident.
            let nonempty = |set: BTreeSet<String>, what: &str| -> Result<BTreeSet<String>> {
                if set.is_empty() {
                    Err(Error::InvalidExport(format!(
                        "export endpoint {endpoint:?}: `{what}` is present but empty — list the \
                         projects, or remove the key (an empty list has no useful meaning)"
                    )))
                } else if set.iter().any(|e| e.trim().is_empty()) {
                    Err(Error::InvalidExport(format!(
                        "export endpoint {endpoint:?}: `{what}` contains an empty entry — every \
                         entry must be a project label or git-root path"
                    )))
                } else {
                    Ok(set)
                }
            };
            let filter = match (raw.projects, raw.exclude_projects) {
                (None, None) => ProjectFilter::All,
                (Some(allow), None) => ProjectFilter::Only(nonempty(allow, "projects")?),
                (None, Some(deny)) => ProjectFilter::Except(nonempty(deny, "exclude_projects")?),
                (Some(_), Some(_)) => {
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
                let (projects, exclude_projects) = match &t.filter {
                    ProjectFilter::All => (None, None),
                    ProjectFilter::Only(s) => (Some(s.clone()), None),
                    ProjectFilter::Except(s) => (None, Some(s.clone())),
                };
                ExportTargetRaw {
                    endpoint: t.endpoint.clone(),
                    mode: t.mode.as_str().to_string(),
                    projects,
                    exclude_projects,
                    timeout_ms: t.timeout_ms,
                    headers: t.headers.clone(),
                }
            })
            .collect()
    }
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
            filter: ProjectFilter::All,
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
        let f = &cfg.targets[0].filter;
        assert!(f.is_filtered());
        assert!(f.allows("work-a", "/repos/work-a") && f.allows("work-b", "/x/work-b"));
        assert!(!f.allows("personal", "/repos/personal"));
        // survives a serialize/parse round-trip
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets[0].filter, cfg.targets[0].filter);
    }

    #[test]
    fn exclude_list_forwards_all_but_named() {
        let cfg = parse(
            "[[export]]\nendpoint = \"http://corp:4318\"\nmode = \"raw\"\n\
             exclude_projects = [\"personal\"]\n",
        )
        .unwrap();
        let f = &cfg.targets[0].filter;
        assert!(f.allows("work-a", "/repos/work-a"));
        assert!(!f.allows("personal", "/repos/personal"));
        let back = parse(&cfg.to_toml()).unwrap();
        assert_eq!(back.targets[0].filter, cfg.targets[0].filter);
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
        let f = &cfg.targets[0].filter;
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
        for key in ["projects", "exclude_projects"] {
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
        let f = &cfg.targets[0].filter;
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
}
