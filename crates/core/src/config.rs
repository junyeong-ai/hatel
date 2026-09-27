//! Runtime configuration: the resolved settings every command runs against, assembled from the
//! configuration file (see [`crate::settings`]) and the environment. Never from Claude Code's
//! `settings.json`, which carries only the native `OTEL_*` / `CLAUDE_CODE_ENABLE_TELEMETRY`
//! block the agent reads at startup.

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::PathBuf;

use crate::settings::Settings;
use crate::sink::SinkKind;
use crate::{Error, Result};

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
    /// Where `plugins` came from, so a diagnostic can name the thing that would have to change.
    pub plugin_source: PluginSource,
    /// JSONL ledger rotation threshold in bytes (high-volume collectors raise this).
    pub rotate_bytes: u64,
    /// Days of history every store retains — see [`Config::retention`]. Generous so any
    /// realistic report window is fully covered.
    pub retention_days: i64,
    pub disabled: bool,
    pub strict: bool,
}

/// Which surface supplied the plugin list. An override is worth naming: advice to edit the
/// configuration file is wrong while an environment variable is replacing what that file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    ConfigFile,
    Environment,
}

impl PluginSource {
    /// The surface to name when prescribing a registry change. One spelling, so a diagnostic and a
    /// query error never send the same operator to two different places.
    pub fn label(self) -> String {
        match self {
            PluginSource::Environment => "HATEL_PLUGINS".to_string(),
            PluginSource::ConfigFile => crate::Settings::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(no config directory)".to_string()),
        }
    }
}

/// Default JSONL rotation threshold.
pub const DEFAULT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Default retention (≫ the default 30-day report window).
pub const DEFAULT_RETENTION_DAYS: i64 = 90;
/// Upper bound on `retention_days`, so `retention_days * 86_400` can never overflow
/// (mirrors `report::MAX_WINDOW_DAYS`); ~273 years, far beyond any real horizon.
pub const MAX_RETENTION_DAYS: i64 = 100_000;
/// How many files a store that deletes whole files spreads one horizon across. Such a file goes
/// only once its newest record expires, so its oldest outlives the horizon by the file's span, plus
/// the delay of the sweeps that rotate and delete it. Ten keeps the span to a tenth of the horizon,
/// at about ten files per slowly filled log.
const FILES_PER_HORIZON: i64 = 10;
/// The longest the receiver's retention sweep waits between runs; it also runs once at startup. A
/// record can outlive the horizon by a file's span plus two sweep intervals, so under a horizon
/// short enough that the rotation span is under a day, the sweep runs once per span instead.
const MAX_SWEEP_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// The retention horizon at one instant, in epoch seconds.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Records from before this have expired.
    pub cutoff: i64,
    /// A store that deletes whole files rotates each open file holding a record from before this,
    /// one rotation span back — see [`Config::rotation_span_secs`].
    pub rotate_before: i64,
}

impl Config {
    /// Resolve the configuration, failing on an unreadable or malformed file or a variable set to a
    /// value it cannot read. Every command that reads or reports data takes this path: a settings
    /// file that cannot be parsed would otherwise silently yield no plugins, and so an
    /// under-reported answer that looks complete.
    pub fn load() -> Result<Self> {
        let (cfg, invalid) = Self::from_settings(&Settings::load()?);
        if invalid.is_empty() {
            Ok(cfg)
        } else {
            let invalid: Vec<String> = invalid.iter().map(ToString::to_string).collect();
            Err(Error::InvalidEnvironment(invalid.join("; ")))
        }
    }

    /// Resolve the configuration, degrading to the defaults on a broken file, and reading a
    /// variable it cannot read as unset, each with a note on stderr. For the hook, whose contract
    /// is that telemetry never blocks a tool call — the same asymmetry
    /// [`crate::schema::build_registry_resilient`] applies to a broken plugin. The defaults include
    /// the store: a file that cannot be read cannot say which store it names, the default is that
    /// store for every file that names none, and for one that does, records kept there can still
    /// be found, where records not written cannot.
    pub fn load_resilient() -> Self {
        let (cfg, invalid) = Self::from_settings(&Settings::load().unwrap_or_else(|e| {
            eprintln!("hatel: {e}");
            Settings::default()
        }));
        for var in invalid {
            eprintln!("hatel: ignoring {var}");
        }
        cfg
    }

    /// Resolve against settings already read, so a command that needs more than one view of the
    /// configuration file reads it once and every view describes the same observation. A variable
    /// set to a value it cannot read resolves as unset, and is returned beside the configuration.
    pub fn from_settings(settings: &Settings) -> (Self, Vec<InvalidVariable>) {
        Self::resolve(settings, &Env::process())
    }

