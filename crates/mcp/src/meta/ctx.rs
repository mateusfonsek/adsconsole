use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::db::Db;
use adsconsole_meta_ads::client::MetaClient;

/// Polling for uploads and async copies: reads the status every `every`, for up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPoll {
    pub every: Duration,
    pub max: Duration,
}

impl Default for UploadPoll {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(10),
            max: Duration::from_secs(600),
        }
    }
}

/// Context of a call on a Meta account.
#[derive(Clone)]
pub struct Ctx {
    /// Already carries the account's rate limiter ([`MetaClient::for_account`]).
    pub client: MetaClient,
    /// `act_<digits>`.
    pub account_id: String,
    pub currency: String,
    /// Decimal places of the currency (`money::currency_exponent`).
    pub exponent: u32,
    pub connection_id: String,
    pub db: Arc<Db>,
    /// Expiry warning for the connection's token, when 7 days or fewer remain.
    pub token_warning: Option<String>,
    pub upload_poll: UploadPoll,
}
