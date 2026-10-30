use crate::{CoreError, Platform};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Status {
    #[serde(rename = "ACTIVE", alias = "ENABLED")]
    Active,
    #[serde(rename = "PAUSED")]
    Paused,
    #[serde(rename = "REMOVED", alias = "DELETED")]
    Removed,
    #[serde(rename = "ARCHIVED")]
    Archived,
}

impl Status {
    pub fn from_google(s: &str) -> Result<Status, CoreError> {
        match s {
            "ENABLED" => Ok(Status::Active),
            "PAUSED" => Ok(Status::Paused),
            "REMOVED" => Ok(Status::Removed),
            _ => Err(CoreError::InvalidStatus {
                platform: Platform::GoogleAds,
                value: s.to_string(),
            }),
        }
    }

    pub fn to_google(self) -> Result<&'static str, CoreError> {
        match self {
            Status::Active => Ok("ENABLED"),
            Status::Paused => Ok("PAUSED"),
            Status::Removed => Ok("REMOVED"),
            Status::Archived => Err(CoreError::InvalidStatus {
                platform: Platform::GoogleAds,
                value: "ARCHIVED".to_string(),
            }),
        }
    }

    pub fn from_meta(s: &str) -> Result<Status, CoreError> {
        match s {
            "ACTIVE" => Ok(Status::Active),
            "PAUSED" => Ok(Status::Paused),
            "DELETED" => Ok(Status::Removed),
            "ARCHIVED" => Ok(Status::Archived),
            _ => Err(CoreError::InvalidStatus {
                platform: Platform::MetaAds,
                value: s.to_string(),
            }),
        }
    }

    pub fn to_meta(self) -> &'static str {
        match self {
            Status::Active => "ACTIVE",
            Status::Paused => "PAUSED",
            Status::Removed => "DELETED",
            Status::Archived => "ARCHIVED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_platform_status_rejected() {
        assert!(Status::from_meta("ENABLED").is_err());
        assert!(Status::from_google("ACTIVE").is_err());
        assert!(Status::from_google("ARCHIVED").is_err());
    }

    #[test]
    fn google_mapping() {
        assert_eq!(Status::from_google("ENABLED").unwrap(), Status::Active);
        assert_eq!(Status::Active.to_google().unwrap(), "ENABLED");
        assert_eq!(Status::Removed.to_google().unwrap(), "REMOVED");
        assert!(Status::Archived.to_google().is_err());
        assert!(Status::from_google("UNKNOWN").is_err());
    }
    #[test]
    fn meta_mapping() {
        assert_eq!(Status::from_meta("DELETED").unwrap(), Status::Removed);
        assert_eq!(Status::Removed.to_meta(), "DELETED");
        assert_eq!(Status::from_meta("ARCHIVED").unwrap(), Status::Archived);
    }
    #[test]
    fn serde_aliases() {
        assert_eq!(
            serde_json::from_str::<Status>("\"ENABLED\"").unwrap(),
            Status::Active
        );
        assert_eq!(
            serde_json::from_str::<Status>("\"DELETED\"").unwrap(),
            Status::Removed
        );
        assert_eq!(
            serde_json::to_string(&Status::Active).unwrap(),
            "\"ACTIVE\""
        );
    }
}
