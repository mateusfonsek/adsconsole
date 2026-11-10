/// Google URLs; configurable so tests can point them at `wiremock`.
#[derive(Debug, Clone)]
pub struct GoogleEndpoints {
    pub api_base: String,
    pub upload_base: String,
    pub auth_url: String,
    pub token_url: String,
}

impl Default for GoogleEndpoints {
    fn default() -> Self {
        Self {
            api_base: "https://googleads.googleapis.com/v26".into(),
            upload_base: "https://googleads.googleapis.com/resumable/upload/v26".into(),
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
        }
    }
}

/// Google Cloud Console links used in the setup instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLinks {
    pub api: String,
    pub consent: String,
    pub clients: String,
}

pub fn console_links(project_id: &str) -> ConsoleLinks {
    ConsoleLinks {
        api: format!(
            "https://console.cloud.google.com/apis/api/googleads.googleapis.com/overview?project={project_id}"
        ),
        consent: format!("https://console.cloud.google.com/auth/overview?project={project_id}"),
        clients: format!("https://console.cloud.google.com/auth/clients?project={project_id}"),
    }
}
