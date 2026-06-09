//! The session index — the generic `session_id → project` join, sink-independent
//! and append-only. The receiver needs it to attribute project-less OTel datapoints
//! to a project, regardless of the configured sink. One line per session start; the
//! reader folds last-wins, so concurrent hooks never race on a read-modify-write.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::project::ProjectRef;

#[derive(Debug, Serialize, Deserialize)]
struct IndexLine {
    session_id: String,
    project_key: String,
    project_label: String,
}

/// A session's project attribution (keyed by session id in the loaded map).
#[derive(Debug, Clone, Default)]
pub struct SessionRow {
    pub project_key: String,
    pub project_label: String,
}

pub struct SessionIndex {
    path: PathBuf,
}

impl SessionIndex {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            path: state_dir.join("session_index.jsonl"),
        }
    }

    /// Append one session → project line. Called once per session (at start), so
    /// there is no read-modify-write and no race between concurrent hook processes.
    pub fn record(&self, session_id: &str, project: &ProjectRef) {
        let line = IndexLine {
            session_id: session_id.to_string(),
            project_key: project.key.clone(),
            project_label: project.label.clone(),
        };
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            let mut s = serde_json::to_string(&line).unwrap_or_default();
            s.push('\n');
            file.write_all(s.as_bytes())
        })();
        if let Err(e) = result {
            eprintln!("hatel: session index append failed: {e}");
        }
    }

    /// Fold the append-only log into one row per session (last writer wins).
    pub fn load(&self) -> BTreeMap<String, SessionRow> {
        let mut map: BTreeMap<String, SessionRow> = BTreeMap::new();
        let Ok(text) = fs::read_to_string(&self.path) else {
            return map;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(il) = serde_json::from_str::<IndexLine>(line) else {
                continue;
            };
            map.insert(
                il.session_id,
                SessionRow {
                    project_key: il.project_key,
                    project_label: il.project_label,
                },
            );
        }
        map
    }
}
