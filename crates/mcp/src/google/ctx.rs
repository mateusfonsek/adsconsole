//! Contexto de uma chamada do Google Ads já resolvida para uma conta.

use std::sync::Arc;
use std::time::Duration;

use adsconsole_core::db::Db;
use adsconsole_google_ads::client::GoogleAdsClient;

use super::connections::AccessLevel;

/// Montado por [`super::GoogleAdapter::ctx`] a partir do cache de contas e da conexão.
#[derive(Clone)]
pub struct Ctx {
    pub client: GoogleAdsClient,
    pub customer_id: String,
    /// Valor do header `login-customer-id`: a MCC sob a qual a conta foi listada no
    /// cache. Conta acessível direto vai sem o header.
    pub login: Option<String>,
    pub currency: String,
    pub project_id: String,
    pub access_level: AccessLevel,
    pub connection_id: String,
    /// Usado para marcar a conexão como `NeedsReconnect` e ler a contagem de operações.
    pub db: Arc<Db>,
    /// Espera do processamento do vídeo em `upload_video`.
    pub upload_poll: UploadPoll,
}

/// Intervalo entre as leituras do estado do upload e, nos testes, o prazo total
/// (em produção o prazo é o `wait_minutes` da entrada).
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
