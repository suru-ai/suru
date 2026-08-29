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
    let usd = cost.as_usd();
    let precision = if usd >= 0.01 {
        2
    } else if usd >= 0.001 {
        3
    } else if usd >= 0.0001 {
        4
    } else {
        return "<$0.0001".to_owned();
    };
    format!("${}", trim_fractional_zeros(format!("{usd:.precision$}")))
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
