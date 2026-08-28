//! Schema loading — core (embedded) plus plugin TOML files, merged into one
//! `Registry`. This is the single path through which any Kind enters the system,
//! so the no-collision guarantee holds uniformly for core and plugins.

use serde::{Deserialize, Serialize};

use crate::registry::{HookBinding, KindSpec, KindSpecRaw, Registry};
use crate::{Config, Error, Result, sink};

const CORE_TOML: &str = include_str!("../core.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaFile {
    #[serde(default)]
    tracked_metrics: Vec<String>,
    #[serde(default)]
    counted_events: Vec<String>,
    #[serde(default)]
    kind: Vec<KindSpecRaw>,
    #[serde(default)]
    binding: Vec<HookBinding>,
}

fn merge(reg: &mut Registry, src: &str, path: &str, is_core: bool) -> Result<()> {
    let file: SchemaFile = toml::from_str(src).map_err(|e| Error::SchemaParse {
        path: path.to_string(),
        source: e,
    })?;
    reg.tracked_metrics.extend(file.tracked_metrics);
    reg.counted_events.extend(file.counted_events);
    for raw in file.kind {
        // `receiver_sourced` is core-only: the receiver writes only the Kinds it has a native
        // handler for. A plugin setting it would declare a Kind that nothing writes (no handler)
        // and that can't be hook-bound either — a dead extension point — so reject it loudly.
        if raw.receiver_sourced && !is_core {
            return Err(Error::InvalidSpec {
                name: raw.name.clone(),
                reason:
                    "receiver_sourced is core-only — a plugin Kind has no native receiver source"
                        .to_string(),
            });
        }
        reg.add_kind(KindSpec::from_raw(raw)?)?;
    }
    for binding in file.binding {
        reg.bind(binding)?;
    }
    Ok(())
}

pub fn load_core() -> Result<Registry> {
    let mut reg = Registry::new();
    merge(&mut reg, CORE_TOML, "<core>", true)?;
    Ok(reg)
}

fn load_plugin(reg: &mut Registry, path: &std::path::Path) -> Result<()> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| Error::Io(format!("read plugin {}: {e}", path.display())))?;
    merge(reg, &src, &path.display().to_string(), false)
}

/// Core schema plus every plugin, merged in order. Strict: any error fails the whole
/// build (used by `serve` / `report` / `kinds` / `doctor`, where loud is correct).
pub fn build_registry(cfg: &Config) -> Result<Registry> {
    let mut reg = load_core()?;
    for path in &cfg.plugins {
        load_plugin(&mut reg, path)?;
    }
    Ok(reg)
}

/// Resilient build for the hook: core must load, then each plugin is merged into a
/// throwaway copy first — a bad plugin is logged and skipped (never silencing core or
/// the other plugins, and never leaving a half-merged registry), the rest still load.
pub fn build_registry_resilient(cfg: &Config) -> Registry {
    let mut reg = match load_core() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("hatel: core schema error ({e})");
            return Registry::default();
        }
    };
    for path in &cfg.plugins {
        let mut trial = reg.clone();
        match load_plugin(&mut trial, path) {
            Ok(()) => reg = trial,
            Err(e) => eprintln!("hatel: skipping plugin {} ({e})", path.display()),
        }
    }
    reg
}

/// The other half of [`build_registry`]: the Kinds the store holds that no loaded schema declares.
/// Their collection worked and every query is blind to them, which no check of the wiring can
/// surface — nothing about the wiring is wrong. Carried by every surface that presents a Kind
/// inventory or an answer computed over one, so a partial registry never reads as a complete one.
#[derive(Debug, Clone, Serialize)]
pub struct UnreadableKinds {
    pub names: Vec<String>,
    /// The surface that would have to list their schema, from [`crate::config::PluginSource`].
    pub plugin_source: String,
}

impl UnreadableKinds {
    /// `None` when every stored Kind is declared. A store that cannot be enumerated is an error
    /// rather than an empty answer: "nothing was collected" and "nothing could be looked at" are
    /// the two answers a diagnostic must never confuse.
    pub fn detect(reg: &Registry, cfg: &Config) -> std::io::Result<Option<Self>> {
        let names: Vec<String> = sink::stored_kinds(cfg)?
            .into_iter()
            .filter(|k| reg.kind(k).is_none())
            .collect();
        Ok((!names.is_empty()).then(|| Self {
            names,
            plugin_source: cfg.plugin_source.label(),
        }))
    }

    /// [`Self::detect`] for the paths whose job is to answer rather than to diagnose: an
    /// unenumerable store is noted on stderr and read as no gap, the same fail-open the sink read
    /// path takes, so one broken store does not turn every query into an error.
    pub fn detect_resilient(reg: &Registry, cfg: &Config) -> Option<Self> {
        match Self::detect(reg, cfg) {
            Ok(found) => found,
            Err(e) => {
                eprintln!("hatel: cannot enumerate stored Kinds ({e})");
                None
            }
        }
    }
}

impl std::fmt::Display for UnreadableKinds {
    /// One sentence — the gap, its consequence, and the surface that closes it — so a diagnostic,
    /// a report caveat and a Kind listing prescribe the same fix in the same words.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the ledger holds {}, which no loaded schema declares — those records stay \
             uncountable until a plugin that declares them is listed in {}",
            self.names.join(", "),
            self.plugin_source
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_top_level_key() {
        // `tracked_metric` (singular) or a `[[kindz]]` typo must fail here — the single loader is
        // the one place such a mistake can be caught, and silently dropping it would no-op a whole
        // Kind or metric list with no diagnostic.
        let r: std::result::Result<SchemaFile, _> = toml::from_str("tracked_metric = []");
        assert!(r.is_err(), "unknown top-level schema key must be rejected");
    }

    #[test]
    fn core_schema_loads_under_deny_unknown_fields() {
        // Guard: the strict schema must still accept the embedded core TOML in full.
        assert!(load_core().is_ok());
    }
}
