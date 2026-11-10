use crate::CoreError;
pub use rust_decimal::Decimal;

const MICROS_EXPONENT: u32 = 6;

/// Converts a decimal amount to micros (6 decimal places), without rounding.
pub fn to_micros(amount: Decimal) -> Result<i64, CoreError> {
    to_minor_units(amount, MICROS_EXPONENT)
}

pub fn from_micros(micros: i64) -> Decimal {
    from_minor_units(micros, MICROS_EXPONENT)
}

/// Converts a decimal amount to the currency's smallest unit (`exponent` decimal places).
/// Negative amounts, or amounts with more decimal places than `exponent`, are rejected without rounding.
pub fn to_minor_units(amount: Decimal, exponent: u32) -> Result<i64, CoreError> {
    if amount.is_sign_negative() && !amount.is_zero() {
        return Err(CoreError::InvalidAmount(format!(
            "{amount} is negative; the amount must be greater than or equal to zero"
        )));
    }
    if amount.normalize().scale() > exponent {
        return Err(CoreError::InvalidAmount(format!(
            "{amount} has more than {exponent} decimal places"
        )));
    }
    let factor = 10_i64
        .checked_pow(exponent)
        .map(Decimal::from)
        .ok_or_else(|| {
            CoreError::InvalidAmount(format!("{exponent} decimal places is too many"))
        })?;
    amount
        .checked_mul(factor)
        .and_then(|v| i64::try_from(v.normalize()).ok())
        .ok_or_else(|| CoreError::InvalidAmount(format!("{amount} is too large")))
}

/// Currency decimal places (ISO 4217): 0 for the listed zero-decimal currencies, 2 for the rest.
pub fn currency_exponent(code: &str) -> u32 {
    const ZERO_DECIMAL: [&str; 8] = ["JPY", "KRW", "CLP", "VND", "ISK", "PYG", "UGX", "XOF"];
    if ZERO_DECIMAL.iter().any(|c| c.eq_ignore_ascii_case(code)) {
        0
    } else {
        2
    }
}

/// Rejects decimal places beyond the currency exponent and converts to micros.
pub fn to_micros_checked(amount: Decimal, currency: &str) -> Result<i64, CoreError> {
    to_minor_units(amount, currency_exponent(currency))?;
    to_micros(amount)
}

pub fn from_minor_units(units: i64, exponent: u32) -> Decimal {
    Decimal::new(units, exponent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn minor_units_large_exponent() {
        assert!(to_minor_units(dec!(1), 19).is_err());
        assert_eq!(to_minor_units(dec!(25.40), 2).unwrap(), 2540);
    }

    #[test]
    fn micros_roundtrip() {
        assert_eq!(to_micros(dec!(25.40)).unwrap(), 25_400_000);
        assert_eq!(from_micros(25_400_000), dec!(25.4));
        assert_eq!(to_micros(dec!(0.1) + dec!(0.2)).unwrap(), 300_000);
    }
    #[test]
    fn micros_rejects_bad_amounts() {
        assert!(matches!(
            to_micros(dec!(1.0000001)),
            Err(CoreError::InvalidAmount(_))
        ));
        assert!(matches!(
            to_micros(dec!(-1)),
            Err(CoreError::InvalidAmount(_))
        ));
    }
    #[test]
    fn currency_exponents() {
        assert_eq!(currency_exponent("jpy"), 0);
        assert_eq!(currency_exponent("BRL"), 2);
        assert_eq!(to_micros_checked(dec!(1000), "JPY").unwrap(), 1_000_000_000);
        assert!(to_micros_checked(dec!(10.5), "JPY").is_err());
        assert!(to_micros_checked(dec!(25.405), "BRL").is_err());
        assert_eq!(to_micros_checked(dec!(25.40), "BRL").unwrap(), 25_400_000);
    }
    #[test]
    fn minor_units_by_exponent() {
        assert_eq!(to_minor_units(dec!(25.40), 2).unwrap(), 2540);
        assert_eq!(to_minor_units(dec!(100), 0).unwrap(), 100);
        assert!(to_minor_units(dec!(100.5), 0).is_err());
        assert_eq!(from_minor_units(2540, 2), dec!(25.40));
    }
}