    /// Resolve against settings already read and the variables `lookup` answers for, in place of
    /// this process's environment, as [`Config::from_settings`] does.
    pub fn from_settings_in(
        settings: &Settings,
        lookup: &dyn Fn(&str) -> Option<OsString>,
    ) -> (Self, Vec<InvalidVariable>) {
        Self::resolve(settings, &Env(lookup))
    }

    /// A variable in `env` replaces the file's value, and the file's replaces the default.
    fn resolve(settings: &Settings, env: &Env) -> (Self, Vec<InvalidVariable>) {
        let mut invalid = Vec::new();
        let state_dir = valid(env.state_dir(), &mut invalid)
            .or_else(|| settings.state_dir())
            .unwrap_or_else(xdg_state_dir);
        let state_dir = if valid(env.flag("HATEL_TESTING"), &mut invalid).unwrap_or(false) {
            state_dir.join("_test")
        } else {
            state_dir
        };
        let ledger_dir = state_dir.join("ledger");
        let sink = valid(env.sink(), &mut invalid)
            .or(settings.storage.sink)
            .unwrap_or(SinkKind::Jsonl);
        // `HATEL_PLUGINS` replaces the file's list rather than adding to it, so a shell can pin a
        // registry exactly. An empty value is treated as unset — as `HATEL_CONFIG` is — so an
        // exported-but-blank variable cannot silently unregister every Kind. Split on the OS
        // path-list separator (`:` on Unix, `;` on Windows), so a native Windows path like
        // `C:\plugins\x.toml` isn't split on its drive colon.
        let (plugins, plugin_source) = match env.get("HATEL_PLUGINS").filter(|s| !s.is_empty()) {
            Some(s) => (
                std::env::split_paths(&s)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect(),
                PluginSource::Environment,
            ),
            None => (settings.plugin_paths(), PluginSource::ConfigFile),
        };
        let rotate_bytes = valid(env.rotate_bytes(), &mut invalid)
            .or(settings.storage.rotate_bytes.map(NonZeroU64::get))
            .unwrap_or(DEFAULT_ROTATE_BYTES);
        let retention_days = valid(env.retention_days(), &mut invalid)
            .or(settings.storage.retention_days)
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        let disabled = valid(env.flag("HATEL_DISABLED"), &mut invalid).unwrap_or(false);
        let strict = valid(env.flag("HATEL_STRICT"), &mut invalid).unwrap_or(false);
        let cfg = Config {
            sink,
            state_dir,
            ledger_dir,
            plugins,
            plugin_source,
            rotate_bytes,
            retention_days,
            disabled,
            strict,
        };
        (cfg, invalid)
    }

    /// The retention horizon as of `now_epoch`, one for every store. `retention_days` is capped at
    /// parse time, so the arithmetic cannot overflow.
    pub fn retention(&self, now_epoch: i64) -> Retention {
        Retention {
            cutoff: now_epoch - self.retention_days * 86_400,
            rotate_before: now_epoch - self.rotation_span_secs(),
        }
    }

    /// How long a store that deletes whole files lets one file span before the sweep rotates it: a
    /// tenth of the horizon. A sweep that ran less often would stretch the span to its own interval.
    pub fn rotation_span_secs(&self) -> i64 {
        self.retention_days * 86_400 / FILES_PER_HORIZON
    }

    /// How often the receiver's retention sweep runs.
    pub fn sweep_interval_secs(&self) -> i64 {
        MAX_SWEEP_INTERVAL_SECS.min(self.rotation_span_secs())
    }

    /// The latest a store still holds a record kind whose newest record was written at `written`
    /// (epoch seconds), when nothing writes that kind again and a receiver keeps sweeping: that
    /// record's file is rotated well within the horizon and removed by the first sweep after it.
    pub fn stored_until(&self, written: i64) -> i64 {
        written + self.retention_days * 86_400 + self.sweep_interval_secs()
    }
}

/// The storage variables `lookup` answers with a value that takes effect. Each replaces the
/// configuration file's `[storage]` value only for a process whose environment carries it.
pub fn storage_overrides(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<&'static str> {
    Env(lookup).storage_overrides()
}

/// The variables `lookup` answers with a value configuration cannot read.
pub fn invalid_variables(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<InvalidVariable> {
    Config::resolve(&Settings::default(), &Env(lookup)).1
}

/// An environment variable set to a value configuration cannot read, which it resolves as unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidVariable {
    pub name: &'static str,
    pub value: OsString,
    /// What the value should be instead.
    pub reason: String,
}

