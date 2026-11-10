use std::time::Duration;

/// Maximum number of extra attempts after the first.
pub const MAX_RETRIES: u32 = 3;
/// Above this the client does not wait: it returns the error with the time to retry.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// Exponential backoff: `base`, `2*base`, `4*base`.
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
    /// Wait before attempt `retry` (0 = the first repetition).
    pub fn delay(&self, retry: u32) -> Duration {
        self.base * 2u32.pow(retry.min(3))
    }
}
