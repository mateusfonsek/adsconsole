//! Rate limiter local por conta: leitura 1 ponto, escrita 3, decaimento em 300 s.
//! O máximo depende do tier (60 no Limited, 9 000 no Full); o limiter espera antes de
//! passar de 80% e desiste se a espera passar de 60 s.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::client::{AccessTier, Usage};
use crate::error::MetaError;
use crate::retry::MAX_WAIT;

pub const COST_READ: u32 = 1;
pub const COST_WRITE: u32 = 3;
const DECAY: Duration = Duration::from_secs(300);
const DEFAULT_BLOCK: Duration = Duration::from_secs(60);
const SATURATED_PCT: f64 = 90.0;

#[derive(Default)]
struct AccountState {
    tier: Option<AccessTier>,
    calls: VecDeque<(Instant, u32)>,
    blocked_until: Option<Instant>,
}

impl AccountState {
    fn threshold(&self) -> u32 {
        match self.tier {
            Some(AccessTier::Full) => 9000 * 8 / 10,
            _ => 60 * 8 / 10,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self.calls.front().is_some_and(|(t, _)| *t + DECAY <= now) {
            self.calls.pop_front();
        }
    }

    /// Quanto esperar para `cost` caber abaixo do limiar.
    fn wait_for(&self, now: Instant, cost: u32) -> Duration {
        let blocked = self
            .blocked_until
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or_default();
        let limit = self.threshold();
        let mut used: u32 = self.calls.iter().map(|(_, c)| c).sum();
        let mut decay = Duration::ZERO;
        let mut calls = self.calls.iter();
        while used + cost > limit {
            let Some((t, c)) = calls.next() else { break };
            used -= c;
            decay = (*t + DECAY).saturating_duration_since(now);
        }
        blocked.max(decay)
    }
}

/// Compartilhado por todos os clientes do adapter.
#[derive(Default)]
pub struct AccountLimiter {
    accounts: Mutex<HashMap<String, AccountState>>,
}

impl AccountLimiter {
    pub fn new() -> Arc<AccountLimiter> {
        Arc::new(AccountLimiter::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, AccountState>> {
        self.accounts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reserva `cost` pontos para `account`, esperando se preciso.
    pub async fn acquire(&self, account: &str, cost: u32) -> Result<(), MetaError> {
        loop {
            let wait = {
                let now = Instant::now();
                let mut map = self.lock();
                let st = map.entry(account.to_string()).or_default();
                st.prune(now);
                let wait = st.wait_for(now, cost);
                if wait.is_zero() {
                    st.calls.push_back((now, cost));
                    return Ok(());
                }
                wait
            };
            if wait > MAX_WAIT {
                return Err(MetaError::RateLimited {
                    account: account.to_string(),
                    wait,
                });
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Calibra pelo uso que a Meta informou na resposta.
    pub fn observe(&self, account: &str, usage: &Usage) {
        let now = Instant::now();
        let mut map = self.lock();
        let st = map.entry(account.to_string()).or_default();
        if usage.tier.is_some() {
            st.tier = usage.tier;
        }
        let saturated = usage.buc_max_pct.is_some_and(|p| p >= SATURATED_PCT);
        if saturated || usage.regain_after.is_some() {
            let block = usage.regain_after.unwrap_or(DEFAULT_BLOCK);
            let until = now + block;
            st.blocked_until = Some(st.blocked_until.map_or(until, |t| t.max(until)));
        }
    }
}
