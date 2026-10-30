/// Recebe a contagem de operações enviadas ao Google (quota por projeto do Cloud).
pub trait OpsMeter: Send + Sync {
    fn record(&self, ops: u32);
}

/// Medidor que descarta a contagem.
pub struct NoopMeter;

impl OpsMeter for NoopMeter {
    fn record(&self, _ops: u32) {}
}
