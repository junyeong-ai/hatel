//! OTLP egress: forward the received stream to downstream collectors. `transform` enriches a
//! body with the project label; `exporter` owns the bounded queue, drain task, and HTTP client.
//! This is the receiver's exporter half of a receiver → processor → exporter pipeline.

pub mod exporter;
pub mod transform;

pub use exporter::{Exporter, OtlpSignal};
