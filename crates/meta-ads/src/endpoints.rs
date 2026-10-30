/// URLs da Meta. Configuráveis para os testes apontarem para o `wiremock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaEndpoints {
    /// Base da Graph API, com a versão (`https://graph.facebook.com/v26.0`).
    pub graph_base: String,
    /// Diálogo de login (`https://www.facebook.com/v26.0/dialog/oauth`).
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
