//! The Working Indicator's Codex-style shimmer: one two-second brightness
//! sweep over text, expressed entirely as draw-time styles.

use std::{f64::consts::PI, sync::OnceLock, time::Duration};

use crossterm::style::available_color_count;
use ratatui::style::{Color, Modifier, Style};

pub(super) const TICK_PERIOD: Duration = Duration::from_millis(32);

const CYCLE_MILLIS: u64 = 2_000;
const BAND_HALF_WIDTH: f64 = 5.0;
// Keep the band just far enough beyond either edge that the palette fallback
// shows one leading character before the rest of the label joins the sweep.
const EDGE_OVERHANG: f64 = 3.5;

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
    let progress = elapsed as f64 / CYCLE_MILLIS as f64;
    let first_center = -EDGE_OVERHANG;
    let last_center = character_count.saturating_sub(1) as f64 + EDGE_OVERHANG;
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
        // 125 × 32ms is exactly two 2s sweeps; one sweep is not an integral
        // number of frames at the chosen refresh interval.
        let frames = 125;
        assert_eq!(
            styles("Working", 0, Style::default()),
            styles("Working", frames, Style::default())
        );
    }

    #[test]
    fn a_sweep_enters_on_the_first_character_and_leaves_on_the_last() {
        let base = Style::default();
        let resting = shimmer_style(base, 0.0, false);
        let opening = styles_for_color_mode("Working", 0, base, false);
        let closing = styles_for_color_mode(
            "Working",
            (CYCLE_MILLIS / TICK_PERIOD.as_millis() as u64) as usize,
            base,
            false,
        );

        assert_ne!(opening[0], resting, "the first character starts the sweep");
        assert!(
            opening[1..].iter().all(|style| *style == resting),
            "the sweep does not begin with several characters already highlighted"
        );
        assert_ne!(
            closing[closing.len() - 1],
            resting,
            "the last character finishes the sweep before it wraps"
        );
        assert!(
            closing[..closing.len() - 1]
                .iter()
                .all(|style| *style == resting),
            "the trailing edge has left the earlier characters"
        );
    }
}
