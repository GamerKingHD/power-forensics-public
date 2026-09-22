//! Collector interface. A failure in one collector must never break the rest.

use pf_core::telemetry::{Clock, FieldMeta};

/// Why a collector sample failed. Recorded in the session, never fatal.
#[derive(Debug, Clone)]
pub struct CollectorError {
    pub collector: &'static str,
    pub message: String,
}

impl CollectorError {
    pub fn new(collector: &'static str, message: impl Into<String>) -> Self {
        CollectorError {
            collector,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CollectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.collector, self.message)
    }
}

/// A modular telemetry source. Implementations must be cheap to poll at
/// their declared interval and must return `Err` (not panic) when a sensor
/// is missing or access is denied.
pub trait Collector {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Vec<FieldMeta>;
    /// One JSON object payload (without trailing newline). Errors are
    /// isolated per collector by the caller.
    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError>;
}
