//! Pluggable storage. Both halves go through this module: emitters write via the
//! `Sink` trait, and reports read via `read_records` — so swapping the backend
//! (JSONL / SQLite / an external store) touches `Config` alone, and a report consumes
//! whichever backend is configured. Every sink is fail-open: a write error degrades to
//! a stderr notice and never propagates, so telemetry cannot block a tool call.

mod jsonl;
mod sqlite;

pub use jsonl::JsonlSink;
pub use sqlite::SqliteSink;

use std::path::PathBuf;

use crate::{Config, Envelope};

pub trait Sink {
    fn write_record(&mut self, env: &Envelope);
    fn flush(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKind {
    Jsonl,
    Sqlite,
}

impl SinkKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "jsonl" => Some(Self::Jsonl),
            "sqlite" => Some(Self::Sqlite),
            _ => None,
        }
    }
}

fn sqlite_db_path(cfg: &Config) -> PathBuf {
    cfg.state_dir.join("telemetry.db")
}

pub fn build_sink(cfg: &Config) -> Box<dyn Sink> {
    match cfg.sink {
        SinkKind::Jsonl => Box::new(JsonlSink::new(cfg.ledger_dir.clone(), cfg.rotate_bytes)),
        SinkKind::Sqlite => Box::new(SqliteSink::open(sqlite_db_path(cfg))),
    }
}

/// Read records for `kind` from the configured backend — the read half of the
/// storage abstraction. `since` is an epoch-second lower bound the backend may use to
/// avoid scanning out-of-window history (SQLite filters in SQL; JSONL leaves the exact
/// windowing to the caller).
pub fn read_records(cfg: &Config, kind: &str, since: Option<i64>) -> Vec<Envelope> {
    match cfg.sink {
        SinkKind::Jsonl => jsonl::read_records(&cfg.ledger_dir, kind),
        SinkKind::Sqlite => sqlite::read_records(&sqlite_db_path(cfg), kind, since),
    }
}