impl std::fmt::Display for InvalidVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={:?}: {}", self.name, self.value, self.reason)
    }
}

/// `read`'s value, or `None` once the variable it could not read is recorded in `invalid`.
fn valid<T>(
    read: std::result::Result<Option<T>, InvalidVariable>,
    invalid: &mut Vec<InvalidVariable>,
) -> Option<T> {
    read.unwrap_or_else(|var| {
        invalid.push(var);
        None
    })
}

/// The environment configuration reads, looked up through one function so a test can answer for
/// it without changing the process's own.
struct Env<'a>(&'a dyn Fn(&str) -> Option<OsString>);

impl Env<'static> {
    fn process() -> Self {
        Env(&|key| std::env::var_os(key))
    }
}

impl Env<'_> {
    fn get(&self, key: &str) -> Option<OsString> {
        (self.0)(key)
    }

    /// The variable `name` as `parse` reads it. An empty value is treated as unset, as
    /// `HATEL_CONFIG`'s is, so an exported-but-blank variable changes nothing.
    fn parsed<T>(
        &self,
        name: &'static str,
        parse: impl FnOnce(&OsStr) -> std::result::Result<T, String>,
    ) -> std::result::Result<Option<T>, InvalidVariable> {
        let Some(value) = self.get(name).filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        parse(&value).map(Some).map_err(|reason| InvalidVariable {
            name,
            value,
            reason,
        })
    }

    fn text<T>(
        &self,
        name: &'static str,
        expected: &str,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> std::result::Result<Option<T>, InvalidVariable> {
        self.parsed(name, |v| {
            v.to_str()
                .and_then(parse)
                .ok_or_else(|| format!("must be {expected}"))
        })
    }

    fn flag(&self, name: &'static str) -> std::result::Result<Option<bool>, InvalidVariable> {
        self.text(name, "1 or 0", |v| match v {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        })
    }

    fn sink(&self) -> std::result::Result<Option<SinkKind>, InvalidVariable> {
        self.text("HATEL_SINK", "jsonl or sqlite", SinkKind::parse)
    }

    /// A leading `~` is refused, as the configuration file's `state_dir` refuses it: a shell
    /// expands one before this process starts, so one that arrives here never will be.
    fn state_dir(&self) -> std::result::Result<Option<PathBuf>, InvalidVariable> {
        self.parsed("HATEL_STATE_DIR", |d| {
            let dir = PathBuf::from(d);
            crate::settings::refuse_tilde(&dir)?;
            Ok(crate::settings::absolute(dir))
        })
    }

    fn retention_days(&self) -> std::result::Result<Option<i64>, InvalidVariable> {
        self.text(
            "HATEL_RETENTION_DAYS",
            &format!("a whole number of days from 1 to {MAX_RETENTION_DAYS}"),
            |s| {
                s.parse()
                    .ok()
                    .filter(|n| (1..=MAX_RETENTION_DAYS).contains(n))
            },
        )
    }

    fn rotate_bytes(&self) -> std::result::Result<Option<u64>, InvalidVariable> {
        self.text(
            "HATEL_ROTATE_BYTES",
            "a whole number of bytes above 0",
            |s| s.parse().ok().filter(|n| *n > 0),
        )
    }

    fn storage_overrides(&self) -> Vec<&'static str> {
        [
            ("HATEL_SINK", self.sink().is_ok_and(|v| v.is_some())),
            (
                "HATEL_STATE_DIR",
                self.state_dir().is_ok_and(|v| v.is_some()),
            ),
            (
                "HATEL_RETENTION_DAYS",
                self.retention_days().is_ok_and(|v| v.is_some()),
            ),
            (
                "HATEL_ROTATE_BYTES",
                self.rotate_bytes().is_ok_and(|v| v.is_some()),
            ),
        ]
        .into_iter()
        .filter_map(|(var, set)| set.then_some(var))
        .collect()
    }
}

