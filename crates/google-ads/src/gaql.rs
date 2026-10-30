//! GAQL seguro: aspas, ids numéricos e cláusulas de data.

use chrono::NaiveDate;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GaqlError {
    #[error("id inválido (só dígitos): {0:?}")]
    InvalidId(String),
    #[error("intervalo de datas inválido: início {start} depois do fim {end}")]
    InvalidRange { start: NaiveDate, end: NaiveDate },
}

/// Envolve em aspas simples, escapando `\` e `'`.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\\' || c == '\'' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Aceita só dígitos (e não vazio).
pub fn id(s: &str) -> Result<&str, GaqlError> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        Ok(s)
    } else {
        Err(GaqlError::InvalidId(s.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DatePreset {
    Today,
    Yesterday,
    Last7Days,
    Last14Days,
    #[default]
    Last30Days,
    ThisMonth,
    LastMonth,
}

impl DatePreset {
    pub fn as_gaql(self) -> &'static str {
        match self {
            Self::Today => "TODAY",
            Self::Yesterday => "YESTERDAY",
            Self::Last7Days => "LAST_7_DAYS",
            Self::Last14Days => "LAST_14_DAYS",
            Self::Last30Days => "LAST_30_DAYS",
            Self::ThisMonth => "THIS_MONTH",
            Self::LastMonth => "LAST_MONTH",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateRange {
    Preset(DatePreset),
    Between { start: NaiveDate, end: NaiveDate },
}

impl Default for DateRange {
    fn default() -> Self {
        Self::Preset(DatePreset::default())
    }
}

pub fn date_clause(r: &DateRange) -> Result<String, GaqlError> {
    match r {
        DateRange::Preset(p) => Ok(format!("segments.date DURING {}", p.as_gaql())),
        DateRange::Between { start, end } => {
            if start > end {
                return Err(GaqlError::InvalidRange {
                    start: *start,
                    end: *end,
                });
            }
            Ok(format!(
                "segments.date BETWEEN '{}' AND '{}'",
                start.format("%Y-%m-%d"),
                end.format("%Y-%m-%d")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn quoting_and_ids() {
        assert_eq!(quote("O'Brien"), "'O\\'Brien'");
        assert_eq!(quote("a\\b"), "'a\\\\b'");
        assert!(id("123").is_ok());
        assert!(id("12 OR 1=1").is_err());
        assert!(id("").is_err());
    }

    #[test]
    fn date_clauses() {
        assert_eq!(
            date_clause(&DateRange::default()).unwrap(),
            "segments.date DURING LAST_30_DAYS"
        );
        assert_eq!(
            date_clause(&DateRange::Preset(DatePreset::LastMonth)).unwrap(),
            "segments.date DURING LAST_MONTH"
        );
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        assert_eq!(
            date_clause(&DateRange::Between {
                start: d(1, 1),
                end: d(1, 31)
            })
            .unwrap(),
            "segments.date BETWEEN '2026-01-01' AND '2026-01-31'"
        );
        assert!(
            date_clause(&DateRange::Between {
                start: d(2, 1),
                end: d(1, 31)
            })
            .is_err()
        );
    }
}
