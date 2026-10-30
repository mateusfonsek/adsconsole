use std::time::Duration;

/// Máximo de tentativas a mais depois da primeira.
pub const MAX_RETRIES: u32 = 3;

/// Backoff exponencial: `base`, `2*base`, `4*base`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    /// Espera antes da `retry`-ésima repetição (começa em 0).
    pub fn delay(&self, retry: u32) -> Duration {
        self.base * 2u32.pow(retry.min(MAX_RETRIES))
    }
}
