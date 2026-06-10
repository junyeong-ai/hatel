//! The open, parity-checked registry — the single extension surface.
//!
//! A `KindSpec` is the one declaration of a Kind: its allow-list, group key,
//! redaction set, and report weight all live in one place, so the four facets
//! cannot drift. Core and plugins contribute specs and hook bindings through the
//! exact same path (see `schema.rs`); the only difference is which TOML file they
//! come from.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::{Error, Result};

/// A validated Kind declaration.
#[derive(Debug, Clone)]
pub struct KindSpec {
    pub name: String,
    /// Allow-list: the only keys that survive sanitization. Single source of truth.
    pub fields: BTreeSet<String>,
    /// Field that identifies a record when aggregating in reports.
    pub group_key: String,
    /// Fields hashed before write (e.g. a raw user identifier).
    pub redact: BTreeSet<String>,
    /// Numeric fields a report sums per group (durations, counts, costs). Ordered:
    /// the first is the primary metric a report ranks groups by; all are displayed.
    pub measures: Vec<String>,
    /// Whether this Kind is written by the receiver from a native OTel signal (e.g. `tool` from
    /// `tool_result`) rather than by the hook. Such a Kind must not have a hook binding — that would
    /// give it two writers and double-count it — so `bind` rejects one.
    pub receiver_sourced: bool,
}

/// The raw, deserialized form of a `[[kind]]` table before validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindSpecRaw {
    pub name: String,
    pub fields: Vec<String>,
    pub group_key: String,
    #[serde(default)]
    pub redact: Vec<String>,
    #[serde(default)]
    pub measures: Vec<String>,
    #[serde(default)]
    pub receiver_sourced: bool,
}

/// A Kind name is also a JSONL filename component, so it is restricted to a safe
/// character set — no path separators, no whitespace — which makes traversal impossible.
fn is_valid_kind_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl KindSpec {
    pub fn from_raw(raw: KindSpecRaw) -> Result<KindSpec> {
        let fields: BTreeSet<String> = raw.fields.into_iter().collect();
        let invalid = |reason: String| Error::InvalidSpec {
            name: raw.name.clone(),
            reason,
        };
        if !is_valid_kind_name(&raw.name) {
            return Err(invalid(
                "name must be non-empty and use only [A-Za-z0-9._-]".to_string(),
            ));
        }
        if !fields.contains(&raw.group_key) {
            return Err(invalid(format!(
                "group_key '{}' not in fields",
                raw.group_key
            )));
        }
        let redact: BTreeSet<String> = raw.redact.into_iter().collect();
        if let Some(r) = redact.iter().find(|r| !fields.contains(*r)) {
            return Err(invalid(format!("redact field '{r}' not in fields")));
        }
        if let Some(m) = raw.measures.iter().find(|m| !fields.contains(*m)) {
            return Err(invalid(format!("measure '{m}' not in fields")));
        }
        Ok(KindSpec {
            name: raw.name,
            fields,
            group_key: raw.group_key,
            redact,
            measures: raw.measures,
            receiver_sourced: raw.receiver_sourced,
        })
    }
}

/// The source field(s) a `FieldMap` reads. A single key, or several tried in
/// order (the first present wins) — which lets a binding tolerate a field whose
/// exact name is version-sensitive without ever guessing a value.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FromSpec {
    One(String),
    Many(Vec<String>),
}

impl FromSpec {
    fn keys(&self) -> &[String] {
        match self {
            FromSpec::One(s) => std::slice::from_ref(s),
            FromSpec::Many(v) => v,
        }
    }
}

/// A declarative transform from a hook stdin field into one payload field.
/// Exactly one outcome applies; an absent source or a non-matching transform
/// yields `None`, and the field is simply omitted — never fabricated.
#[derive(Debug, Clone, Deserialize)]
pub struct FieldMap {
    #[serde(default)]
    pub from: Option<FromSpec>,
    /// Regex; the first capture group becomes the value.
    #[serde(default)]
    pub capture: Option<String>,
    /// Take the character length of the source string.
    #[serde(default)]
    pub len: bool,
    /// Emit a bool: whether the source field is present and non-null.
    #[serde(default)]
    pub present: bool,
    /// Take the final path component of the source string.
    #[serde(default)]
    pub basename: bool,
    /// A constant value, independent of stdin.
    #[serde(default, rename = "const")]
    pub constant: Option<serde_json::Value>,
}

