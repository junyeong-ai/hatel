//! Domain-agnostic core of the Claude Code telemetry collector.
//!
//! Everything here is runtime-agnostic (no async, no HTTP) so the hook binary can
//! link it without pulling in the receiver's async stack. The receiver (in the
//! `cli` crate) reuses the same model, registry, ledger, and session index.

pub mod catalog;
pub mod claude;
pub mod config;
pub mod cost;
pub mod export;
pub mod model;
pub mod outbox;
pub mod pii;
pub mod project;
pub mod query;
pub mod record;
pub mod registry;
pub mod render;
pub mod report;
pub mod rolling;
pub mod schema;
pub mod session;
pub mod settings;
pub mod sink;

pub use config::Config;
pub use export::{
    AccountFilter, ExportConfig, ExportMode, ExportTarget, OtlpSignal, ProjectFilter,
};
pub use model::{
    Envelope, Payload, SCHEMA_VERSION, make_envelope, now_epoch, now_iso_utc, ts_epoch,
};
pub use project::{ProjectRef, project_selector, resolve_project};
pub use registry::{FieldMap, HookBinding, KindSpec, Registry};
pub use session::{SessionIndex, SessionIndexCache, SessionRow};
pub use settings::Settings;
pub use sink::{SqliteSink, build_sink};

/// One error type for the whole core crate. The hook path is fail-open and never
/// surfaces these to the caller; the registry/schema paths fail loud at startup.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("unknown kind: {0}")]
    UnknownKind(String),
    #[error("duplicate kind: {0}")]
    DuplicateKind(String),
    #[error("invalid kind spec '{name}': {reason}")]
    InvalidSpec { name: String, reason: String },
    #[error("disallowed payload keys for kind '{kind}': {keys:?}")]
    DisallowedKeys { kind: String, keys: Vec<String> },
    #[error("schema parse error in {path}: {source}")]
    SchemaParse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("config parse error in {path}: {source}")]
    ConfigParse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid export config: {0}")]
    InvalidExport(String),
    #[error("invalid environment: {0}")]
    InvalidEnvironment(String),
    #[error("io error: {0}")]
    Io(String),
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        let message = match &error {
            rusqlite::Error::SqliteFailure(code, Some(message)) => format!("{message} ({code})"),
            _ => error.to_string(),
        };
        Self::Io(message)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    #[test]
    fn sqlite_errors_keep_the_message_and_extended_code() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sample(value TEXT UNIQUE); INSERT INTO sample VALUES ('a')",
        )
        .unwrap();
        let error = conn
            .execute("INSERT INTO sample VALUES ('a')", [])
            .unwrap_err();
        let message = super::Error::from(error).to_string();
        assert!(
            message.contains("UNIQUE constraint failed: sample.value"),
            "{message}"
        );
        assert!(message.contains("2067"), "{message}");
        assert_eq!(
            super::Error::from(rusqlite::Error::QueryReturnedNoRows).to_string(),
            "io error: Query returned no rows"
        );
    }
}
