//! Domain-agnostic core of the Claude Code telemetry collector.
//!
//! Everything here is runtime-agnostic (no async, no HTTP) so the hook binary can
//! link it without pulling in the receiver's async stack. The receiver (in the
//! `cli` crate) reuses the same model, registry, sinks, and session index.

pub mod config;
pub mod cost;
pub mod export;
pub mod hook;
pub mod model;
pub mod pii;
pub mod project;
pub mod registry;
pub mod render;
pub mod report;
pub mod rolling;
pub mod schema;
pub mod session;
pub mod sink;

pub use config::Config;
pub use export::{ExportConfig, ExportMode, ExportTarget, ProjectFilter};
pub use model::{
    Envelope, Payload, SCHEMA_VERSION, make_envelope, now_epoch, now_iso_utc, ts_epoch,
};
pub use project::{ProjectRef, resolve_project};
pub use registry::{FieldMap, HookBinding, KindSpec, Registry};
pub use session::{SessionIndex, SessionIndexCache, SessionRow};
pub use sink::{Sink, SinkKind, build_sink};

/// One error type for the whole core crate. The hook path is fail-open and never
/// surfaces these to the caller; the registry/schema paths fail loud at startup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
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
    #[error("export config parse error in {path}: {source}")]
    ExportParse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid export config: {0}")]
    InvalidExport(String),
    #[error("io error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;
