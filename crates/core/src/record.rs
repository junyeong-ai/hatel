//! Validated local recording shared by command-line and protocol clients.

use crate::model::RecordContext;
use crate::{Config, Payload, Registry, Sink, make_envelope};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordInput {
    pub kind: String,
    pub payload: Payload,
    #[serde(default)]
    pub context: RecordContext,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub record_id: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecordResult {
    Stored { receipt: Receipt },
    Rejected { error: String },
    Failed { error: String },
}

pub struct Writer<'a> {
    config: &'a Config,
    registry: &'a Registry,
    sink: Box<dyn Sink>,
    project: Option<crate::ProjectRef>,
}

impl<'a> Writer<'a> {
    pub fn new(config: &'a Config, registry: &'a Registry) -> Self {
        let project = std::env::current_dir()
            .ok()
            .and_then(|p| crate::resolve_project(&p.to_string_lossy()));
        Self {
            config,
            registry,
            sink: crate::build_sink(config),
            project,
        }
    }

    pub fn write(&mut self, input: RecordInput) -> RecordResult {
        if self.config.disabled {
            return RecordResult::Rejected {
                error: "recording is disabled".into(),
            };
        }
        let Some(spec) = self.registry.kind(&input.kind) else {
            return RecordResult::Rejected {
                error: format!("unknown kind: {}", input.kind),
            };
        };
        let dropped: Vec<_> = input
            .payload
            .keys()
            .filter(|k| !spec.fields.contains(*k))
            .cloned()
            .collect();
        let warning = (!dropped.is_empty())
            .then(|| format!("{} does not accept {dropped:?} (dropped)", input.kind));
        let envelope = make_envelope(
            &input.kind,
            input.payload,
            self.registry,
            self.config.strict,
        )
        .and_then(|mut env| {
            env.apply_context(input.context)?;
            env.metadata.project = self.project.clone();
            Ok(env)
        });
        let env = match envelope {
            Ok(env) => env,
            Err(e) => {
                return RecordResult::Rejected {
                    error: e.to_string(),
                };
            }
        };
        match self
            .sink
            .write_record(&env)
            .and_then(|receipt| self.sink.flush().map(|()| receipt))
        {
            Ok(receipt) => RecordResult::Stored {
                receipt: Receipt {
                    record_id: receipt.record_id,
                    warning,
                },
            },
            Err(e) => RecordResult::Failed {
                error: e.to_string(),
            },
        }
    }
}
