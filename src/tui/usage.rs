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

pub(super) fn compact_cost(cost: Cost) -> String {
    format!("${:.2}", cost.as_usd())
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
    use super::compact_cost;
    use crate::protocol::Cost;

    #[test]
    fn cost_always_has_two_decimal_places() {
        for (usd, expected) in [(0.001, "$0.00"), (1.2, "$1.20"), (10.0, "$10.00")] {
            assert_eq!(compact_cost(Cost::from_usd(usd).unwrap()), expected);
        }
    }
}