impl FieldMap {
    /// Reject an ambiguous map at startup rather than silently applying a priority
    /// order: at most one transform may be set, and a non-`const` map needs a `from`
    /// source (a transform with no source would always omit — a dead mapping).
    fn validate(&self) -> std::result::Result<(), &'static str> {
        let transforms = [
            self.capture.is_some(),
            self.len,
            self.present,
            self.basename,
        ]
        .iter()
        .filter(|x| **x)
        .count()
            + usize::from(self.constant.is_some());
        if transforms > 1 {
            return Err("at most one of capture/len/present/basename/const may be set");
        }
        if self.constant.is_none() && self.from.is_none() {
            return Err("a non-const map needs a `from` source");
        }
        Ok(())
    }

    /// Whether this mapping reads `key` as a source — used to decide, per event,
    /// whether a synthetic field like `git_branch` is worth computing at all.
    pub fn references(&self, key: &str) -> bool {
        match &self.from {
            Some(FromSpec::One(s)) => s == key,
            Some(FromSpec::Many(v)) => v.iter().any(|s| s == key),
            None => false,
        }
    }

    pub fn apply(&self, stdin: &serde_json::Value) -> Option<serde_json::Value> {
        if let Some(c) = &self.constant {
            return Some(c.clone());
        }
        let keys = self.from.as_ref()?.keys();
        if self.present {
            let has = keys
                .iter()
                .any(|k| stdin.get(k).map(|v| !v.is_null()).unwrap_or(false));
            return Some(serde_json::Value::Bool(has));
        }
        let value = keys.iter().find_map(|k| stdin.get(k))?;
        if self.len {
            return Some(serde_json::Value::from(value.as_str()?.chars().count()));
        }
        if self.basename {
            let s = value.as_str()?;
            let base = std::path::Path::new(s)
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or(s);
            return Some(serde_json::Value::from(base));
        }
        if let Some(pat) = &self.capture {
            let re = regex::Regex::new(pat).ok()?;
            let caps = re.captures(value.as_str()?)?;
            return Some(serde_json::Value::from(caps.get(1)?.as_str()));
        }
        Some(value.clone())
    }
}

/// A hook event → Kind mapping with its field transforms.
#[derive(Debug, Clone, Deserialize)]
pub struct HookBinding {
    pub event: String,
    pub kind: String,
    #[serde(default)]
    pub map: BTreeMap<String, FieldMap>,
}

/// The merged set of Kinds, hook bindings, and tracked native signals. Built once
/// at startup from core + plugin TOML; collisions are a hard error there.
#[derive(Debug, Default, Clone)]
pub struct Registry {
    kinds: BTreeMap<String, KindSpec>,
    bindings: BTreeMap<String, Vec<HookBinding>>,
    pub tracked_metrics: BTreeSet<String>,
    pub counted_events: BTreeSet<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_kind(&mut self, spec: KindSpec) -> Result<()> {
        if self.kinds.contains_key(&spec.name) {
            return Err(Error::DuplicateKind(spec.name));
        }
        self.kinds.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn bind(&mut self, binding: HookBinding) -> Result<()> {
        let invalid = |reason: String| Error::InvalidSpec {
            name: binding.kind.clone(),
            reason,
        };
        let Some(spec) = self.kinds.get(&binding.kind) else {
            return Err(invalid(format!(
                "binding for event '{}' targets unregistered kind",
                binding.event
            )));
        };
        // A receiver-sourced Kind (e.g. `tool`, written from the native `tool_result` event) must
        // have exactly one writer; a hook binding would double-count it, so reject it loudly.
        if spec.receiver_sourced {
            return Err(invalid(format!(
                "binding for event '{}' targets receiver-sourced kind '{}' — it is written from \
                 native OTel and cannot be hook-bound",
                binding.event, binding.kind
            )));
        }
        // Every output field a binding writes must be allow-listed by the target Kind,
        // and every capture regex must compile — both checked here, loudly, at startup,
        // rather than silently dropping a mistyped field or a bad pattern at event time.
        for (out_field, map) in &binding.map {
            if out_field == "project" {
                return Err(invalid(format!(
                    "binding for event '{}' may not map 'project' — it is injected from the cwd",
                    binding.event
                )));
            }
            if !spec.fields.contains(out_field) {
                return Err(invalid(format!(
                    "binding for event '{}' writes field '{out_field}' not in the kind's fields",
                    binding.event
                )));
            }
            if let Err(reason) = map.validate() {
                return Err(invalid(format!(
                    "binding for event '{}' field '{out_field}': {reason}",
                    binding.event
                )));
            }
            if let Some(pattern) = &map.capture {
                match regex::Regex::new(pattern) {
                    Err(e) => {
                        return Err(invalid(format!(
                            "binding for event '{}' field '{out_field}' has an invalid capture regex: {e}",
                            binding.event
                        )));
                    }
                    // `apply` takes capture group 1, so a pattern with no group would
                    // silently never produce a value — reject it loudly here instead.
                    Ok(re) if re.captures_len() < 2 => {
                        return Err(invalid(format!(
                            "binding for event '{}' field '{out_field}' capture regex needs a group, e.g. ^spec/(.+)$",
                            binding.event
                        )));
                    }
                    Ok(_) => {}
                }
            }
        }
        let siblings = self.bindings.entry(binding.event.clone()).or_default();
        // One (event, kind) pair may be bound only once — a second binding for the same
        // pair would write two records per hook fire. Distinct Kinds for one event are
        // fine (e.g. a session-end binding plus a domain binding).
        if siblings.iter().any(|b| b.kind == binding.kind) {
            return Err(invalid(format!(
                "event '{}' already has a binding for kind '{}'",
                binding.event, binding.kind
            )));
        }
        siblings.push(binding);
        Ok(())
    }

    pub fn kind(&self, name: &str) -> Option<&KindSpec> {
        self.kinds.get(name)
    }

    pub fn kinds(&self) -> impl Iterator<Item = &KindSpec> {
        self.kinds.values()
    }

    pub fn bindings_for(&self, event: &str) -> &[HookBinding] {
        self.bindings.get(event).map(Vec::as_slice).unwrap_or(&[])
    }
}
