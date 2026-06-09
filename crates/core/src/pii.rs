//! Privacy enforcement. The allow-list is the primary defense: every key outside
//! the Kind's `fields` is dropped before write. Redacted fields are hashed. The
//! core ships no content-bearing fields (prompt/tool bodies), so there is nothing
//! to opt out of.

use crate::registry::KindSpec;
use crate::{Error, Payload, Result};

/// Keep only allow-listed keys, hashing any declared as redacted. In strict mode
/// an unknown key is an error (the test suite proves no leakage); otherwise it is
/// silently dropped.
pub fn sanitize(spec: &KindSpec, payload: Payload, strict: bool) -> Result<Payload> {
    let mut out = Payload::new();
    let mut rejected = Vec::new();
    for (key, value) in payload {
        if spec.fields.contains(&key) {
            let value = if spec.redact.contains(&key) {
                hash_value(&value)
            } else {
                value
            };
            out.insert(key, value);
        } else {
            rejected.push(key);
        }
    }
    if strict && !rejected.is_empty() {
        rejected.sort();
        return Err(Error::DisallowedKeys {
            kind: spec.name.clone(),
            keys: rejected,
        });
    }
    Ok(out)
}

/// A short, stable, non-cryptographic correlation hash for an identifier.
fn hash_id(raw: &str) -> String {
    let hex = blake3::hash(raw.as_bytes()).to_hex();
    hex[..16].to_string()
}

fn hash_value(v: &serde_json::Value) -> serde_json::Value {
    let raw = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    serde_json::Value::from(hash_id(&raw))
}
