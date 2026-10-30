use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    GoogleAds,
    MetaAds,
}

impl Platform {
    pub const ALL: [Platform; 2] = [Platform::GoogleAds, Platform::MetaAds];

    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::GoogleAds => "google_ads",
            Platform::MetaAds => "meta_ads",
        }
    }

    pub fn endpoint(&self) -> &'static str {
        match self {
            Platform::GoogleAds => "/mcp/google-ads",
            Platform::MetaAds => "/mcp/meta-ads",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Platform::GoogleAds => "Google Ads",
            Platform::MetaAds => "Meta Ads",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_serde() {
        assert_eq!(Platform::GoogleAds.endpoint(), "/mcp/google-ads");
        assert_eq!(Platform::MetaAds.display_name(), "Meta Ads");
        for p in Platform::ALL {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
        }
    }
}
