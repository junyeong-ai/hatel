//! OTLP egress: forward the received stream to downstream collectors. `transform` enriches a
//! body with the project label; `exporter` owns the bounded queue and drain task; `durable`
//! delivers local records; `client` is the HTTP client both delivery paths post through.
//! This is the receiver's exporter half of a receiver → processor → exporter pipeline.

pub mod client;
pub mod durable;
pub mod exporter;
pub mod transform;

pub use exporter::{Exporter, OtlpSignal};
