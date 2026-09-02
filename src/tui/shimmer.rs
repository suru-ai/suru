//! The Working Indicator's Codex-style shimmer: a 1.5-second brightness sweep
//! followed by one second at rest, expressed entirely as draw-time styles.

use std::{f64::consts::PI, time::Duration};

use ratatui::style::{Color, Modifier, Style};

pub(super) const TICK_PERIOD: Duration = Duration::from_millis(32);

const CYCLE_MILLIS: u64 = 2_500;
const SWEEP_MILLIS: u64 = 1_500;
const BAND_HALF_WIDTH: f64 = 5.0;

/// One style per character. All Shimmers share the run loop's frame clock, so
/// appearing text joins the process-wide sweep instead of starting a private
/// timer that would have to enter transcript state or its cache key.
pub(super) fn styles(
    text: &str,
    frame: usize,
    primary: Style,
    subdued: Style,
    truecolor: bool,
) -> Vec<Style> {
    let character_count = text.chars().count();
    if character_count == 0 {
        return Vec::new();
    }
    let elapsed = (frame as u64).saturating_mul(TICK_PERIOD.as_millis() as u64) % CYCLE_MILLIS;
    if elapsed >= SWEEP_MILLIS {
        return vec![shimmer_style(primary, subdued, 0.0, truecolor); character_count];
    }
    let progress = elapsed as f64 / SWEEP_MILLIS as f64;
    let first_center = -BAND_HALF_WIDTH;
    let last_center = character_count.saturating_sub(1) as f64 + BAND_HALF_WIDTH;
    let center = first_center + progress * (last_center - first_center);
    (0..character_count)
        .map(|index| {
            shimmer_style(
                primary,
                subdued,
                intensity(index as f64 - center),
                truecolor,
            )
        })
        .collect()
}

fn intensity(distance: f64) -> f64 {
    let distance = distance.abs();
    if distance >= BAND_HALF_WIDTH {
        return 0.0;
    }
    (1.0 + (PI * distance / BAND_HALF_WIDTH).cos()) / 2.0
}

fn shimmer_style(primary: Style, subdued: Style, intensity: f64, truecolor: bool) -> Style {
    if truecolor {
        if let (Some(primary_channels), Some(subdued_channels)) = (
            primary.fg.and_then(color_channels),
            subdued.fg.and_then(color_channels),
        ) {
            let color = Color::Rgb(
                blend(subdued_channels.0, primary_channels.0, intensity),
                blend(subdued_channels.1, primary_channels.1, intensity),
                blend(subdued_channels.2, primary_channels.2, intensity),
            );
            return primary.fg(color).add_modifier(Modifier::BOLD);
        }
        if let (Some(primary_color), Some(subdued_color)) = (primary.fg, subdued.fg) {
            let color = if intensity < 0.5 {
                subdued_color
            } else {
                primary_color
            };
            return primary.fg(color).add_modifier(Modifier::BOLD);
        }
    }
    if intensity < 0.2 {
        primary.add_modifier(Modifier::DIM)
    } else if intensity < 0.6 {
        primary
    } else {
        primary.add_modifier(Modifier::BOLD)
    }
}

fn blend(from: u8, to: u8, intensity: f64) -> u8 {
    (f64::from(from) + (f64::from(to) - f64::from(from)) * intensity).round() as u8
}

fn color_channels(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset | Color::White => Some((255, 255, 255)),
        Color::DarkGray => Some((88, 88, 88)),
        Color::Black => Some((0, 0, 0)),
        Color::Gray => Some((192, 192, 192)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        Color::Indexed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn fallback_maps_the_sweep_to_dim_plain_and_bold() {
        let base = Style::default();
        assert!(
            shimmer_style(base, base, 0.0, false)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(shimmer_style(base, base, 0.4, false), base);
        assert!(
            shimmer_style(base, base, 1.0, false)
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn truecolor_sweep_blends_the_themes_subdued_and_primary_text() {
        let primary = Style::default().fg(Color::Rgb(220, 180, 140));
        let subdued = Style::default().fg(Color::Rgb(20, 40, 60));

        assert_eq!(
            shimmer_style(primary, subdued, 0.0, true).fg,
            Some(Color::Rgb(20, 40, 60))
        );
        assert_eq!(
            shimmer_style(primary, subdued, 0.5, true).fg,
            Some(Color::Rgb(120, 110, 100))
        );
        assert_eq!(
            shimmer_style(primary, subdued, 1.0, true).fg,
            Some(Color::Rgb(220, 180, 140))
        );
    }

    #[test]
    fn the_system_theme_keeps_the_existing_truecolor_grayscale() {
        let theme = Theme::system();

        assert_eq!(
            shimmer_style(theme.text.primary, theme.text.subdued, 0.0, true),
            Style::default()
                .fg(Color::Rgb(88, 88, 88))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            shimmer_style(theme.text.primary, theme.text.subdued, 0.5, true),
            Style::default()
                .fg(Color::Rgb(172, 172, 172))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            shimmer_style(theme.text.primary, theme.text.subdued, 1.0, true),
            Style::default()
                .fg(Color::Rgb(255, 255, 255))
                .add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn a_cycle_returns_to_the_same_styles() {
        // 625 × 32ms is exactly eight 2.5s cycles; one cycle is not an integral
        // number of frames at the chosen refresh interval.
        let frames = 625;
        assert_eq!(
            styles("Working", 0, Style::default(), Style::default(), true,),
            styles("Working", frames, Style::default(), Style::default(), true,)
        );
    }

    #[test]
    fn a_sweep_has_dark_boundaries_then_rests_for_one_second() {
        let base = Style::default();
        let midpoint = styles(
            "Working",
            (SWEEP_MILLIS / 2 / TICK_PERIOD.as_millis() as u64) as usize,
            base,
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
            let resting = shimmer_style(base, base, 0.0, truecolor);
            let opening = styles("Working", 0, base, base, truecolor);
            let closing = styles(
                "Working",
                first_resting_frame as usize,
                base,
                base,
                truecolor,
            );

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
                    styles("Working", frame as usize, base, base, truecolor)
                        .iter()
                        .all(|style| *style == resting)
                }),
                "the label remains unhighlighted for the second half of the cycle"
            );
        }
    }
}
