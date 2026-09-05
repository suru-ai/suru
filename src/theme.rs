//! Semantic terminal styles used by built-in renderers.

use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{
    protocol::AppearanceMode,
    terminal::{TerminalColor, TerminalFacts},
};

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
    pub(crate) syntax: SyntaxRoles,
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

/// The colors a Code Block paints its tokens with. A theme document names
/// them with the `syntax*` keys; a role the document leaves out takes the
/// code block color so the block reads as it did before highlighting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyntaxRoles {
    pub(crate) comment: Style,
    pub(crate) keyword: Style,
    pub(crate) function: Style,
    pub(crate) variable: Style,
    pub(crate) string: Style,
    pub(crate) number: Style,
    pub(crate) r#type: Style,
    pub(crate) operator: Style,
    pub(crate) punctuation: Style,
}

/// The document keys a Theme document spells [`SyntaxRoles`] with.
#[cfg(test)]
const SYNTAX_KEYS: [&str; 9] = [
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
];

impl SyntaxRoles {
    /// The System theme's roles, drawn from the terminal's own colors: the
    /// accent and feedback styles carry keywords, operators, variables,
    /// strings, numbers, and types so a probed palette flows through, while
    /// functions take the terminal's blue.
    fn from_terminal_roles(
        text: TextRoles,
        accent: AccentRoles,
        feedback: FeedbackRoles,
        blue: Color,
    ) -> Self {
        Self {
            comment: text.subdued,
            keyword: accent.primary,
            function: Style::default().fg(blue),
            variable: feedback.error,
            string: feedback.success,
            number: feedback.warning,
            r#type: feedback.warning,
            operator: accent.primary,
            punctuation: text.primary,
        }
    }
}

/// How a surface says which of its rows a reader means. The two questions are
/// separate wherever a list stands beside the thing it lists: which row the
/// keys would act on, and which row is the one already on show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectionRoles {
    /// The row the keys are on, drawn only while the surface holding it has
    /// them. It is a block in the accent colour with its own text foreground,
    /// so it stands out from the surface behind it even where that surface is
    /// already the element gray a menu is painted on. A Sidebar Rail painted
    /// over this block falls back to this foreground where its feedback
    /// colour would vanish into the block.
    pub(crate) focused: Style,
    /// The row a surface that has given the keys up would come back to, where
    /// that surface keeps one. The Sidebar keeps none: its row focus goes with
    /// the keys.
    pub(crate) unfocused: Style,
    /// The Title standing for what the main view is already showing, which is
    /// true whoever holds the keys. It is a foreground-only role so row focus
    /// can keep its background while the open Title remains distinct.
    pub(crate) open_title: Style,
}

#[derive(Clone, Debug)]
pub(crate) struct ThemeError(String);

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ThemeError {}

impl ThemeError {
    fn user_file_message(&self) -> String {
        if self.0.starts_with("Theme document is not valid JSON:")
            || self.0.starts_with("Theme version ")
        {
            format!("ignored because {self}")
        } else {
            format!("ignored because its colors could not be resolved: {self}")
        }
    }
}

#[derive(Deserialize)]
struct ThemeDocument {
    version: Option<u64>,
    #[serde(default)]
    defs: Map<String, Value>,
    theme: Map<String, Value>,
}

#[derive(Clone, Copy)]
enum ThemeVariant {
    Dark,
    Light,
}

