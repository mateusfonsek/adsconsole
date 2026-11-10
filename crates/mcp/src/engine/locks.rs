use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use adsconsole_core::Platform;
use tokio::sync::OwnedMutexGuard;

type AccountKey = (Platform, String);

/// One mutex per account: writes to the same account run one at a time.
#[derive(Default)]
pub(crate) struct AccountLocks {
    map: Mutex<HashMap<AccountKey, Arc<tokio::sync::Mutex<()>>>>,
}

impl AccountLocks {
    pub(crate) async fn lock(&self, platform: Platform, account_id: &str) -> OwnedMutexGuard<()> {
        let m = {
            let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.entry((platform, account_id.to_string()))
                .or_default()
                .clone()
        };
        m.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_account_shares_lock_other_accounts_do_not() {
        let locks = AccountLocks::default();
        let _g = locks.lock(Platform::GoogleAds, "111").await;
        // Another account and another platform do not wait.
        let _o = locks.lock(Platform::GoogleAds, "222").await;
        let _m = locks.lock(Platform::MetaAds, "111").await;
        let again = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            locks.lock(Platform::GoogleAds, "111"),
        )
        .await;
        assert!(again.is_err(), "the same account should wait");
    }
}
