//! The collector's own configuration file — `$HATEL_CONFIG`, else `<config-dir>/hatel/config.toml`.
//! Distinct from the *state* dir (the ledger/db live there) and from Claude Code's
//! `settings.json` (which carries only the native `OTEL_*` block the agent reads at startup).
//!
//! Every section the file may hold is declared on one type. That is what lets
//! `deny_unknown_fields` reject a misspelled key — two parsers over the same file would each
//! see the other's sections as unknown — and what lets a command that manages a single section
//! (`init --insert`, which owns `export`) rewrite the file without dropping the sections it
//! does not manage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The parsed configuration file. An absent file is `default()` — running without one is the
/// normal case, not an error.
///
/// Fields hold the file's own spelling, never a derived form: writing the file back is how one
/// section is edited, so a value that arrived relative must leave relative, or each rewrite would
/// re-anchor it against the config directory again. [`Settings::plugin_paths`] does the anchoring
/// at the point of use instead.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Plugin schema files merged onto the core registry, in listed order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PathBuf>,
    /// Downstream OTLP destinations the receiver tees to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export: Vec<ExportTargetRaw>,
    /// Where this was read from, so relative paths inside it can be anchored. Not part of the
    /// file.
    #[serde(skip)]
    source: PathBuf,
}

/// One `[[export]]` entry exactly as the file spells it, before validation. `Option` on the two
/// project lists distinguishes absent (no filter) from an explicit empty list, which
/// [`crate::ExportConfig`] rejects. `headers` is a sub-table, so it is declared last — TOML
/// requires a table's keys to follow every scalar of its parent.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportTargetRaw {
    pub endpoint: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_projects: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl Settings {
    /// The configuration file's path: `$HATEL_CONFIG` (an empty value is treated as unset), else
    /// the XDG config dir. `None` when the platform exposes no config directory.
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("HATEL_CONFIG").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(p));
        }
        use etcetera::BaseStrategy as _;
        etcetera::choose_base_strategy()
            .ok()
            .map(|s| s.config_dir().join("hatel").join("config.toml"))
    }

    /// Read and parse the file. A missing file yields the default; a present-but-broken one is a
    /// hard error, so a misconfiguration fails fast rather than silently dropping a section the
    /// operator wrote.
    pub fn load() -> Result<Settings> {
        let Some(path) = Self::path() else {
            return Ok(Settings::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(Error::Io(format!("read config {}: {e}", path.display()))),
        };
        Self::parse(&text, &path)
    }

    /// Parse configuration text as if it were read from `path`, which anchors any relative
    /// plugin path.
    pub(crate) fn parse(text: &str, path: &Path) -> Result<Settings> {
        let mut settings: Settings = toml::from_str(text).map_err(|e| Error::ConfigParse {
            path: path.display().to_string(),
            source: e,
        })?;
        settings.source = path.to_path_buf();
        Ok(settings)
    }

    /// The plugin schema files to load, each anchored to the configuration file's own directory
    /// when the file spells it relatively. A config is read by the hook, which is spawned in
    /// whatever project directory Claude Code is working in, so resolving against the working
    /// directory would make one file mean different things per invocation.
    pub fn plugin_paths(&self) -> Vec<PathBuf> {
        let dir = self.source.parent();
        self.plugins
            .iter()
            .map(|plugin| match dir {
                Some(dir) if plugin.is_relative() => dir.join(plugin),
                _ => plugin.clone(),
            })
            .collect()
    }

    /// Write the file, owner-only (`0o600`) via a temp file and an atomic rename — it may hold a
    /// downstream's auth headers, so it is never left partial or world-readable. Returns the path.
    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path().ok_or_else(|| Error::Io("no config directory".to_string()))?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| Error::Io(format!("serialize config: {e}")))?;
        write_private_atomic(&path, &body)
            .map_err(|e| Error::Io(format!("write {}: {e}", path.display())))?;
        Ok(path)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_misspelled_key_in_any_section_is_rejected() {
        // One shape for the whole file is what makes this possible: a second parser that knew
        // only `export` would have to tolerate `plugins`, and so could not reject `pluginz`.
        for text in [
            "pluginz = []",
            "[[exports]]\nendpoint = \"x\"\nmode = \"raw\"",
        ] {
            assert!(
                Settings::parse(text, Path::new("/c/config.toml")).is_err(),
                "{text:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_relative_plugin_path_anchors_to_the_config_directory() {
        let s = Settings::parse(
            "plugins = [\"schemas/aix.toml\", \"/abs/w.toml\"]",
            Path::new("/home/u/.config/hatel/config.toml"),
        )
        .unwrap();
        assert_eq!(
            s.plugin_paths(),
            vec![
                PathBuf::from("/home/u/.config/hatel/schemas/aix.toml"),
                PathBuf::from("/abs/w.toml"),
            ]
        );
    }

    #[test]
    fn rewriting_the_file_never_re_anchors_a_relative_plugin_path() {
        // A section is edited by rewriting the whole file. If the write stored the anchored form,
        // a config reached by a relative path would gain another directory level on every rewrite
        // until the plugin no longer resolves.
        let config = Path::new("cfg/config.toml");
        let mut settings = Settings::parse("plugins = [\"schemas/p.toml\"]", config).unwrap();
        for _ in 0..3 {
            let text = toml::to_string_pretty(&settings).unwrap();
            settings = Settings::parse(&text, config).unwrap();
            assert_eq!(settings.plugins, vec![PathBuf::from("schemas/p.toml")]);
            assert_eq!(
                settings.plugin_paths(),
                vec![PathBuf::from("cfg/schemas/p.toml")]
            );
        }
    }

    #[test]
    fn every_section_survives_a_write_read_round_trip() {
        // `init --insert` rewrites the whole file to change one section; a section it does not
        // manage must come back byte-for-byte in meaning, or the write silently drops config.
        let original = Settings::parse(
            "plugins = [\"/p/a.toml\"]\n\
             [[export]]\nendpoint = \"http://h:4318\"\nmode = \"enriched\"\ntimeout_ms = 900\n\
             [export.headers]\nauthorization = \"t\"\n",
            Path::new("/c/config.toml"),
        )
        .unwrap();
        let text = toml::to_string_pretty(&original).unwrap();
        let again = Settings::parse(&text, Path::new("/c/config.toml")).unwrap();
        assert_eq!(again.plugins, vec![PathBuf::from("/p/a.toml")]);
        assert_eq!(again.export.len(), 1);
        assert_eq!(again.export[0].mode, "enriched");
        assert_eq!(again.export[0].timeout_ms, Some(900));
        assert_eq!(again.export[0].headers.get("authorization").unwrap(), "t");
    }
}