fn xdg_state_dir() -> PathBuf {
    use etcetera::BaseStrategy as _;
    match etcetera::choose_base_strategy() {
        Ok(s) => s.state_dir().unwrap_or_else(|| s.data_dir()).join("hatel"),
        Err(_) => PathBuf::from(".hatel"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn resolution(file: &str, vars: &[(&str, &str)]) -> (Config, Vec<InvalidVariable>) {
        let settings =
            Settings::parse(file, Path::new("/home/u/.config/hatel/config.toml")).unwrap();
        let lookup = |key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        };
        Config::resolve(&settings, &Env(&lookup))
    }

    fn resolved(file: &str, vars: &[(&str, &str)]) -> Config {
        let (cfg, invalid) = resolution(file, vars);
        assert_eq!(invalid, []);
        cfg
    }

    #[test]
    fn storage_comes_from_a_variable_then_the_file_then_the_default() {
        let file = "[storage]\nsink = \"sqlite\"\nstate_dir = \"data\"\nretention_days = 30\nrotate_bytes = 1024";
        let from_file = resolved(file, &[]);
        assert_eq!(from_file.sink, SinkKind::Sqlite);
        assert_eq!(from_file.state_dir, Path::new("/home/u/.config/hatel/data"));
        assert_eq!(
            from_file.ledger_dir,
            Path::new("/home/u/.config/hatel/data/ledger")
        );
        assert_eq!(from_file.retention_days, 30);
        assert_eq!(from_file.rotate_bytes, 1024);

        // Absolute on this platform: Windows resolves a rooted path without a drive against the
        // current drive.
        let elsewhere = if cfg!(windows) {
            r"C:\elsewhere"
        } else {
            "/elsewhere"
        };
        let vars = [
            ("HATEL_SINK", "jsonl"),
            ("HATEL_STATE_DIR", elsewhere),
            ("HATEL_RETENTION_DAYS", "7"),
            ("HATEL_ROTATE_BYTES", "2048"),
        ];
        let overridden = resolved(file, &vars);
        assert_eq!(overridden.sink, SinkKind::Jsonl);
        assert_eq!(overridden.state_dir, Path::new(elsewhere));
        assert_eq!(
            resolved(file, &[("HATEL_STATE_DIR", "relative")]).state_dir,
            std::env::current_dir().unwrap().join("relative"),
            "a relative variable names one directory whatever directory later work runs in"
        );
        assert_eq!(overridden.retention_days, 7);
        assert_eq!(overridden.rotate_bytes, 2048);

        let defaults = resolved("", &[]);
        assert_eq!(defaults.sink, SinkKind::Jsonl);
        assert_eq!(defaults.state_dir, xdg_state_dir());
        assert_eq!(defaults.retention_days, DEFAULT_RETENTION_DAYS);
        assert_eq!(defaults.rotate_bytes, DEFAULT_ROTATE_BYTES);
    }

    #[test]
    fn only_a_variable_that_takes_effect_is_named_as_an_override() {
        let vars: &[(&str, &str)] = &[
            ("HATEL_STATE_DIR", "/elsewhere"),
            ("HATEL_RETENTION_DAYS", "0"),
            ("HATEL_SINK", ""),
        ];
        let lookup = |key: &str| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        };
        assert_eq!(storage_overrides(&lookup), ["HATEL_STATE_DIR"]);
        let invalid = invalid_variables(&lookup);
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].name, "HATEL_RETENTION_DAYS");
    }

    #[test]
    fn a_variable_set_to_a_value_it_cannot_read_is_named_and_leaves_the_file_in_force() {
        let file = "[storage]\nsink = \"sqlite\"\nretention_days = 30\nrotate_bytes = 1024";
        for days in ["365d", "0", "100001"] {
            let (cfg, invalid) = resolution(
                file,
                &[
                    ("HATEL_SINK", "sqllite"),
                    ("HATEL_STATE_DIR", "~/data"),
                    ("HATEL_RETENTION_DAYS", days),
                    ("HATEL_ROTATE_BYTES", "10MB"),
                    ("HATEL_DISABLED", "true"),
                ],
            );
            assert_eq!(cfg.sink, SinkKind::Sqlite);
            assert_eq!(cfg.state_dir, xdg_state_dir());
            assert_eq!(cfg.retention_days, 30);
            assert_eq!(cfg.rotate_bytes, 1024);
            assert!(!cfg.disabled);
            let mut named: Vec<_> = invalid.iter().map(|v| v.name).collect();
            named.sort_unstable();
            assert_eq!(
                named,
                [
                    "HATEL_DISABLED",
                    "HATEL_RETENTION_DAYS",
                    "HATEL_ROTATE_BYTES",
                    "HATEL_SINK",
                    "HATEL_STATE_DIR"
                ]
            );
        }
        let (_, invalid) = resolution("", &[("HATEL_SINK", "sqllite")]);
        assert_eq!(
            invalid[0].to_string(),
            r#"HATEL_SINK="sqllite": must be jsonl or sqlite"#
        );
    }

    #[test]
    fn a_flag_is_set_by_1_and_cleared_by_0_or_an_empty_value() {
        for (value, set) in [("1", true), ("0", false), ("", false)] {
            assert_eq!(resolved("", &[("HATEL_DISABLED", value)]).disabled, set);
        }
    }
}
