/// Meta URLs. Configurable so tests can point them at `wiremock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaEndpoints {
    /// Graph API base, with the version (`https://graph.facebook.com/v26.0`).
    pub graph_base: String,
    /// Login dialog (`https://www.facebook.com/v26.0/dialog/oauth`).
    pub dialog_url: String,
}

impl Default for MetaEndpoints {
    fn default() -> Self {
        Self {
            graph_base: "https://graph.facebook.com/v26.0".into(),
            dialog_url: "https://www.facebook.com/v26.0/dialog/oauth".into(),
        }
    }
}