impl ThemeVariant {
    fn for_mode(mode: AppearanceMode, terminal_facts: &TerminalFacts) -> Self {
        match mode {
            AppearanceMode::Dark => Self::Dark,
            AppearanceMode::Light => Self::Light,
            AppearanceMode::System => terminal_background(terminal_facts)
                .filter(|background| terminal_luminance(*background) > 127.5)
                .map_or(Self::Dark, |_| Self::Light),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}

struct DocumentResolver<'a> {
    defs: &'a Map<String, Value>,
    theme: &'a Map<String, Value>,
    variant: ThemeVariant,
    resolved: HashMap<String, Color>,
}

impl<'a> DocumentResolver<'a> {
    fn new(document: &'a ThemeDocument, variant: ThemeVariant) -> Self {
        Self {
            defs: &document.defs,
            theme: &document.theme,
            variant,
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
                .get(self.variant.key())
                .ok_or_else(|| {
                    ThemeError(format!("color pair has no {} value", self.variant.key()))
                })
                .and_then(|variant| self.value(variant, chain)),
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
quantized_fields!(SyntaxRoles {
    comment,
    keyword,
    function,
    variable,
    string,
    number,
    r#type,
    operator,
    punctuation,
});
quantized_fields!(SelectionRoles {
    focused,
    unfocused,
    open_title,
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
    syntax,
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

struct ResolvedThemeVariants {
    dark: Result<Theme, ThemeError>,
    light: Result<Theme, ThemeError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemeChoice {
    pub(crate) name: String,
    pub(crate) source: ThemeSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThemeSource {
    BuiltIn,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemeDiagnostic {
    pub(crate) file: PathBuf,
    pub(crate) message: String,
}

#[derive(Default)]
pub(crate) struct ThemeCatalog {
    user_themes: Vec<UserTheme>,
}

struct UserTheme {
    name: String,
    variants: ResolvedThemeVariants,
}

impl ThemeCatalog {
    pub(crate) fn scan(config_root: Option<&Path>) -> (Self, Vec<ThemeDiagnostic>) {
        let Some(directory) = config_root.map(|root| root.join("themes")) else {
            return (Self::default(), Vec::new());
        };
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), Vec::new());
            }
            Err(error) => {
                tracing::warn!(path = %directory.display(), %error, "could not read user Themes");
                return (Self::default(), Vec::new());
            }
        };
        let mut paths = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry.path()),
                Err(error) => {
                    tracing::warn!(path = %directory.display(), %error, "could not read a user Theme directory entry");
                    None
                }
            })
            .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
            .collect::<Vec<_>>();
        paths.sort();

        let mut catalog = Self::default();
        let mut diagnostics = Vec::new();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                diagnostics.push(ThemeDiagnostic {
                    file: path,
                    message: "ignored because its basename is not valid Unicode".to_owned(),
                });
                continue;
            };
            let name = name.to_owned();
            if name == "system" {
                diagnostics.push(ThemeDiagnostic {
                    file: path,
                    message: "ignored because System is reserved for terminal-derived colors"
                        .to_owned(),
                });
                continue;
            }
            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    diagnostics.push(ThemeDiagnostic {
                        file: path,
                        message: format!("ignored because it could not be read: {error}"),
                    });
                    continue;
                }
            };
            match ResolvedThemeVariants::try_from_document(&source) {
                Ok(variants) => catalog.user_themes.push(UserTheme { name, variants }),
                Err(error) => diagnostics.push(ThemeDiagnostic {
                    file: path,
                    message: error.user_file_message(),
                }),
            }
        }
        (catalog, diagnostics)
    }

    pub(crate) fn choices(&self) -> Vec<ThemeChoice> {
        let mut choices = built_in_themes()
            .iter()
            .filter(|(name, _)| !self.user_themes.iter().any(|user| user.name == *name))
            .map(|(name, _)| ThemeChoice {
                name: (*name).to_owned(),
                source: ThemeSource::BuiltIn,
            })
            .chain(self.user_themes.iter().map(|theme| ThemeChoice {
                name: theme.name.clone(),
                source: ThemeSource::User,
            }))
            .collect::<Vec<_>>();
        choices.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        choices.insert(
            0,
            ThemeChoice {
                name: "system".to_owned(),
                source: ThemeSource::BuiltIn,
            },
        );
        choices
    }

    pub(crate) fn resolve(
        &self,
        name: &str,
        mode: AppearanceMode,
        terminal_facts: &TerminalFacts,
    ) -> Result<Theme, ThemeError> {
        let variant = ThemeVariant::for_mode(mode, terminal_facts);
        if let Some(theme) = self.user_themes.iter().find(|theme| theme.name == name) {
            return theme
                .variants
                .get(variant)
                .map(|theme| theme.for_terminal_capabilities(terminal_facts));
        }
        Theme::resolve(name, mode, terminal_facts)
    }
}

impl ResolvedThemeVariants {
    fn from_document(source: &str) -> Self {
        Self {
            dark: Theme::from_document(source, ThemeVariant::Dark),
            light: Theme::from_document(source, ThemeVariant::Light),
        }
    }

    fn get(&self, variant: ThemeVariant) -> Result<Theme, ThemeError> {
        match variant {
            ThemeVariant::Dark => self.dark.clone(),
            ThemeVariant::Light => self.light.clone(),
        }
    }

