//! Período das leituras de insights: um preset da Meta ou um intervalo de datas.

use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DatePreset {
    Today,
    Yesterday,
    #[serde(rename = "last_7d")]
    Last7d,
    #[serde(rename = "last_14d")]
    Last14d,
    #[default]
    #[serde(rename = "last_30d")]
    Last30d,
    ThisMonth,
    LastMonth,
}

impl DatePreset {
    pub fn as_str(self) -> &'static str {
        match self {
            DatePreset::Today => "today",
            DatePreset::Yesterday => "yesterday",
            DatePreset::Last7d => "last_7d",
            DatePreset::Last14d => "last_14d",
            DatePreset::Last30d => "last_30d",
            DatePreset::ThisMonth => "this_month",
            DatePreset::LastMonth => "last_month",
        }
    }
}

/// Um preset (`"last_30d"`) ou `{"since": "AAAA-MM-DD", "until": "AAAA-MM-DD"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Period {
    Preset(DatePreset),
    Range {
        /// Primeiro dia (AAAA-MM-DD).
        #[schemars(with = "String")]
        since: NaiveDate,
        /// Último dia (AAAA-MM-DD), inclusive.
        #[schemars(with = "String")]
        until: NaiveDate,
    },
}

impl Default for Period {
    fn default() -> Self {
        Period::Preset(DatePreset::default())
    }
}

impl Period {
    /// `since` depois de `until` é erro.
    pub fn check(&self) -> Result<(), String> {
        match self {
            Period::Range { since, until } if since > until => {
                Err(format!("since ({since}) é depois de until ({until})"))
            }
            _ => Ok(()),
        }
    }
}

/// O parâmetro da Meta: `("date_preset", "last_30d")` ou `("time_range", "{…}")`.
pub fn insights_param(p: &Period) -> (&'static str, String) {
    match p {
        Period::Preset(d) => ("date_preset", d.as_str().to_string()),
        Period::Range { since, until } => (
            "time_range",
            format!(r#"{{"since":"{since}","until":"{until}"}}"#),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods() {
        let p: Period = serde_json::from_str(r#""last_7d""#).unwrap();
        assert_eq!(insights_param(&p), ("date_preset", "last_7d".into()));
        let r: Period =
            serde_json::from_str(r#"{"since":"2026-01-01","until":"2026-01-31"}"#).unwrap();
        assert_eq!(
            insights_param(&r).1,
            r#"{"since":"2026-01-01","until":"2026-01-31"}"#
        );
        assert!(r.check().is_ok());
        let bad: Period =
            serde_json::from_str(r#"{"since":"2026-02-01","until":"2026-01-31"}"#).unwrap();
        assert!(bad.check().is_err());
        assert!(serde_json::from_str::<Period>(r#""ontem""#).is_err());
    }
}
