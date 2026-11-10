/// Receives the count of operations sent to Google (quota is per Cloud project).
pub trait OpsMeter: Send + Sync {
    fn record(&self, ops: u32);
}

/// Meter that discards the count.
pub struct NoopMeter;

impl OpsMeter for NoopMeter {
    fn record(&self, _ops: u32) {}
}
