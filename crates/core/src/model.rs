//! The telemetry envelope — one stored record.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::pii;
use crate::registry::Registry;

/// Wire schema version, carried on every envelope. Bump only on a breaking change.
pub const SCHEMA_VERSION: u32 = 1;

/// A record payload: field → JSON value. A `BTreeMap` keeps keys sorted so the
/// serialized line is deterministic.
pub type Payload = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    pub ts: String,
    pub kind: String,
    pub schema_version: u32,
    pub payload: Payload,
    pub metadata: RecordMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordMetadata {
    #[serde(default, skip_serializing_if = "is_false")]
    pub projection: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<crate::ProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordContext {
    pub occurred_at: Option<String>,
    pub source: Option<String>,
    pub run_id: Option<String>,
    pub correlation_id: Option<String>,
}

impl Envelope {
    pub fn apply_context(&mut self, context: RecordContext) -> crate::Result<()> {
        if let Some(ts) = context.occurred_at {
            let parsed = ts
                .parse::<jiff::Timestamp>()
                .map_err(|_| crate::Error::InvalidPayload("occurred_at must be RFC 3339".into()))?;
            self.ts = parsed.to_string();
        }
        self.metadata.source = context.source;
        self.metadata.run_id = context.run_id;
        self.metadata.correlation_id = context.correlation_id;
        Ok(())
    }
}

impl Envelope {
    /// Re-observing one event may change its observation time, never its meaning or attribution.
    pub(crate) fn same_record(&self, other: &Self) -> bool {
        let mut ours = self.metadata.clone();
        let mut theirs = other.metadata.clone();
        ours.observed_at = None;
        theirs.observed_at = None;
        self.kind == other.kind
            && self.ts == other.ts
            && self.schema_version == other.schema_version
            && self.payload == other.payload
            && ours == theirs
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
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
    if serde_json::to_vec(&payload)
        .map_err(|e| crate::Error::InvalidPayload(e.to_string()))?
        .len()
        > 256 * 1024
    {
        return Err(crate::Error::InvalidPayload(
            "payload exceeds 256 KiB".into(),
        ));
    }
    Ok(Envelope {
        ts: now_iso_utc(),
        kind: kind.to_string(),
        schema_version: SCHEMA_VERSION,
        payload,
        metadata: RecordMetadata {
            record_id: Some(uuid::Uuid::new_v4().to_string()),
            observed_at: Some(now_iso_utc()),
            kind_version: Some(spec.version),
            schema_id: Some(spec.schema_id.clone()),
            ..RecordMetadata::default()
        },
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
    ts.parse::<jiff::Timestamp>()
        .ok()
        .map(|t| timestamp_parts(t).0)
}

/// Floor seconds keep fractional instants before the epoch on the correct side of a cutoff.
pub(crate) fn timestamp_parts(ts: jiff::Timestamp) -> (i64, i32) {
    let nanos = ts.subsec_nanosecond();
    (
        ts.as_second() - i64::from(nanos < 0),
        nanos.rem_euclid(1_000_000_000),
    )
}

pub fn source_record_id(source: &str, parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{source}:{}", hash.finalize().to_hex())
}