    fn try_from_document(source: &str) -> Result<Self, ThemeError> {
        let themes = Self::from_document(source);
        themes.dark.clone()?;
        themes.light.clone()?;
        Ok(themes)
    }
}

static RESOLVED_BUILT_INS: OnceLock<Vec<(&'static str, ResolvedThemeVariants)>> = OnceLock::new();

impl Theme {
    pub(crate) fn resolve(
        name: &str,
        mode: AppearanceMode,
        terminal_facts: &TerminalFacts,
    ) -> Result<Self, ThemeError> {
        let variant = ThemeVariant::for_mode(mode, terminal_facts);
        if name == "system" {
            let Some(probe) = terminal_facts.probe else {
                return Ok(Self::system());
            };
            if probe.background.is_none() && probe.palette[0].is_none() {
                return Ok(Self::system());
            }
            let theme = Self::from_terminal_probe(probe, variant);
            return Ok(theme.for_terminal_capabilities(terminal_facts));
        }
        Self::named(name, variant)
            .unwrap_or_else(|| Err(ThemeError(format!("Theme {name:?} was not found"))))
            .map(|theme| theme.for_terminal_capabilities(terminal_facts))
    }

    fn for_terminal_capabilities(self, terminal_facts: &TerminalFacts) -> Self {
        if terminal_facts.truecolor {
            self
        } else {
            self.quantized(&IndexedPalette::from_terminal_facts(terminal_facts))
        }
    }

    pub(crate) fn system() -> Self {
        let accent = AccentRoles {
            primary: Style::default().fg(Color::Cyan),
        };
        let feedback = FeedbackRoles {
            error: Style::default().fg(Color::Red),
            warning: Style::default().fg(Color::Yellow),
            success: Style::default().fg(Color::Green),
            info: Style::default().fg(Color::LightBlue),
        };
        let text = TextRoles {
            primary: Style::default().fg(Color::Reset),
            subdued: Style::default().fg(Color::DarkGray),
        };
        Self {
            text,
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
                    blue: Color::Blue,
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
            syntax: SyntaxRoles::from_terminal_roles(text, accent, feedback, Color::Blue),
            selection: SelectionRoles {
                focused: Style::default().fg(Color::Black).bg(Color::Blue),
                unfocused: Style::default().fg(Color::Reset).bg(Color::DarkGray),
                open_title: accent.primary,
            },
        }
    }

    fn from_terminal_probe(
        probe: crate::terminal::TerminalColorProbe,
        variant: ThemeVariant,
    ) -> Self {
        let fallback = Self::system();
        let reported = |index: usize, fallback: Color| {
            probe.palette[index]
                .map(|color| Color::Rgb(color.red, color.green, color.blue))
                .unwrap_or(fallback)
        };
        let background_rgb = probe
            .background
            .or(probe.palette[0])
            .unwrap_or(TerminalColor::new(0, 0, 0));
        let foreground = probe
            .foreground
            .map(|color| Color::Rgb(color.red, color.green, color.blue))
            .or_else(|| {
                probe.palette[7].map(|color| Color::Rgb(color.red, color.green, color.blue))
            })
            .unwrap_or(fallback.ansi.normal.white);
        let background = Color::Rgb(
            background_rgb.red,
            background_rgb.green,
            background_rgb.blue,
        );
        let luminance = terminal_luminance(background_rgb);
        let dark = variant.is_dark();
        let grays = terminal_grays(background_rgb, luminance, dark);
        let muted = terminal_muted(luminance, dark);
        let primary = reported(6, fallback.accent.primary.fg.unwrap_or(Color::Cyan));
        let red = reported(1, fallback.ansi.normal.red);
        let green = reported(2, fallback.ansi.normal.green);
        let yellow = reported(3, fallback.ansi.normal.yellow);
        let blue = reported(4, fallback.ansi.normal.blue);
        let magenta = reported(5, fallback.ansi.normal.magenta);
        let style = |color| Style::default().fg(color);
        let surface = |color| Style::default().bg(color);
        let ansi = AnsiPalette {
            normal: AnsiColors {
                black: reported(0, fallback.ansi.normal.black),
                red,
                green,
                yellow,
                blue,
                magenta,
                cyan: primary,
                white: reported(7, fallback.ansi.normal.white),
            },
            bright: AnsiColors {
                black: reported(8, fallback.ansi.bright.black),
                red: reported(9, fallback.ansi.bright.red),
                green: reported(10, fallback.ansi.bright.green),
                yellow: reported(11, fallback.ansi.bright.yellow),
                blue: reported(12, fallback.ansi.bright.blue),
                magenta: reported(13, fallback.ansi.bright.magenta),
                cyan: reported(14, fallback.ansi.bright.cyan),
                white: reported(15, fallback.ansi.bright.white),
            },
        };
        let feedback = FeedbackRoles {
            error: style(red),
            warning: style(yellow),
            success: style(green),
            info: style(primary),
        };
        let text = TextRoles {
            primary: style(foreground),
            subdued: style(muted),
        };
        let accent = AccentRoles {
            primary: style(primary),
        };
        Self {
            text,
            surface: SurfaceRoles {
                base: surface(Color::Reset),
                elevated: surface(grays.panel()),
                overlay: surface(grays.element()),
            },
            accent,
            ansi,
            action: ActionRoles {
                primary: style(primary).add_modifier(Modifier::BOLD),
                disabled: style(muted),
            },
            form_field: FormFieldRoles {
                text: style(foreground),
                placeholder: style(muted),
                border: style(grays.border_active()),
                invalid: style(red),
            },
            feedback,
            border: BorderRoles {
                default: style(grays.border()),
                subdued: style(grays.border_subtle()),
            },
            markdown: MarkdownRoles {
                heading: style(foreground).add_modifier(Modifier::BOLD),
                emphasis: style(yellow).add_modifier(Modifier::ITALIC),
                strong: style(foreground).add_modifier(Modifier::BOLD),
                link: style(blue).add_modifier(Modifier::UNDERLINED),
                inline_code: style(green),
                code_block: style(foreground),
                list_marker: style(blue),
            },
            syntax: SyntaxRoles::from_terminal_roles(text, accent, feedback, blue),
            selection: SelectionRoles {
                focused: style(background).bg(primary),
                unfocused: style(foreground).bg(grays.element()),
                open_title: style(primary),
            },
        }
    }

    fn from_document(source: &str, variant: ThemeVariant) -> Result<Self, ThemeError> {
        let document: ThemeDocument = serde_json::from_str(source)
            .map_err(|error| ThemeError(format!("Theme document is not valid JSON: {error}")))?;
        if let Some(version) = document.version
            && version != 1
        {
            return Err(ThemeError(format!(
                "Theme version {version} is not supported"
            )));
        }
        let mut colors = DocumentResolver::new(&document, variant);
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
        let mut syntax_role = |key| colors.optional(key, "markdownCodeBlock");
        let syntax = SyntaxRoles {
            comment: Style::default().fg(syntax_role("syntaxComment")?),
            keyword: Style::default().fg(syntax_role("syntaxKeyword")?),
            function: Style::default().fg(syntax_role("syntaxFunction")?),
            variable: Style::default().fg(syntax_role("syntaxVariable")?),
            string: Style::default().fg(syntax_role("syntaxString")?),
            number: Style::default().fg(syntax_role("syntaxNumber")?),
            r#type: Style::default().fg(syntax_role("syntaxType")?),
            operator: Style::default().fg(syntax_role("syntaxOperator")?),
            punctuation: Style::default().fg(syntax_role("syntaxPunctuation")?),
        };
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
            syntax,
            selection: SelectionRoles {
                focused: style(selected_text).bg(primary),
                unfocused: style(text).bg(element),
                open_title: style(accent),
            },
        })
    }

    fn named(name: &str, variant: ThemeVariant) -> Option<Result<Self, ThemeError>> {
        RESOLVED_BUILT_INS
            .get_or_init(|| {
                built_in_themes()
                    .iter()
                    .map(|(name, source)| (*name, ResolvedThemeVariants::from_document(source)))
                    .collect()
            })
            .iter()
            .find(|(built_in, _)| *built_in == name)
            .map(|(_, themes)| themes.get(variant))
    }
}

