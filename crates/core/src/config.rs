//! Runtime configuration: the resolved settings every command runs against, assembled from the
//! configuration file (see [`crate::settings`]) and the environment. Never from Claude Code's
//! `settings.json`, which carries only the native `OTEL_*` / `CLAUDE_CODE_ENABLE_TELEMETRY`
//! block the agent reads at startup.

use std::path::PathBuf;

use crate::Result;
use crate::settings::Settings;
use crate::sink::SinkKind;

#[derive(Debug, Clone)]
pub struct Config {
    pub sink: SinkKind,
    /// Root for sink-independent state (the session index, the sqlite db).
    pub state_dir: PathBuf,
    /// Where the JSONL sink writes per-kind ledgers.
    pub ledger_dir: PathBuf,
    /// Plugin schema files merged onto the core registry. Read from the configuration file so
    /// every command resolves the same Kinds — a registry that depended on a process's ambient
    /// environment would let the write path record Kinds the read path cannot see.
    pub plugins: Vec<PathBuf>,
    /// JSONL ledger rotation threshold in bytes (high-volume collectors raise this).
    pub rotate_bytes: u64,
    /// Days of cost-snapshot history to retain. A session whose snapshot is older than
    /// this is pruned on the next merge — bounding the durable file (and the receiver's
    /// per-flush rewrite) at the report horizon. Generous so any realistic report window
    /// is fully covered.
    pub retention_days: i64,
    pub disabled: bool,
    pub strict: bool,
}

/// Default JSONL rotation threshold.
pub const DEFAULT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Default cost-snapshot retention (≫ the default 30-day report window).
pub const DEFAULT_RETENTION_DAYS: i64 = 90;
/// Upper bound on `retention_days`, so `retention_days * 86_400` can never overflow
/// (mirrors `report::MAX_WINDOW_DAYS`); ~273 years, far beyond any real horizon.
pub const MAX_RETENTION_DAYS: i64 = 100_000;

impl Config {
    /// Resolve the configuration, failing on an unreadable or malformed file. Every command that
    /// reads or reports data takes this path: a settings file that cannot be parsed would
    /// otherwise silently yield no plugins, and so an under-reported answer that looks complete.
    pub fn load() -> Result<Self> {
        Ok(Self::from_settings(Settings::load()?))
    }

    /// Resolve the configuration, degrading to the defaults on a broken file with a note on
    /// stderr. For the hook, whose contract is that telemetry never blocks a tool call — the same
    /// asymmetry [`crate::schema::build_registry_resilient`] applies to a broken plugin.
    pub fn load_resilient() -> Self {
        Self::from_settings(Settings::load().unwrap_or_else(|e| {
            eprintln!("hatel: {e}");
            Settings::default()
        }))
    }

    fn from_settings(settings: Settings) -> Self {
        let testing = env_flag("HATEL_TESTING");
        let state_dir = resolve_state_dir(testing);
        let ledger_dir = state_dir.join("ledger");
        let sink = std::env::var("HATEL_SINK")
            .ok()
            .and_then(|s| SinkKind::parse(&s))
            .unwrap_or(SinkKind::Jsonl);
        // `HATEL_PLUGINS` replaces the file's list rather than adding to it, so a shell can pin a
        // registry exactly. Split on the OS path-list separator (`:` on Unix, `;` on Windows), so
        // a native Windows path like `C:\plugins\x.toml` isn't split on its drive colon.
        let plugins = match std::env::var_os("HATEL_PLUGINS") {
            Some(s) => std::env::split_paths(&s)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            None => settings.plugin_paths(),
        };
        let rotate_bytes = std::env::var("HATEL_ROTATE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_ROTATE_BYTES);
        let retention_days = std::env::var("HATEL_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| (1..=MAX_RETENTION_DAYS).contains(n))
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        Config {
            sink,
            state_dir,
            ledger_dir,
            plugins,
            rotate_bytes,
            retention_days,
            disabled: env_flag("HATEL_DISABLED"),
            strict: env_flag("HATEL_STRICT"),
        }
    }
}

fn env_flag(key: &str) -> bool {
    std::env::var(key).map(|v| v == "1").unwrap_or(false)
}

fn resolve_state_dir(testing: bool) -> PathBuf {
    // An empty value is treated as unset (rather than resolving state under the cwd).
    let base = std::env::var("HATEL_STATE_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(xdg_state_dir);
    if testing { base.join("_test") } else { base }
}

fn xdg_state_dir() -> PathBuf {
    use etcetera::BaseStrategy as _;
    match etcetera::choose_base_strategy() {
        Ok(s) => s.state_dir().unwrap_or_else(|| s.data_dir()).join("hatel"),
        Err(_) => PathBuf::from(".hatel"),
    }
}
