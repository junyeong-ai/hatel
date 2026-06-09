//! Schema loading — core (embedded) plus plugin TOML files, merged into one
//! `Registry`. This is the single path through which any Kind enters the system,
//! so the no-collision guarantee holds uniformly for core and plugins.

use serde::Deserialize;

use crate::registry::{HookBinding, KindSpec, KindSpecRaw, Registry};
use crate::{Config, Error, Result};

const CORE_TOML: &str = include_str!("../core.toml");

#[derive(Debug, Deserialize)]
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

fn merge(reg: &mut Registry, src: &str, path: &str) -> Result<()> {
    let file: SchemaFile = toml::from_str(src).map_err(|e| Error::SchemaParse {
        path: path.to_string(),
        source: e,
    })?;
    reg.tracked_metrics.extend(file.tracked_metrics);
    reg.counted_events.extend(file.counted_events);
    for raw in file.kind {
        reg.add_kind(KindSpec::from_raw(raw)?)?;
    }
    for binding in file.binding {
        reg.bind(binding)?;
    }
    Ok(())
}

pub fn load_core() -> Result<Registry> {
    let mut reg = Registry::new();
    merge(&mut reg, CORE_TOML, "<core>")?;
    Ok(reg)
}

fn load_plugin(reg: &mut Registry, path: &std::path::Path) -> Result<()> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| Error::Io(format!("read plugin {}: {e}", path.display())))?;
    merge(reg, &src, &path.display().to_string())
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