fn terminal_background(terminal_facts: &TerminalFacts) -> Option<TerminalColor> {
    terminal_facts
        .probe
        .and_then(|probe| probe.background.or(probe.palette[0]))
}

fn terminal_luminance(color: TerminalColor) -> f64 {
    0.299 * f64::from(color.red) + 0.587 * f64::from(color.green) + 0.114 * f64::from(color.blue)
}

#[derive(Clone, Copy)]
struct TerminalGrayRamp([Color; 12]);

impl TerminalGrayRamp {
    fn panel(self) -> Color {
        self.0[1]
    }

    fn element(self) -> Color {
        self.0[2]
    }

    fn border_subtle(self) -> Color {
        self.0[5]
    }

    fn border(self) -> Color {
        self.0[6]
    }

    fn border_active(self) -> Color {
        self.0[7]
    }
}

fn terminal_grays(background: TerminalColor, luminance: f64, dark: bool) -> TerminalGrayRamp {
    TerminalGrayRamp(std::array::from_fn(|index| {
        let factor = (index + 1) as f64 / 12.0;
        let gray = if dark && luminance < 10.0 {
            (factor * 0.4 * 255.0).floor() as u8
        } else if !dark && luminance > 245.0 {
            (255.0 - factor * 0.4 * 255.0).floor() as u8
        } else {
            let next = if dark {
                luminance + (255.0 - luminance) * factor * 0.4
            } else {
                luminance * (1.0 - factor * 0.4)
            };
            let ratio = next / luminance;
            let channel =
                |channel: u8| (f64::from(channel) * ratio).clamp(0.0, 255.0).floor() as u8;
            return Color::Rgb(
                channel(background.red),
                channel(background.green),
                channel(background.blue),
            );
        };
        Color::Rgb(gray, gray, gray)
    }))
}

