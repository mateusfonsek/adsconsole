use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::db::Db;
use adsconsole_meta_ads::client::MetaClient;

/// Espera de uploads e cópias assíncronas: lê o estado a cada `every`, por até `max`.
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

/// Contexto de uma chamada numa conta da Meta.
#[derive(Clone)]
pub struct Ctx {
    /// Já com o rate limiter da conta ([`MetaClient::for_account`]).
    pub client: MetaClient,
    /// `act_<dígitos>`.
    pub account_id: String,
    pub currency: String,
    /// Casas decimais da moeda (`money::currency_exponent`).
    pub exponent: u32,
    pub connection_id: String,
    pub db: Arc<Db>,
    /// Aviso de vencimento do token da conexão, quando falta 7 dias ou menos.
    pub token_warning: Option<String>,
    pub upload_poll: UploadPoll,
}
