//! Draw-time brightness sweeps at the speed of the original “Working” label,
//! followed by one second at rest. Distances are measured in terminal columns.

use std::{cell::Cell, f64::consts::PI, time::Duration};

use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) const TICK_PERIOD: Duration = Duration::from_millis(32);

const REST_MICROS: u128 = 1_000_000;
// Working spans seven columns. Including the five-column band beyond each
// end, its center travels 16 columns in the original 1.5-second sweep.
const MICROS_PER_COLUMN: u128 = 1_500_000 / 16;
const BAND_HALF_WIDTH: f64 = 5.0;

/// A label-local origin on the existing animation clock; no additional timer
/// or transcript cache dependency is needed when the label changes.
#[derive(Clone, Debug, Default)]
pub(super) struct Clock {
    origin: Cell<Option<(&'static str, usize)>>,
}

impl Clock {
    pub(super) fn frame(&self, label: &'static str, frame: usize) -> usize {
        let origin = match self.origin.get() {
            Some((previous, origin)) if previous == label => origin,
            _ => {
                self.origin.set(Some((label, frame)));
                frame
            }
        };
        frame.wrapping_sub(origin)
    }
}

fn sweep_micros(width: usize) -> u128 {
    (width.saturating_sub(1) as u128 + 10) * MICROS_PER_COLUMN
}

/// One style per extended grapheme cluster, sampled at its first terminal
/// column. Combining sequences and wide glyphs remain intact during rendering.
pub(super) fn styles(
    text: &str,
    frame: usize,
    primary: Style,
    subdued: Style,
    truecolor: bool,
) -> Vec<Style> {
    let sweep = sweep_micros(UnicodeWidthStr::width(text));
    let elapsed = (frame as u128 * TICK_PERIOD.as_micros()) % (sweep + REST_MICROS);
    let center = -BAND_HALF_WIDTH + elapsed as f64 / MICROS_PER_COLUMN as f64;
    let mut column = 0;
    text.graphemes(true)
        .map(|grapheme| {
            let brightness = if elapsed >= sweep {
                0.0
            } else {
                intensity(column as f64 - center)
            };
            column += UnicodeWidthStr::width(grapheme);
            shimmer_style(primary, subdued, brightness, truecolor)
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

/// A two-second fade to half opacity and two seconds back, beginning at full
/// strength. Unknown terminal backgrounds and limited colors use dim instead.
pub(super) fn fade_style(style: Style, frame: usize, truecolor: bool) -> Style {
    let elapsed = (frame as u128 * TICK_PERIOD.as_micros()) % 4_000_000;
    let opacity = 0.75 + 0.25 * (elapsed as f64 * PI / 2_000_000.0).cos();
    if elapsed == 0 {
        return style;
    }
    if truecolor
        && let (Some(foreground), Some(background)) = (style.fg, style.bg)
        && foreground != Color::Reset
        && background != Color::Reset
        && let (Some(fg), Some(bg)) = (color_channels(foreground), color_channels(background))
    {
        return style.fg(Color::Rgb(
            blend(bg.0, fg.0, opacity),
            blend(bg.1, fg.1, opacity),
            blend(bg.2, fg.2, opacity),
        ));
    }
    if opacity < 0.75 {
        style.add_modifier(Modifier::DIM)
    } else {
        style
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
    fn labels_share_speed_but_have_length_dependent_cycles() {
        assert_eq!(sweep_micros(7), 1_500_000);
        assert_eq!(sweep_micros(21), 2_812_500);
        let primary = Style::default().fg(Color::White);
        let subdued = Style::default().fg(Color::Black);
        for frame in 0..47 {
            let short = styles("Working", frame, primary, subdued, true);
            let long = styles("Waiting for subagents", frame, primary, subdued, true);
            assert_eq!(short, long[..7]);
        }
        let resting = shimmer_style(primary, subdued, 0.0, true);
        // Every sampled frame in the longer label's one-second rest is dark.
        for frame in 88..120 {
            assert!(
                styles("Waiting for subagents", frame, primary, subdued, true)
                    .iter()
                    .all(|style| *style == resting)
            );
        }
        assert!(
            styles("Waiting for subagents", 125, primary, subdued, true)
                .iter()
                .any(|style| *style != resting)
        );
    }

    #[test]
    fn unicode_is_positioned_by_columns_without_splitting_graphemes() {
        let primary = Style::default().fg(Color::White);
        let subdued = Style::default().fg(Color::Black);
        for frame in 0..100 {
            let ascii = styles("abcdefg", frame, primary, subdued, true);
            let wide = styles("界abcde", frame, primary, subdued, true);
            assert_eq!(wide[0], ascii[0]);
            assert_eq!(wide[1..], ascii[2..]);
            assert_eq!(
                styles("a\u{301}bcdefg", frame, primary, subdued, true),
                ascii
            );
            assert!(styles("", frame, primary, subdued, true).is_empty());
        }
    }

    #[test]
    fn changing_labels_restarts_the_sweep_on_the_shared_clock() {
        let clock = Clock::default();
        assert_eq!(clock.frame("Loading", 100), 0);
        assert_eq!(clock.frame("Loading", 110), 10);
        assert_eq!(clock.frame("Working", 110), 0);
        assert_eq!(clock.frame("Working", 120), 10);
        assert_eq!(clock.frame("Waiting for subagents", 120), 0);
        assert_eq!(clock.frame("Working", 130), 0);
    }

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
            (1_500 / 2 / TICK_PERIOD.as_millis()) as usize,
            base,
            base,
            false,
        );
        assert!(
            midpoint[midpoint.len() / 2]
                .add_modifier
                .contains(Modifier::BOLD),
            "the sweep reaches the middle of the label halfway through the sweep"
        );
        // The sweep does not end on a frame boundary, so the first resting
        // frame is the first tick at or after the sweep's final millisecond.
        let first_resting_frame = 1_500_u64.div_ceil(TICK_PERIOD.as_millis() as u64);
        let last_resting_frame = 2_500 / TICK_PERIOD.as_millis() as u64;
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
                "the label remains unhighlighted during the one-second rest"
            );
        }
    }
}
