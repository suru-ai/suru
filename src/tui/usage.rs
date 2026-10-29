//! Compact Usage and Cost presentation shared by Session surfaces.

use crate::protocol::Cost;

pub(super) fn compact_count(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    let (divisor, suffix) = if value >= 1_000_000_000_000 {
        (1_000_000_000_000.0, "T")
    } else if value >= 1_000_000_000 {
        (1_000_000_000.0, "B")
    } else if value >= 1_000_000 {
        (1_000_000.0, "M")
    } else {
        (1_000.0, "K")
    };
    let scaled = value as f64 / divisor;
    let precision = if scaled < 10.0 {
        2
    } else if scaled < 100.0 {
        1
    } else {
        0
    };
    format!(
        "{}{suffix}",
        trim_fractional_zeros(format!("{scaled:.precision$}"))
    )
}

/// `value` written whole, its thousands grouped by commas: `1,213`. Where
/// [`compact_count`] rounds a figure to fit beside others, this states a
/// count a reader may want exactly.
pub(super) fn grouped_count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (place, digit) in digits.chars().enumerate() {
        if place > 0 && (digits.len() - place).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub(super) fn compact_cost(cost: Cost) -> String {
    compact_dollars(cost.nano_usd())
}

/// `nano_usd` billionths of a dollar, written as every dollar figure is —
/// a Cost's, and an estimate that is none.
pub(super) fn compact_dollars(nano_usd: u64) -> String {
    format!("${:.2}", Cost::from_nano_usd(nano_usd).as_usd())
}

fn trim_fractional_zeros(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{compact_cost, grouped_count};
    use crate::protocol::Cost;

    #[test]
    fn cost_always_has_two_decimal_places() {
        for (usd, expected) in [(0.001, "$0.00"), (1.2, "$1.20"), (10.0, "$10.00")] {
            assert_eq!(compact_cost(Cost::from_usd(usd).unwrap()), expected);
        }
    }

    #[test]
    fn a_grouped_count_puts_a_comma_between_each_three_digits() {
        for (value, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (1_213, "1,213"),
            (12_345_678, "12,345,678"),
            (u64::MAX, "18,446,744,073,709,551,615"),
        ] {
            assert_eq!(grouped_count(value), expected);
        }
    }
}
