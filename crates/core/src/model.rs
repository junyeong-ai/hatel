//! The telemetry envelope — one record on the wire / one JSONL line.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::pii;
use crate::registry::Registry;

/// Wire schema version, carried on every envelope. Bump only on a breaking change.
pub const SCHEMA_VERSION: u32 = 1;

/// A record payload: field → JSON value. A `BTreeMap` keeps keys sorted so the
/// serialized line is deterministic.
pub type Payload = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub ts: String,
    pub kind: String,
    #[serde(rename = "_schema_version", default = "schema_v1")]
    pub schema_version: u32,
    pub payload: Payload,
}

/// A line written by an older or foreign producer defaults to v1 rather than
/// silently claiming the current version.
fn schema_v1() -> u32 {
    1
}

impl Envelope {
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse one JSONL line. A record stamped with a schema version newer than this
    /// build understands is skipped rather than mis-interpreted — the version stamp is
    /// load-bearing on read, so a future format bump degrades safely instead of
    /// silently aggregating an incompatible shape.
    pub fn from_json_line(line: &str) -> Option<Self> {
        let env: Self = serde_json::from_str(line).ok()?;
        (env.schema_version <= SCHEMA_VERSION).then_some(env)
    }
}

/// Build a sanitized envelope for `kind`: the payload is filtered to the kind's
/// allow-list and its redacted fields are hashed before the record is stamped.
pub fn make_envelope(
    kind: &str,
    payload: Payload,
    reg: &Registry,
    strict: bool,
) -> crate::Result<Envelope> {
    let spec = reg
        .kind(kind)
        .ok_or_else(|| crate::Error::UnknownKind(kind.to_string()))?;
    let payload = pii::sanitize(spec, payload, strict)?;
    Ok(Envelope {
        ts: now_iso_utc(),
        kind: kind.to_string(),
        schema_version: SCHEMA_VERSION,
        payload,
    })
}

/// ISO-8601 / RFC-3339 UTC timestamp with a `Z` suffix.
pub fn now_iso_utc() -> String {
    jiff::Timestamp::now().to_string()
}

/// Current UTC time in epoch seconds — the single source for "now" wherever a window
/// cutoff is computed (reports, cost retention).
pub fn now_epoch() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// Epoch seconds for an RFC-3339 timestamp string, or `None` if unparseable. The single
/// parser used everywhere a stored `ts` is windowed (reports, cost snapshot pruning) — an
/// unparseable timestamp consistently yields `None`, so the caller drops the record rather
/// than bucketing it at epoch 0.
pub fn ts_epoch(ts: &str) -> Option<i64> {
    ts.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}
