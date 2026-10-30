use std::time::Duration;

/// Tentativas a mais depois da primeira.
pub const MAX_RETRIES: u32 = 3;
/// Acima disso o cliente não espera: devolve o erro com o tempo para tentar de novo.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// Backoff exponencial: `base`, `2 × base`, `4 × base`.
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
    /// Espera antes da tentativa `retry` (0 = primeira repetição).
    pub fn delay(&self, retry: u32) -> Duration {
        self.base * 2u32.pow(retry.min(3))
    }
}
