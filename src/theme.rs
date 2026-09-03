//! Semantic terminal styles used by built-in renderers.

use std::{collections::HashMap, fmt, sync::OnceLock};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::terminal::{TerminalColor, TerminalFacts};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Theme {
    pub(crate) text: TextRoles,
    pub(crate) surface: SurfaceRoles,
    pub(crate) accent: AccentRoles,
    pub(crate) ansi: AnsiPalette,
    #[allow(dead_code)] // Reserved by the required semantic contract for command affordances.
    pub(crate) action: ActionRoles,
    pub(crate) form_field: FormFieldRoles,
    pub(crate) feedback: FeedbackRoles,
    pub(crate) border: BorderRoles,
    pub(crate) markdown: MarkdownRoles,
    #[allow(dead_code)] // Reserved by the required semantic contract for selectable UI.
    pub(crate) selection: SelectionRoles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRoles {
    pub(crate) primary: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SurfaceRoles {
    /// The application's own canvas. `Color::Reset` means the attached
    /// terminal keeps showing through.
    pub(crate) base: Style,
    pub(crate) elevated: Style,
    pub(crate) overlay: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccentRoles {
    pub(crate) primary: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiPalette {
    pub(crate) normal: AnsiColors,
    pub(crate) bright: AnsiColors,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiColors {
    pub(crate) black: Color,
    pub(crate) red: Color,
    pub(crate) green: Color,
    pub(crate) yellow: Color,
    pub(crate) blue: Color,
    pub(crate) magenta: Color,
    pub(crate) cyan: Color,
    pub(crate) white: Color,
}

impl AnsiPalette {
    pub(crate) fn color(&self, index: u16, bright: bool) -> Option<Color> {
        let colors = if bright { self.bright } else { self.normal };
        Some(match index {
            0 => colors.black,
            1 => colors.red,
            2 => colors.green,
            3 => colors.yellow,
            4 => colors.blue,
            5 => colors.magenta,
            6 => colors.cyan,
            7 => colors.white,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // The roles are defined before command affordances consume them.
pub(crate) struct ActionRoles {
    pub(crate) primary: Style,
    pub(crate) disabled: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FormFieldRoles {
    pub(crate) text: Style,
    pub(crate) placeholder: Style,
    pub(crate) border: Style,
    pub(crate) invalid: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackRoles {
    pub(crate) error: Style,
    pub(crate) warning: Style,
    pub(crate) success: Style,
    #[allow(dead_code)] // Informational feedback has no current transcript variant.
    pub(crate) info: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BorderRoles {
    pub(crate) default: Style,
    pub(crate) subdued: Style,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarkdownRoles {
    pub(crate) heading: Style,
    pub(crate) emphasis: Style,
    pub(crate) strong: Style,
    pub(crate) link: Style,
    pub(crate) inline_code: Style,
    pub(crate) code_block: Style,
    pub(crate) list_marker: Style,
}

/// How a surface says which of its rows a reader means. The two questions are
/// separate wherever a list stands beside the thing it lists: which row the
/// keys would act on, and which row is the one already on show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectionRoles {
    /// The row the keys are on, drawn only while the surface holding it has
    /// them.
    pub(crate) focused: Style,
    /// The row a surface that has given the keys up would come back to, where
    /// that surface keeps one. The Sidebar keeps none: its row focus goes with
    /// the keys.
    pub(crate) unfocused: Style,
    /// The row standing for what the main view is already showing, which is
    /// true whoever holds the keys.
    pub(crate) open: Style,
    /// The column down the left of that row, which is the combined state's own
    /// role: it is what says "open" while the row itself carries the focus
    /// style, and so it is the one a theme has to keep legible against
    /// [`Self::focused`] rather than against the surface behind it.
    pub(crate) open_rail: Style,
}

#[derive(Clone, Debug)]
pub(crate) struct ThemeError(String);

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ThemeError {}

#[derive(Deserialize)]
struct ThemeDocument {
    version: Option<u64>,
    #[serde(default)]
    defs: Map<String, Value>,
    theme: Map<String, Value>,
}

struct DocumentResolver<'a> {
    defs: &'a Map<String, Value>,
    theme: &'a Map<String, Value>,
    resolved: HashMap<String, Color>,
}

impl<'a> DocumentResolver<'a> {
    fn new(document: &'a ThemeDocument) -> Self {
        Self {
            defs: &document.defs,
            theme: &document.theme,
            resolved: HashMap::new(),
        }
    }

    fn required(&mut self, key: &str) -> Result<Color, ThemeError> {
        if let Some(color) = self.resolved.get(key) {
            return Ok(*color);
        }
        let value = self
            .theme
            .get(key)
            .ok_or_else(|| ThemeError(format!("Theme key {key:?} is missing")))?;
        let color = self.value(value, &mut Vec::new())?;
        self.resolved.insert(key.to_owned(), color);
        Ok(color)
    }

    fn optional(&mut self, key: &str, fallback: &str) -> Result<Color, ThemeError> {
        if !self.theme.contains_key(key) {
            return self.required(fallback);
        }
        self.required(key)
    }

    fn value(&self, value: &Value, chain: &mut Vec<String>) -> Result<Color, ThemeError> {
        match value {
            Value::String(value) if value == "transparent" || value == "none" => Ok(Color::Reset),
            Value::String(value) if value.starts_with('#') => parse_hex(value),
            Value::String(reference) => {
                if chain.iter().any(|seen| seen == reference) {
                    chain.push(reference.clone());
                    return Err(ThemeError(format!(
                        "Circular color reference: {}",
                        chain.join(" -> ")
                    )));
                }
                let next = self
                    .defs
                    .get(reference)
                    .or_else(|| self.theme.get(reference))
                    .ok_or_else(|| {
                        ThemeError(format!(
                            "Color reference {reference:?} not found in defs or theme"
                        ))
                    })?;
                chain.push(reference.clone());
                let result = self.value(next, chain);
                chain.pop();
                result
            }
            Value::Number(index) => index
                .as_u64()
                .and_then(|index| u8::try_from(index).ok())
                .map(ansi_color)
                .ok_or_else(|| ThemeError(format!("ANSI palette index {index} is not 0..255"))),
            Value::Object(variants) => variants
                .get("dark")
                .ok_or_else(|| ThemeError("color pair has no dark value".to_owned()))
                .and_then(|dark| self.value(dark, chain)),
            _ => Err(ThemeError(format!("invalid Theme color value {value}"))),
        }
    }
}

fn parse_hex(value: &str) -> Result<Color, ThemeError> {
    let expanded;
    let digits = match value.len() {
        4 => {
            expanded = value[1..]
                .chars()
                .flat_map(|digit| [digit, digit])
                .collect::<String>();
            expanded.as_str()
        }
        // Ratatui has no alpha channel. OpenCode's translucent accent roles
        // retain their authored RGB while the terminal supplies the surface.
        7 | 9 => &value[1..7],
        _ => return Err(ThemeError(format!("invalid hex color {value:?}"))),
    };
    let channel = |start| {
        u8::from_str_radix(&digits[start..start + 2], 16)
            .map_err(|_| ThemeError(format!("invalid hex color {value:?}")))
    };
    Ok(Color::Rgb(channel(0)?, channel(2)?, channel(4)?))
}

#[derive(Clone, Copy)]
struct Rgb {
    red: u8,
    green: u8,
    blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

impl From<TerminalColor> for Rgb {
    fn from(color: TerminalColor) -> Self {
        Self::new(color.red, color.green, color.blue)
    }
}

fn ansi_rgb(index: u8) -> Rgb {
    const BASIC: [Rgb; 16] = [
        Rgb::new(0, 0, 0),
        Rgb::new(128, 0, 0),
        Rgb::new(0, 128, 0),
        Rgb::new(128, 128, 0),
        Rgb::new(0, 0, 128),
        Rgb::new(128, 0, 128),
        Rgb::new(0, 128, 128),
        Rgb::new(192, 192, 192),
        Rgb::new(128, 128, 128),
        Rgb::new(255, 0, 0),
        Rgb::new(0, 255, 0),
        Rgb::new(255, 255, 0),
        Rgb::new(0, 0, 255),
        Rgb::new(255, 0, 255),
        Rgb::new(0, 255, 255),
        Rgb::new(255, 255, 255),
    ];
    if index < 16 {
        BASIC[usize::from(index)]
    } else if index < 232 {
        let index = index - 16;
        let channel = |part: u8| if part == 0 { 0 } else { part * 40 + 55 };
        Rgb::new(
            channel(index / 36),
            channel((index / 6) % 6),
            channel(index % 6),
        )
    } else {
        let gray = (index - 232) * 10 + 8;
        Rgb::new(gray, gray, gray)
    }
}

fn ansi_color(index: u8) -> Color {
    let rgb = ansi_rgb(index);
    Color::Rgb(rgb.red, rgb.green, rgb.blue)
}

#[derive(Clone, Copy)]
struct Oklab {
    lightness: f64,
    green_red: f64,
    blue_yellow: f64,
}

impl Oklab {
    fn from_rgb(rgb: Rgb) -> Self {
        let red = srgb_to_linear(rgb.red);
        let green = srgb_to_linear(rgb.green);
        let blue = srgb_to_linear(rgb.blue);
        let lightness =
            (0.412_221_470_8 * red + 0.536_332_536_3 * green + 0.051_445_992_9 * blue).cbrt();
        let medium =
            (0.211_903_498_2 * red + 0.680_699_545_1 * green + 0.107_396_956_6 * blue).cbrt();
        let short =
            (0.088_302_461_9 * red + 0.281_718_837_6 * green + 0.629_978_700_5 * blue).cbrt();
        Self {
            lightness: 0.210_454_255_3 * lightness + 0.793_617_785 * medium
                - 0.004_072_046_8 * short,
            green_red: 1.977_998_495_1 * lightness - 2.428_592_205 * medium
                + 0.450_593_709_9 * short,
            blue_yellow: 0.025_904_037_1 * lightness + 0.782_771_766_2 * medium
                - 0.808_675_766 * short,
        }
    }

    fn distance_from(self, other: Self) -> f64 {
        let lightness = self.lightness - other.lightness;
        let green_red = self.green_red - other.green_red;
        let blue_yellow = self.blue_yellow - other.blue_yellow;
        lightness * lightness * 2.0 + green_red * green_red + blue_yellow * blue_yellow
    }
}

struct IndexedPalette {
    colors: [Oklab; 256],
}

impl IndexedPalette {
    fn from_terminal_facts(terminal_facts: &TerminalFacts) -> Self {
        Self {
            colors: std::array::from_fn(|index| {
                let rgb = terminal_facts
                    .probe
                    .and_then(|probe| probe.palette.get(index).copied().flatten())
                    .map(Rgb::from)
                    .unwrap_or_else(|| ansi_rgb(index as u8));
                Oklab::from_rgb(rgb)
            }),
        }
    }

    fn nearest(&self, rgb: Rgb) -> u8 {
        // Match by perceived color rather than channel distance. This is the
        // weighted OKLab comparison OpenCode uses for indexed output.
        let target = Oklab::from_rgb(rgb);
        self.colors
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                left.distance_from(target)
                    .total_cmp(&right.distance_from(target))
            })
            .map(|(index, _)| index as u8)
            .expect("the indexed terminal palette is never empty")
    }
}

fn srgb_to_linear(channel: u8) -> f64 {
    let channel = f64::from(channel) / 255.0;
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

trait Quantized {
    fn quantized(self, palette: &IndexedPalette) -> Self;
}

impl Quantized for Color {
    fn quantized(self, palette: &IndexedPalette) -> Self {
        match self {
            Self::Rgb(red, green, blue) => {
                Self::Indexed(palette.nearest(Rgb::new(red, green, blue)))
            }
            color => color,
        }
    }
}

impl Quantized for Style {
    fn quantized(mut self, palette: &IndexedPalette) -> Self {
        self.fg = self.fg.map(|color| color.quantized(palette));
        self.bg = self.bg.map(|color| color.quantized(palette));
        self
    }
}

macro_rules! quantized_fields {
    ($role:ident { $($field:ident),+ $(,)? }) => {
        impl Quantized for $role {
            fn quantized(self, palette: &IndexedPalette) -> Self {
                let Self { $($field),+ } = self;
                Self {
                    $($field: $field.quantized(palette)),+
                }
            }
        }
    };
}

quantized_fields!(TextRoles { primary, subdued });
quantized_fields!(SurfaceRoles {
    base,
    elevated,
    overlay,
});
quantized_fields!(AccentRoles { primary });
quantized_fields!(AnsiColors {
    black,
    red,
    green,
    yellow,
    blue,
    magenta,
    cyan,
    white,
});
quantized_fields!(AnsiPalette { normal, bright });
quantized_fields!(ActionRoles { primary, disabled });
quantized_fields!(FormFieldRoles {
    text,
    placeholder,
    border,
    invalid,
});
quantized_fields!(FeedbackRoles {
    error,
    warning,
    success,
    info,
});
quantized_fields!(BorderRoles { default, subdued });
quantized_fields!(MarkdownRoles {
    heading,
    emphasis,
    strong,
    link,
    inline_code,
    code_block,
    list_marker,
});
quantized_fields!(SelectionRoles {
    focused,
    unfocused,
    open,
    open_rail,
});
quantized_fields!(Theme {
    text,
    surface,
    accent,
    ansi,
    action,
    form_field,
    feedback,
    border,
    markdown,
    selection,
});

macro_rules! built_in_themes {
    ($($name:literal),+ $(,)?) => {
        pub(crate) const BUILT_IN_THEMES: &[(&str, &str)] = &[
            $(($name, include_str!(concat!("theme/assets/", $name, ".json")))),+
        ];
    };
}

built_in_themes!(
    "aura",
    "ayu",
    "carbonfox",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "catppuccin",
    "cobalt2",
    "cursor",
    "dracula",
    "everforest",
    "flexoki",
    "github",
    "gruvbox",
    "kanagawa",
    "lucent-orng",
    "material",
    "matrix",
    "mercury",
    "monokai",
    "nightowl",
    "nord",
    "one-dark",
    "opencode",
    "orng",
    "osaka-jade",
    "palenight",
    "rosepine",
    "solarized",
    "synthwave84",
    "tokyonight",
    "vercel",
    "vesper",
    "zenburn",
);

pub(crate) fn built_in_themes() -> &'static [(&'static str, &'static str)] {
    BUILT_IN_THEMES
}

static RESOLVED_BUILT_INS: OnceLock<Vec<(&'static str, Result<Theme, ThemeError>)>> =
    OnceLock::new();

impl Theme {
    pub(crate) fn resolve(name: &str, terminal_facts: &TerminalFacts) -> Result<Self, ThemeError> {
        if name == "system" {
            return Ok(Self::system());
        }
        Self::named(name)
            .unwrap_or_else(|| Err(ThemeError(format!("Theme {name:?} was not found"))))
            .map(|theme| {
                if terminal_facts.truecolor {
                    theme
                } else {
                    theme.quantized(&IndexedPalette::from_terminal_facts(terminal_facts))
                }
            })
    }

    pub(crate) fn system() -> Self {
        let accent = AccentRoles {
            primary: Style::default().fg(Color::Cyan),
        };
        // The accent's own colour, carried into a block rather than into text:
        // the open row is the one the accent has always stood for, and a theme
        // that moves the accent moves it. The rail is the same block one
        // column further left, because it stands for the same thing.
        let open = Style::default()
            .fg(Color::Black)
            .bg(accent.primary.fg.unwrap_or(Color::Cyan));
        let feedback = FeedbackRoles {
            error: Style::default().fg(Color::Red),
            warning: Style::default().fg(Color::Yellow),
            success: Style::default().fg(Color::Green),
            info: Style::default().fg(Color::Blue),
        };
        Self {
            text: TextRoles {
                primary: Style::default().fg(Color::Reset),
                subdued: Style::default().fg(Color::DarkGray),
            },
            surface: SurfaceRoles {
                base: Style::default().bg(Color::Reset),
                elevated: Style::default().bg(Color::Black),
                overlay: Style::default().bg(Color::Black),
            },
            accent,
            ansi: AnsiPalette {
                normal: AnsiColors {
                    black: Color::Black,
                    red: feedback.error.fg.unwrap_or(Color::Red),
                    green: feedback.success.fg.unwrap_or(Color::Green),
                    yellow: feedback.warning.fg.unwrap_or(Color::Yellow),
                    blue: feedback.info.fg.unwrap_or(Color::Blue),
                    magenta: Color::Magenta,
                    cyan: accent.primary.fg.unwrap_or(Color::Cyan),
                    white: Color::Gray,
                },
                bright: AnsiColors {
                    black: Color::DarkGray,
                    red: Color::LightRed,
                    green: Color::LightGreen,
                    yellow: Color::LightYellow,
                    blue: Color::LightBlue,
                    magenta: Color::LightMagenta,
                    cyan: Color::LightCyan,
                    white: Color::White,
                },
            },
            action: ActionRoles {
                primary: Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
                disabled: Style::default().fg(Color::DarkGray),
            },
            form_field: FormFieldRoles {
                text: Style::default().fg(Color::Reset),
                placeholder: Style::default().fg(Color::DarkGray),
                border: Style::default().fg(Color::Cyan),
                invalid: Style::default().fg(Color::Red),
            },
            feedback,
            border: BorderRoles {
                default: Style::default().fg(Color::Gray),
                subdued: Style::default().fg(Color::DarkGray),
            },
            markdown: MarkdownRoles {
                heading: Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                emphasis: Style::default().add_modifier(Modifier::ITALIC),
                strong: Style::default().add_modifier(Modifier::BOLD),
                link: Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
                inline_code: Style::default().fg(Color::Yellow),
                code_block: Style::default().fg(Color::Green),
                list_marker: Style::default().fg(Color::Cyan),
            },
            selection: SelectionRoles {
                focused: Style::default().fg(Color::Black).bg(Color::Blue),
                unfocused: Style::default().fg(Color::Reset).bg(Color::DarkGray),
                open,
                open_rail: open,
            },
        }
    }

    pub(crate) fn from_document(source: &str) -> Result<Self, ThemeError> {
        let document: ThemeDocument = serde_json::from_str(source)
            .map_err(|error| ThemeError(format!("Theme document is not valid JSON: {error}")))?;
        if let Some(version) = document.version
            && version != 1
        {
            return Err(ThemeError(format!(
                "Theme version {version} is not supported"
            )));
        }
        let mut colors = DocumentResolver::new(&document);
        let primary = colors.required("primary")?;
        let secondary = colors.required("secondary")?;
        let accent = colors.required("accent")?;
        let error = colors.required("error")?;
        let warning = colors.required("warning")?;
        let success = colors.required("success")?;
        let info = colors.required("info")?;
        let text = colors.required("text")?;
        let muted = colors.required("textMuted")?;
        let background = colors.required("background")?;
        let panel = colors.required("backgroundPanel")?;
        let element = colors.required("backgroundElement")?;
        let menu = colors.optional("backgroundMenu", "backgroundElement")?;
        let border = colors.required("border")?;
        let border_active = colors.required("borderActive")?;
        let border_subtle = colors.required("borderSubtle")?;
        let selected_text = colors.optional("selectedListItemText", "background")?;
        let heading = colors.required("markdownHeading")?;
        let link = colors.required("markdownLink")?;
        let markdown_code = colors.required("markdownCode")?;
        let emphasis = colors.required("markdownEmph")?;
        let strong = colors.required("markdownStrong")?;
        let list_marker = colors.required("markdownListItem")?;
        let code_block = colors.required("markdownCodeBlock")?;
        let style = |color| Style::default().fg(color);
        let surface = |color| Style::default().bg(color);
        let feedback = FeedbackRoles {
            error: style(error),
            warning: style(warning),
            success: style(success),
            info: style(info),
        };
        let normal = AnsiColors {
            black: panel,
            red: error,
            green: success,
            yellow: warning,
            blue: primary,
            magenta: secondary,
            cyan: info,
            white: text,
        };
        Ok(Self {
            text: TextRoles {
                primary: style(text),
                subdued: style(muted),
            },
            surface: SurfaceRoles {
                base: surface(background),
                elevated: surface(panel),
                overlay: surface(menu),
            },
            accent: AccentRoles {
                primary: style(primary),
            },
            ansi: AnsiPalette {
                normal,
                bright: normal,
            },
            action: ActionRoles {
                primary: style(primary).add_modifier(Modifier::BOLD),
                disabled: style(muted),
            },
            form_field: FormFieldRoles {
                text: style(text),
                placeholder: style(muted),
                border: style(border_active),
                invalid: style(error),
            },
            feedback,
            border: BorderRoles {
                default: style(border),
                subdued: style(border_subtle),
            },
            markdown: MarkdownRoles {
                heading: style(heading).add_modifier(Modifier::BOLD),
                emphasis: style(emphasis).add_modifier(Modifier::ITALIC),
                strong: style(strong).add_modifier(Modifier::BOLD),
                link: style(link).add_modifier(Modifier::UNDERLINED),
                inline_code: style(markdown_code),
                code_block: style(code_block),
                list_marker: style(list_marker),
            },
            selection: SelectionRoles {
                focused: style(selected_text).bg(primary),
                unfocused: style(text).bg(element),
                open: style(selected_text).bg(accent),
                open_rail: surface(accent),
            },
        })
    }

    pub(crate) fn named(name: &str) -> Option<Result<Self, ThemeError>> {
        RESOLVED_BUILT_INS
            .get_or_init(|| {
                built_in_themes()
                    .iter()
                    .map(|(name, source)| (*name, Self::from_document(source)))
                    .collect()
            })
            .iter()
            .find(|(built_in, _)| *built_in == name)
            .map(|(_, theme)| theme.clone())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::*;

    fn document(defs: Value, changes: &[(&str, Value)]) -> String {
        let mut theme = Map::from_iter([
            ("primary".to_owned(), json!("#010101")),
            ("secondary".to_owned(), json!("#020202")),
            ("accent".to_owned(), json!("#030303")),
            ("error".to_owned(), json!("#040404")),
            ("warning".to_owned(), json!("#050505")),
            ("success".to_owned(), json!("#060606")),
            ("info".to_owned(), json!("#070707")),
            ("text".to_owned(), json!("#080808")),
            ("textMuted".to_owned(), json!("#090909")),
            ("background".to_owned(), json!("#101010")),
            ("backgroundPanel".to_owned(), json!("#111111")),
            ("backgroundElement".to_owned(), json!("#121212")),
            ("border".to_owned(), json!("#131313")),
            ("borderActive".to_owned(), json!("#141414")),
            ("borderSubtle".to_owned(), json!("#151515")),
            ("markdownHeading".to_owned(), json!("#161616")),
            ("markdownLink".to_owned(), json!("#171717")),
            ("markdownCode".to_owned(), json!("#181818")),
            ("markdownEmph".to_owned(), json!("#191919")),
            ("markdownStrong".to_owned(), json!("#202020")),
            ("markdownListItem".to_owned(), json!("#212121")),
            ("markdownCodeBlock".to_owned(), json!("#222222")),
        ]);
        for (key, value) in changes {
            theme.insert((*key).to_owned(), value.clone());
        }
        json!({ "defs": defs, "theme": theme }).to_string()
    }

    #[test]
    fn defs_and_theme_references_resolve_into_roles() {
        let source = document(
            json!({ "brand": "#123456" }),
            &[("primary", json!("brand")), ("secondary", json!("primary"))],
        );
        let theme = Theme::from_document(&source).expect("resolve references");

        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(theme.ansi.normal.magenta, Color::Rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn circular_references_are_rejected() {
        let source = document(
            json!({ "first": "second", "second": "first" }),
            &[("primary", json!("first"))],
        );
        let error = Theme::from_document(&source).expect_err("reject a reference cycle");
        assert!(error.to_string().contains("first -> second -> first"));
    }

    #[test]
    fn unknown_references_are_rejected() {
        let source = document(json!({}), &[("primary", json!("missing"))]);
        let error = Theme::from_document(&source).expect_err("reject an unknown reference");
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn dark_is_selected_from_a_dark_light_pair() {
        let source = document(
            json!({}),
            &[("primary", json!({ "dark": "#102030", "light": "#f0e0d0" }))],
        );
        let theme = Theme::from_document(&source).expect("resolve the dark variant");
        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(0x10, 0x20, 0x30)));
    }

    #[test]
    fn ansi_indices_resolve_to_the_xterm_palette() {
        let source = document(json!({}), &[("primary", json!(196))]);
        let theme = Theme::from_document(&source).expect("resolve an ANSI index");
        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn transparent_background_resets_the_terminal_background() {
        let source = document(json!({}), &[("background", json!("transparent"))]);
        let theme = Theme::from_document(&source).expect("resolve transparency");
        assert_eq!(theme.surface.base.bg, Some(Color::Reset));
    }

    #[test]
    fn optional_selection_text_and_menu_background_take_opencode_defaults() {
        let source = document(json!({}), &[]);
        let theme = Theme::from_document(&source).expect("resolve optional defaults");
        assert_eq!(
            theme.selection.focused.fg,
            Some(Color::Rgb(0x10, 0x10, 0x10))
        );
        assert_eq!(theme.surface.overlay.bg, Some(Color::Rgb(0x12, 0x12, 0x12)));
    }

    #[test]
    fn version_two_documents_are_rejected() {
        let mut value: Value = serde_json::from_str(&document(json!({}), &[])).unwrap();
        value["version"] = json!(2);
        let error = Theme::from_document(&value.to_string()).expect_err("reject v2");
        assert!(error.to_string().contains("version 2"));
    }

    #[test]
    fn every_vendored_theme_resolves() {
        let themes = built_in_themes();
        assert_eq!(themes.len(), 33);
        for (name, source) in themes {
            Theme::from_document(source)
                .unwrap_or_else(|error| panic!("built-in Theme {name:?} failed: {error}"));
        }
    }
}