fn terminal_muted(luminance: f64, dark: bool) -> Color {
    let gray = if dark {
        if luminance < 10.0 {
            180
        } else {
            (160.0 + luminance * 0.3).floor().min(200.0) as u8
        }
    } else if luminance > 245.0 {
        75
    } else {
        (100.0 - (255.0 - luminance) * 0.2).floor().max(60.0) as u8
    };
    Color::Rgb(gray, gray, gray)
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
        let theme = Theme::from_document(&source, ThemeVariant::Dark).expect("resolve references");

        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(theme.ansi.normal.magenta, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(
            theme.selection.open_title.fg,
            Some(Color::Rgb(0x03, 0x03, 0x03)),
            "the document's accent key supplies the open Title"
        );
    }

    #[test]
    fn circular_references_are_rejected() {
        let source = document(
            json!({ "first": "second", "second": "first" }),
            &[("primary", json!("first"))],
        );
        let error = Theme::from_document(&source, ThemeVariant::Dark)
            .expect_err("reject a reference cycle");
        assert!(error.to_string().contains("first -> second -> first"));
    }

    #[test]
    fn unknown_references_are_rejected() {
        let source = document(json!({}), &[("primary", json!("missing"))]);
        let error = Theme::from_document(&source, ThemeVariant::Dark)
            .expect_err("reject an unknown reference");
        assert!(error.to_string().contains("missing"));
    }

    #[test]
    fn dark_is_selected_from_a_dark_light_pair() {
        let source = document(
            json!({}),
            &[("primary", json!({ "dark": "#102030", "light": "#f0e0d0" }))],
        );
        let theme =
            Theme::from_document(&source, ThemeVariant::Dark).expect("resolve the dark variant");
        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(0x10, 0x20, 0x30)));
    }

    #[test]
    fn ansi_indices_resolve_to_the_xterm_palette() {
        let source = document(json!({}), &[("primary", json!(196))]);
        let theme =
            Theme::from_document(&source, ThemeVariant::Dark).expect("resolve an ANSI index");
        assert_eq!(theme.accent.primary.fg, Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn transparent_background_resets_the_terminal_background() {
        let source = document(json!({}), &[("background", json!("transparent"))]);
        let theme =
            Theme::from_document(&source, ThemeVariant::Dark).expect("resolve transparency");
        assert_eq!(theme.surface.base.bg, Some(Color::Reset));
    }

    #[test]
    fn optional_selection_text_and_menu_background_take_opencode_defaults() {
        let source = document(json!({}), &[]);
        let theme =
            Theme::from_document(&source, ThemeVariant::Dark).expect("resolve optional defaults");
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
        let error =
            Theme::from_document(&value.to_string(), ThemeVariant::Dark).expect_err("reject v2");
        assert!(error.to_string().contains("version 2"));
    }

    #[test]
    fn every_vendored_theme_resolves() {
        let themes = built_in_themes();
        assert_eq!(themes.len(), 33);
        for (name, source) in themes {
            for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
                Theme::from_document(source, variant).unwrap_or_else(|error| {
                    panic!(
                        "built-in Theme {name:?} {} variant failed: {error}",
                        variant.key()
                    )
                });
            }
        }
    }

    #[test]
    fn every_built_in_keeps_the_focused_row_legible_over_its_block() {
        let mut themes = vec![("system", Theme::system())];
        for (name, source) in built_in_themes() {
            for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
                themes.push((
                    *name,
                    Theme::from_document(source, variant).unwrap_or_else(|error| {
                        panic!(
                            "built-in Theme {name:?} {} variant failed: {error}",
                            variant.key()
                        )
                    }),
                ));
            }
        }

        for (name, theme) in themes {
            assert!(
                theme.selection.open_title.fg.is_some(),
                "built-in Theme {name:?} resolves its open Title role"
            );
            let focus = theme
                .selection
                .focused
                .bg
                .unwrap_or_else(|| panic!("built-in Theme {name:?} has no focus background"));
            let text = theme
                .selection
                .focused
                .fg
                .unwrap_or_else(|| panic!("built-in Theme {name:?} has no focus foreground"));
            assert_ne!(
                text, focus,
                "built-in Theme {name:?} paints the focused row's text in its own block colour"
            );
            assert_ne!(
                Some(focus),
                theme.surface.overlay.bg,
                "built-in Theme {name:?} paints the focused row in the menu's own background"
            );
            assert_ne!(
                Some(focus),
                theme.surface.elevated.bg,
                "built-in Theme {name:?} paints the focused row in the Sidebar's own background"
            );
        }
    }

    fn syntax_roles(theme: &Theme) -> [(&'static str, Style); 9] {
        [
            ("comment", theme.syntax.comment),
            ("keyword", theme.syntax.keyword),
            ("function", theme.syntax.function),
            ("variable", theme.syntax.variable),
            ("string", theme.syntax.string),
            ("number", theme.syntax.number),
            ("type", theme.syntax.r#type),
            ("operator", theme.syntax.operator),
            ("punctuation", theme.syntax.punctuation),
        ]
    }

    #[test]
    fn syntax_keys_resolve_into_syntax_roles() {
        let source = document(
            json!({ "ink": "#abcdef" }),
            &[
                ("syntaxComment", json!("#a1a1a1")),
                ("syntaxKeyword", json!("ink")),
                ("syntaxFunction", json!("#a3a3a3")),
                ("syntaxVariable", json!("#a4a4a4")),
                ("syntaxString", json!("#a5a5a5")),
                ("syntaxNumber", json!("#a6a6a6")),
                ("syntaxType", json!("#a7a7a7")),
                ("syntaxOperator", json!("#a8a8a8")),
                ("syntaxPunctuation", json!("#a9a9a9")),
            ],
        );
        let theme = Theme::from_document(&source, ThemeVariant::Dark).expect("resolve syntax keys");

        assert_eq!(theme.syntax.comment.fg, Some(Color::Rgb(0xa1, 0xa1, 0xa1)));
        assert_eq!(theme.syntax.keyword.fg, Some(Color::Rgb(0xab, 0xcd, 0xef)));
        assert_eq!(theme.syntax.function.fg, Some(Color::Rgb(0xa3, 0xa3, 0xa3)));
        assert_eq!(theme.syntax.variable.fg, Some(Color::Rgb(0xa4, 0xa4, 0xa4)));
        assert_eq!(theme.syntax.string.fg, Some(Color::Rgb(0xa5, 0xa5, 0xa5)));
        assert_eq!(theme.syntax.number.fg, Some(Color::Rgb(0xa6, 0xa6, 0xa6)));
        assert_eq!(theme.syntax.r#type.fg, Some(Color::Rgb(0xa7, 0xa7, 0xa7)));
        assert_eq!(theme.syntax.operator.fg, Some(Color::Rgb(0xa8, 0xa8, 0xa8)));
        assert_eq!(
            theme.syntax.punctuation.fg,
            Some(Color::Rgb(0xa9, 0xa9, 0xa9))
        );
    }

    #[test]
    fn syntax_roles_missing_from_a_document_fall_back_to_the_code_block_style() {
        let source = document(json!({}), &[("syntaxKeyword", json!("#a2a2a2"))]);
        let theme = Theme::from_document(&source, ThemeVariant::Dark).expect("resolve fallbacks");

        assert_eq!(theme.syntax.keyword.fg, Some(Color::Rgb(0xa2, 0xa2, 0xa2)));
        for (role, style) in syntax_roles(&theme) {
            if role == "keyword" {
                continue;
            }
            assert_eq!(
                style, theme.markdown.code_block,
                "the absent {role} role falls back to the code block style"
            );
        }
    }

    #[test]
    fn every_vendored_theme_carries_every_syntax_role() {
        for (name, source) in built_in_themes() {
            let document: ThemeDocument = serde_json::from_str(source).unwrap();
            for key in SYNTAX_KEYS {
                assert!(
                    document.theme.contains_key(key),
                    "built-in Theme {name:?} carries {key:?}"
                );
            }
            for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
                let theme = Theme::from_document(source, variant).unwrap();
                for (role, style) in syntax_roles(&theme) {
                    assert!(
                        style.fg.is_some(),
                        "built-in Theme {name:?} {} variant resolves the {role} role",
                        variant.key()
                    );
                }
            }
        }
    }

    #[test]
    fn system_theme_paints_syntax_roles_with_terminal_colors() {
        let theme = Theme::system();
        assert_eq!(theme.syntax.comment.fg, Some(Color::DarkGray));
        assert_eq!(theme.syntax.keyword, theme.accent.primary);
        assert_eq!(theme.syntax.function.fg, Some(Color::Blue));
        assert_eq!(theme.syntax.variable, theme.feedback.error);
        assert_eq!(theme.syntax.string, theme.feedback.success);
        assert_eq!(theme.syntax.number, theme.feedback.warning);
        assert_eq!(theme.syntax.r#type, theme.feedback.warning);
        assert_eq!(theme.syntax.operator.fg, Some(Color::Cyan));
        assert_eq!(theme.syntax.punctuation, theme.text.primary);
        assert_eq!(theme.syntax.punctuation.fg, Some(Color::Reset));
    }

    #[test]
    fn probed_palette_flows_through_the_system_syntax_roles() {
        let mut palette = [None; 16];
        palette[0] = Some(TerminalColor::new(1, 2, 3));
        palette[1] = Some(TerminalColor::new(200, 10, 10));
        palette[2] = Some(TerminalColor::new(10, 200, 10));
        palette[3] = Some(TerminalColor::new(200, 200, 10));
        palette[4] = Some(TerminalColor::new(10, 10, 200));
        palette[6] = Some(TerminalColor::new(10, 200, 200));
        let probe = crate::terminal::TerminalColorProbe::new(
            palette,
            Some(TerminalColor::new(230, 230, 230)),
            Some(TerminalColor::new(1, 2, 3)),
        );
        let theme = Theme::from_terminal_probe(probe, ThemeVariant::Dark);

        assert_eq!(theme.syntax.comment, theme.text.subdued);
        assert_eq!(theme.syntax.keyword, theme.accent.primary);
        assert_eq!(theme.syntax.keyword.fg, Some(Color::Rgb(10, 200, 200)));
        assert_eq!(theme.syntax.function.fg, Some(Color::Rgb(10, 10, 200)));
        assert_eq!(theme.syntax.variable, theme.feedback.error);
        assert_eq!(theme.syntax.string, theme.feedback.success);
        assert_eq!(theme.syntax.number, theme.feedback.warning);
        assert_eq!(theme.syntax.r#type, theme.feedback.warning);
        assert_eq!(theme.syntax.operator, theme.accent.primary);
        assert_eq!(theme.syntax.punctuation, theme.text.primary);
        assert_eq!(theme.syntax.punctuation.fg, Some(Color::Rgb(230, 230, 230)));
    }

    #[test]
    fn quantization_to_indexed_color_covers_the_syntax_roles() {
        let source = document(json!({}), &[("syntaxKeyword", json!("#ff0000"))]);
        let theme = Theme::from_document(&source, ThemeVariant::Dark)
            .expect("resolve syntax keys")
            .for_terminal_capabilities(&TerminalFacts::unprobed(false));

        assert_eq!(
            theme.syntax.keyword.fg,
            Some(Color::Indexed(9)),
            "pure red lands on the terminal's own bright red"
        );
        for (role, style) in syntax_roles(&theme) {
            assert!(
                matches!(style.fg, Some(Color::Indexed(_))),
                "the {role} role is quantized to an indexed color, got {:?}",
                style.fg
            );
        }
    }
}
