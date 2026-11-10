//! Context of a Google Ads call already resolved to an account.

use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::db::Db;
use adsconsole_google_ads::client::GoogleAdsClient;

use super::connections::AccessLevel;

/// Built by [`super::GoogleAdapter::ctx`] from the account cache and the connection.
#[derive(Clone)]
pub struct Ctx {
    pub client: GoogleAdsClient,
    pub customer_id: String,
    /// Value of the `login-customer-id` header: the MCC the account was listed under in
    /// the cache. A directly accessible account goes without the header.
    pub login: Option<String>,
    pub currency: String,
    pub project_id: String,
    pub access_level: AccessLevel,
    pub connection_id: String,
    /// Used to mark the connection as `NeedsReconnect` and to read the operation count.
    pub db: Arc<Db>,
    /// Wait for video processing in `upload_video`.
    pub upload_poll: UploadPoll,
}

/// Interval between upload status reads and, in tests, the total deadline
/// (in production the deadline is the input's `wait_minutes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadPoll {
    pub every: Duration,
    pub max: Option<Duration>,
}

impl Default for UploadPoll {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(15),
            max: None,
        }
    }
}
