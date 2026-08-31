//! The Working Indicator's Codex-style shimmer: a 1.5-second brightness sweep
//! followed by one second at rest, expressed entirely as draw-time styles.

use std::{f64::consts::PI, sync::OnceLock, time::Duration};

use crossterm::style::available_color_count;
use ratatui::style::{Color, Modifier, Style};

pub(super) const TICK_PERIOD: Duration = Duration::from_millis(32);

const CYCLE_MILLIS: u64 = 2_500;
const SWEEP_MILLIS: u64 = 1_500;
const BAND_HALF_WIDTH: f64 = 5.0;

/// One style per character. All Shimmers share the run loop's frame clock, so
/// appearing text joins the process-wide sweep instead of starting a private
/// timer that would have to enter transcript state or its cache key.
pub(super) fn styles(text: &str, frame: usize, base: Style) -> Vec<Style> {
    styles_for_color_mode(
        text,
        frame,
        base,
        *TRUECOLOR.get_or_init(|| available_color_count() == u16::MAX),
    )
}

fn styles_for_color_mode(text: &str, frame: usize, base: Style, truecolor: bool) -> Vec<Style> {
    let character_count = text.chars().count();
    if character_count == 0 {
        return Vec::new();
    }
    let elapsed = (frame as u64).saturating_mul(TICK_PERIOD.as_millis() as u64) % CYCLE_MILLIS;
    if elapsed >= SWEEP_MILLIS {
        return vec![shimmer_style(base, 0.0, truecolor); character_count];
    }
    let progress = elapsed as f64 / SWEEP_MILLIS as f64;
    let first_center = -BAND_HALF_WIDTH;
    let last_center = character_count.saturating_sub(1) as f64 + BAND_HALF_WIDTH;
    let center = first_center + progress * (last_center - first_center);
    (0..character_count)
        .map(|index| shimmer_style(base, intensity(index as f64 - center), truecolor))
        .collect()
}

fn intensity(distance: f64) -> f64 {
    let distance = distance.abs();
    if distance >= BAND_HALF_WIDTH {
        return 0.0;
    }
    (1.0 + (PI * distance / BAND_HALF_WIDTH).cos()) / 2.0
}

fn shimmer_style(base: Style, intensity: f64, truecolor: bool) -> Style {
    if truecolor {
        // Suru's system surface is dark. A neutral grayscale keeps the effect
        // semantic-theme independent while truecolor lets the sweep remain
        // smooth rather than stepping through terminal palette entries.
        let channel = (88.0 + 167.0 * intensity).round() as u8;
        return base
            .fg(Color::Rgb(channel, channel, channel))
            .add_modifier(Modifier::BOLD);
    }
    if intensity < 0.2 {
        base.add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        base
    } else {
        base.add_modifier(Modifier::BOLD)
    }
}

static TRUECOLOR: OnceLock<bool> = OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_maps_the_sweep_to_dim_plain_and_bold() {
        let base = Style::default();
        assert!(
            shimmer_style(base, 0.0, false)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(shimmer_style(base, 0.4, false), base);
        assert!(
            shimmer_style(base, 1.0, false)
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn a_cycle_returns_to_the_same_styles() {
        // 625 × 32ms is exactly eight 2.5s cycles; one cycle is not an integral
        // number of frames at the chosen refresh interval.
        let frames = 625;
        assert_eq!(
            styles("Working", 0, Style::default()),
            styles("Working", frames, Style::default())
        );
    }

    #[test]
    fn a_sweep_has_dark_boundaries_then_rests_for_one_second() {
        let base = Style::default();
        let midpoint = styles_for_color_mode(
            "Working",
            (SWEEP_MILLIS / 2 / TICK_PERIOD.as_millis() as u64) as usize,
            base,
            false,
        );
        assert!(
            midpoint[midpoint.len() / 2]
                .add_modifier
                .contains(Modifier::BOLD),
            "the sweep reaches the middle of the label within half a second"
        );
        // The sweep does not end on a frame boundary, so the first resting
        // frame is the first tick at or after the sweep's final millisecond.
        let first_resting_frame = SWEEP_MILLIS.div_ceil(TICK_PERIOD.as_millis() as u64);
        let last_resting_frame = CYCLE_MILLIS / TICK_PERIOD.as_millis() as u64;
        for truecolor in [false, true] {
            let resting = shimmer_style(base, 0.0, truecolor);
            let opening = styles_for_color_mode("Working", 0, base, truecolor);
            let closing =
                styles_for_color_mode("Working", first_resting_frame as usize, base, truecolor);

            assert!(
                opening.iter().all(|style| *style == resting),
                "the sweep begins with every character unhighlighted"
            );
            assert!(
                closing.iter().all(|style| *style == resting),
                "the sweep ends with every character unhighlighted"
            );
            assert!(
                (first_resting_frame..=last_resting_frame).all(|frame| {
                    styles_for_color_mode("Working", frame as usize, base, truecolor)
                        .iter()
                        .all(|style| *style == resting)
                }),
                "the label remains unhighlighted for the second half of the cycle"
            );
        }
    }
}
