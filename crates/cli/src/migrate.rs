use std::path::PathBuf;

use hatel_core::{Config, Settings, SinkKind};
use serde::Serialize;

#[derive(Serialize)]
struct Report {
    check: bool,
    stage: &'static str,
    config: Option<PathBuf>,
    source_sink: Option<SinkKind>,
    source: Option<PathBuf>,
    database: Option<PathBuf>,
    preflight: Option<hatel_core::migration::Preflight>,
    imported_records: Option<usize>,
    normalized_records: Option<usize>,
    operation_committed: bool,
    config_updated: bool,
    error: Option<String>,
}

struct Configuration {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    settings: Settings,
    cfg: Config,
}

fn io(error: impl std::fmt::Display) -> hatel_core::Error {
    hatel_core::Error::Io(error.to_string())
}

fn contents(path: &std::path::Path) -> hatel_core::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(format!("read config {}: {error}", path.display()))),
    }
}

impl Configuration {
    fn load() -> hatel_core::Result<Self> {
        for variable in ["HATEL_SINK", "HATEL_STATE_DIR", "HATEL_TESTING"] {
            if std::env::var_os(variable).is_some_and(|v| !v.is_empty()) {
                return Err(io(format!(
                    "unset {variable} before migrating; configure storage in config.toml"
                )));
            }
        }
        let path = Settings::path().ok_or_else(|| io("no config directory"))?;
        let before = contents(&path)?;
        let settings = Settings::read(&path)?;
        let cfg = Config::try_from_settings(&settings)?;
        let result = Self {
            path,
            contents: before,
            settings,
            cfg,
        };
        result.ensure_unchanged()?;
        Ok(result)
    }

    // The operator must also keep configuration editors stopped. This detects changes
    // during long imports; an atomic rename does not provide a filesystem compare-and-swap.
    fn ensure_unchanged(&self) -> hatel_core::Result<()> {
        if contents(&self.path)? != self.contents {
            return Err(io(
                "configuration changed during migration; configuration was not replaced; restore the original storage selection and retry with unchanged sources",
            ));
        }
        Ok(())
    }

    fn select_sqlite(&self) -> hatel_core::Result<()> {
        self.ensure_unchanged()?;
        let mut settings = self.settings.clone();
        settings.storage.sink = Some(SinkKind::Sqlite);
        settings.save()?;
        Ok(())
    }
}

pub fn run(check: bool, json: bool) -> i32 {
    let mut report = Report {
        check,
        stage: "configuration",
        config: Settings::path(),
        source_sink: None,
        source: None,
        database: None,
        preflight: None,
        imported_records: None,
        normalized_records: None,
        operation_committed: false,
        config_updated: false,
        error: None,
    };
    let result = (|| -> hatel_core::Result<()> {
        let config = Configuration::load()?;
        let cfg = &config.cfg;
        let database = cfg.state_dir.join("telemetry.db");
        report.source_sink = Some(cfg.sink);
        report.database = Some(database.clone());
        report.source = Some(if cfg.sink == SinkKind::Jsonl {
            cfg.ledger_dir.clone()
        } else {
            database.clone()
        });
        if check {
            report.stage = "preflight";
            report.preflight = Some(hatel_core::migration::check(cfg)?);
            config.ensure_unchanged()?;
        } else {
            report.stage = "lock";
            let _lock = match crate::serve::acquire_state_lock(&cfg.state_dir) {
                crate::serve::LockOutcome::Acquired(lock) => lock,
                crate::serve::LockOutcome::Held => {
                    return Err(io("stop the receiver before migrating"));
                }
                crate::serve::LockOutcome::Failed(error) => return Err(io(error)),
            };
            config.ensure_unchanged()?;
            report.stage = "storage";
            match cfg.sink {
                SinkKind::Jsonl => {
                    report.imported_records = Some(hatel_core::migration::import_jsonl(
                        &cfg.ledger_dir,
                        &database,
                    )?)
                }
                SinkKind::Sqlite => {
                    report.normalized_records =
                        Some(hatel_core::migration::upgrade_sqlite(&database)?)
                }
            }
            report.operation_committed = true;
            report.stage = "configuration_selection";
            config.ensure_unchanged()?;
            if cfg.sink == SinkKind::Jsonl {
                config.select_sqlite()?;
                report.config_updated = true;
            }
        }
        report.stage = "complete";
        Ok(())
    })();
    if let Err(error) = result {
        report.error = Some(error.to_string());
    }
    if json {
        let value = serde_json::to_value(&report).expect("migration report serializes");
        println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON serializes")
        );
    } else if let Some(error) = &report.error {
        eprintln!(
            "migrate: {error}; stage={}; operation_committed={}",
            report.stage, report.operation_committed
        );
    } else if let Some(preflight) = &report.preflight {
        println!(
            "checked {} records in {} contracts; source={}; database={}; source files and configuration unchanged",
            preflight.records,
            preflight.contracts.len(),
            preflight.source.display(),
            preflight.database.display()
        );
    } else if let Some(count) = report.imported_records {
        println!("imported {count} records; storage=sqlite; source files preserved");
    } else if let Some(count) = report.normalized_records {
        println!("normalized {count} records; storage=sqlite");
    }
    i32::from(report.error.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_configuration_is_rejected_without_overwriting_it() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let original = b"[storage]\nsink='jsonl'\nstate_dir='one'\n";
        std::fs::write(&path, original).unwrap();
        let settings = Settings::read(&path).unwrap();
        let config = Configuration {
            path: path.clone(),
            contents: Some(original.to_vec()),
            cfg: Config::try_from_settings(&settings).unwrap(),
            settings,
        };
        let changed = b"[storage]\nsink='jsonl'\nstate_dir='two'\n";
        std::fs::write(&path, changed).unwrap();
        assert!(
            config
                .select_sqlite()
                .unwrap_err()
                .to_string()
                .contains("configuration changed")
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        assert!(!home.path().join("two/telemetry.db").exists());
    }
}
