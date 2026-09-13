//! Settings and the Config Documents that pin them.
//!
//! The schema is compile-time: every Setting declares its dotted camelCase
//! key, its scope, and how a pinned JSON value becomes a typed field of
//! [`EffectiveSettings`]. The loader collapses an ordered stack of Config
//! Documents (depth one today; project-level documents later are additive)
//! into one [`SettingsSnapshot`] and never lets a configuration problem stop
//! the server: a file that fails to parse is ignored whole, an unknown or
//! mistyped key is ignored alone, and every ignore becomes a diagnostic
//! naming the file, the key path, and the reason.
//!
//! The server is also the only writer. A typed mutation becomes a
//! format-preserving CST edit of the winning Config Document, so a user's key
//! order, spacing, and comments survive an edit Suru makes; the file it leaves
//! behind is then reloaded, which keeps the file — not an in-memory shadow of
//! it — the source of truth for what every client is told.

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt, fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstRootNode},
};
use serde_json::Value;

use crate::protocol::{
    AgentSelection, AppearanceMode, AutoSettle, ClaudePermissionMode, CodexApprovalPolicy,
    CodexSandboxMode, CommandAutoExpand, CopilotPermissions, EffectiveSettings, EmojiVisibility,
    FoldPosture, LandingPage, ProviderId, ReasoningSummaryDetail, ReasoningVisibility,
    SessionContentWidth, SettingMutation, SettingScope, SettingsDiagnostic,
    SettingsDiagnosticSeverity, SettingsSnapshot, SidebarScope, SidebarVisibility,
    TextSelectionCopy, TitleErrand,
};

/// The Config Document Suru prefers when both accepted names exist.
pub const PRIMARY_CONFIG_FILE: &str = "suru.jsonc";
/// Accepted when the primary file is absent; parsed just as leniently.
pub const FALLBACK_CONFIG_FILE: &str = "suru.json";

// The dotted key of each Setting, named once so the schema and the typed
// mutations that edit it can never drift apart.
const APPEARANCE_THEME: &str = "appearance.theme";
const APPEARANCE_MODE: &str = "appearance.mode";
const APPEARANCE_LANDING_PAGE: &str = "appearance.landingPage";
const APPEARANCE_SHOW_ICONS: &str = "appearance.showIcons";
const TEXT_SELECTION_COPY: &str = "textSelection.copy";
const TRANSCRIPT_DEFAULT_FOLD_POSTURE: &str = "transcript.defaultFoldPosture";
const TRANSCRIPT_REASONING_VISIBILITY: &str = "transcript.reasoningVisibility";
const TRANSCRIPT_COMMAND_AUTO_EXPAND: &str = "transcript.commandAutoExpand";
const SESSION_CONTENT_WIDTH: &str = "session.contentWidth";
// Keyed per purpose rather than Errand-wide, so a compaction Errand arriving
// later gets its own key and turning Titles off can never silently disable
// work that has nothing to do with them.
const SESSION_TITLE_ERRAND: &str = "session.title.errand";
const SESSION_TITLE_EMOJI: &str = "session.title.emoji";
const SIDEBAR_INITIAL_VISIBILITY: &str = "sidebar.initialVisibility";
const SIDEBAR_INITIAL_WIDTH: &str = "sidebar.initialWidth";
const SIDEBAR_INITIAL_SCOPE: &str = "sidebar.initialScope";
const SIDEBAR_AUTO_SETTLE: &str = "sidebar.autoSettle";
const PROVIDER_CODEX_ENABLED: &str = "provider.codex.enabled";
const PROVIDER_CODEX_REASONING_SUMMARY: &str = "provider.codex.reasoningSummary";
const PROVIDER_CODEX_APPROVAL_POLICY: &str = "provider.codex.approvalPolicy";
const PROVIDER_CODEX_SANDBOX_MODE: &str = "provider.codex.sandboxMode";
const PROVIDER_COPILOT_ENABLED: &str = "provider.copilot.enabled";
const PROVIDER_COPILOT_PERMISSIONS: &str = "provider.copilot.permissions";
const PROVIDER_CLAUDE_ENABLED: &str = "provider.claude.enabled";
const PROVIDER_CLAUDE_PERMISSION_MODE: &str = "provider.claude.permissionMode";
const SERVING_ENABLED: &str = "serving.enabled";
const SERVING_PORT: &str = "serving.port";
const SERVING_BIND_ADDRESS: &str = "serving.bindAddress";

/// What a Config Document that does not exist yet is edited as.
const EMPTY_DOCUMENT: &str = "{}\n";

/// Which company a Setting keeps in the settings panel, which is the whole of
/// what a Setting says about its own presentation: the panel maps a group to
/// the tab that lists it, so moving a Setting between tabs stays a one-line
/// schema change and a Setting can never belong to two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingGroup {
    /// Settings governing how every Client surface is painted.
    Appearance,
    /// Settings that configure no Provider.
    General,
    /// Settings scoped to one Provider, which the panel presents beside the
    /// Provider they configure rather than in a flat list.
    Providers,
    /// Settings still finding their shape, kept apart so a reader meets them
    /// knowing as much. Nothing else follows from the group: an experimental
    /// Setting is loaded, pinned, and edited exactly like any other, and
    /// settling one is a one-line move to the group it belongs in.
    Experimental,
}

/// One Setting's compile-time definition: what a Config Document calls it,
/// what it may hold, and how each of those values is read and written.
pub struct SettingDescriptor {
    /// Dotted camelCase path of the Setting in a Config Document.
    pub key: &'static str,
    /// What the Setting is called where a person reads it rather than writes it.
    pub label: &'static str,
    /// What choosing between its values means, in one line.
    pub description: &'static str,
    /// Which tab of the settings panel presents this Setting.
    pub group: SettingGroup,
    pub scope: SettingScope,
    /// What the Setting accepts, and whether the schema can name all of it.
    pub values: SettingValues,
    /// The mutation that takes this Setting's pin out of the Config Document,
    /// so the built-in default resumes.
    pub reset: SettingMutation,
    /// Writes a pinned JSON value into the typed field it governs, or reports
    /// that the value is not one of the accepted ones.
    apply: fn(&mut EffectiveSettings, &Value) -> bool,
}

/// What a Setting accepts. Almost every Setting is Fixed: a set of compile-time
/// choices, which is what lets a client cycle one through its values and what
/// lets a diagnostic tell a reader exactly what to type. A Setting whose values
/// are not all known until Suru is running — one holding an Agent Selection
/// discovered from a Provider, say — cannot be written down that way, so an
/// Open one names the values the schema can and describes the rest.
pub enum SettingValues {
    /// The whole of what the Setting accepts, in the order a reader cycles
    /// them.
    Fixed(&'static [SettingChoice]),
    /// A Setting the schema can only partly enumerate.
    Open {
        /// The values the schema does name, in the order a reader cycles them.
        named: &'static [SettingChoice],
        /// What the values it does not name are, phrased to stand as the last
        /// item of a diagnostic's list of accepted values — "an Agent
        /// Selection". A reader cannot be told what to type here, so they are
        /// told what kind of thing belongs instead.
        accepts: &'static str,
        /// How the value in force reads when it is none of the named ones, so
        /// a surface presenting the Setting draws what it holds rather than a
        /// confession that it cannot say. Total, because the Setting can spell
        /// anything its own typed field is able to hold.
        spell: fn(&EffectiveSettings) -> String,
        /// Where a reader chooses one of those unnamed values, for a Setting
        /// whose unnamed values are theirs to pick. `None` where they are not:
        /// a Setting holding something Suru worked out for itself offers the
        /// reader the named values and nothing more.
        chosen_at: Option<SettingChoiceSurface>,
    },
}

/// The surface a reader chooses an Open Setting's unnamed value at, and the pin
/// that puts what they chose in force. The surface is one Suru already has, so
/// a Setting holding a value too rich to cycle through costs no second place to
/// choose that kind of value.
#[derive(Clone, Copy)]
pub enum SettingChoiceSurface {
    /// The Model picker, which is where every Agent Selection in Suru is
    /// chosen.
    AgentSelection {
        /// The Selection in force, where the Setting is holding one, so the
        /// picker opens on the Model the reader already chose rather than at
        /// the top of the list.
        current: fn(&EffectiveSettings) -> Option<AgentSelection>,
        /// What the Selection they choose becomes.
        pin: fn(AgentSelection) -> SettingMutation,
    },
    /// A numeric editor whose complete behavior is declared by the Setting.
    /// The panel only hosts the editor; it never needs to know which typed
    /// field the number belongs to or how that Setting spells it.
    Numeric(NumericSettingChoice),
    /// A Theme picker carrying only the current name and the mutation that
    /// pins a chosen name. The picker need not know which Setting opened it.
    Theme {
        current: fn(&EffectiveSettings) -> String,
        pin: fn(String) -> SettingMutation,
    },
}

/// Everything a reusable numeric editor needs to edit one Setting. Callers
/// receive the seed text and either a complete typed mutation or the concise
/// validation explanation; the numeric value and its Setting-specific
/// conversion stay behind this interface.
#[derive(Clone, Copy, Debug)]
pub struct NumericSettingChoice {
    label: &'static str,
    seed: fn(&EffectiveSettings) -> u64,
    validate: fn(&str) -> Result<u64, &'static str>,
    spell: fn(u64) -> String,
    pin: fn(u64) -> SettingMutation,
}

impl NumericSettingChoice {
    pub const fn new(
        label: &'static str,
        seed: fn(&EffectiveSettings) -> u64,
        validate: fn(&str) -> Result<u64, &'static str>,
        spell: fn(u64) -> String,
        pin: fn(u64) -> SettingMutation,
    ) -> Self {
        Self {
            label,
            seed,
            validate,
            spell,
            pin,
        }
    }

    pub fn label(self) -> &'static str {
        self.label
    }

    pub fn seed(self, settings: &EffectiveSettings) -> String {
        (self.seed)(settings).to_string()
    }

    pub fn accept(self, input: &str) -> Result<SettingMutation, &'static str> {
        (self.validate)(input).map(self.pin)
    }

    pub fn spell(self, value: u64) -> String {
        (self.spell)(value)
    }
}

fn spell_session_content_width(maximum: u64) -> String {
    format!("max {maximum} columns")
}

fn validate_session_content_width(value: &str) -> Result<u64, &'static str> {
    value
        .parse::<u64>()
        .ok()
        .filter(|maximum| *maximum >= 50)
        .ok_or("minimum: 50")
}

const SESSION_CONTENT_WIDTH_NUMERIC: NumericSettingChoice = NumericSettingChoice::new(
    "Maximum columns",
    |settings| match settings.session.content_width {
        SessionContentWidth::Fill => 80,
        SessionContentWidth::Maximum(maximum) => maximum,
    },
    validate_session_content_width,
    spell_session_content_width,
    |maximum| SettingMutation::SessionContentWidth {
        value: Some(SessionContentWidth::Maximum(maximum)),
    },
);

fn spell_command_auto_expand(milliseconds: u64) -> String {
    format!("{milliseconds}ms")
}

fn validate_command_auto_expand(value: &str) -> Result<u64, &'static str> {
    value.parse::<u64>().map_err(|_| "a whole number")
}

const COMMAND_AUTO_EXPAND_NUMERIC: NumericSettingChoice = NumericSettingChoice::new(
    "Delay in milliseconds",
    |settings| match settings.transcript.command_auto_expand {
        CommandAutoExpand::Off => CommandAutoExpand::DEFAULT_MILLIS,
        CommandAutoExpand::AfterMillis(milliseconds) => milliseconds,
    },
    validate_command_auto_expand,
    spell_command_auto_expand,
    |milliseconds| SettingMutation::TranscriptCommandAutoExpand {
        value: Some(CommandAutoExpand::AfterMillis(milliseconds)),
    },
);

fn spell_auto_settle_idle_days(days: u64) -> String {
    if days == 1 {
        "1 day".to_owned()
    } else {
        format!("{days} days")
    }
}

fn validate_auto_settle_idle_days(value: &str) -> Result<u64, &'static str> {
    value
        .parse::<u64>()
        .ok()
        .filter(|days| *days >= AutoSettle::MINIMUM_IDLE_DAYS)
        .ok_or("minimum: 1")
}

const SIDEBAR_AUTO_SETTLE_NUMERIC: NumericSettingChoice = NumericSettingChoice::new(
    "Days idle",
    |settings| match settings.sidebar.auto_settle {
        // The built-in threshold, so a reader turning settling back on begins
        // where it began rather than at nothing.
        AutoSettle::Off => 3,
        AutoSettle::Idle(days) => days,
    },
    validate_auto_settle_idle_days,
    spell_auto_settle_idle_days,
    |days| SettingMutation::SidebarAutoSettle {
        value: Some(AutoSettle::Idle(days)),
    },
);

fn validate_sidebar_initial_width(value: &str) -> Result<u64, &'static str> {
    value
        .parse::<u64>()
        .ok()
        .filter(|columns| *columns >= 24)
        .ok_or("minimum: 24")
}

const SIDEBAR_INITIAL_WIDTH_NUMERIC: NumericSettingChoice = NumericSettingChoice::new(
    "Columns at launch",
    |settings| settings.sidebar.initial_width,
    validate_sidebar_initial_width,
    |columns| format!("{columns} columns"),
    |columns| SettingMutation::SidebarInitialWidth {
        value: Some(columns),
    },
);

fn validate_serving_port(value: &str) -> Result<u64, &'static str> {
    value
        .parse::<u16>()
        .map(u64::from)
        .map_err(|_| "a port from 0 to 65535")
}

const SERVING_PORT_NUMERIC: NumericSettingChoice = NumericSettingChoice::new(
    "Serving port",
    |settings| u64::from(settings.serving.port),
    validate_serving_port,
    |port| port.to_string(),
    |port| SettingMutation::ServingPort {
        value: Some(port as u16),
    },
);

impl SettingValues {
    /// The values the schema names, which is everything a Fixed Setting accepts
    /// and only part of what an Open one does. This is the list a client cycles
    /// and the list a diagnostic spells out.
    pub fn named(&self) -> &'static [SettingChoice] {
        match *self {
            Self::Fixed(choices) => choices,
            Self::Open { named, .. } => named,
        }
    }

    /// Where this Setting's unnamed values are chosen, if anywhere.
    fn chosen_at(&self) -> Option<SettingChoiceSurface> {
        match *self {
            Self::Fixed(_) => None,
            Self::Open { chosen_at, .. } => chosen_at,
        }
    }
}

/// One value a Setting can hold, and the mutation factory that pins it.
pub struct SettingChoice {
    /// The value as a Config Document spells it, which is also what a client
    /// shows: what the reader sees and what they would type are the same word.
    /// The spelling elides JSON's own quoting, so a string choice reads
    /// `folded` rather than `"folded"` and a boolean one reads `true`.
    pub value: &'static str,
    /// Builds the mutation that pins this value, even when it is the built-in default.
    pub build_mutation: fn() -> SettingMutation,
}

impl SettingChoice {
    /// Builds the typed mutation for this choice. A function keeps the static
    /// schema able to name choices whose mutation owns data, such as a Theme
    /// name, without leaking or special-casing that Setting.
    pub fn mutation(&self) -> SettingMutation {
        (self.build_mutation)()
    }

    /// The JSON this choice's pin writes into a Config Document, which is what
    /// the loader reads back.
    fn pinned_value(&self) -> Value {
        pin_for(&self.mutation())
            .1
            .expect("a Setting's choice always pins a value")
    }

    /// The choice as a message naming it spells it: quoted when a Config
    /// Document spells it as a JSON string, bare when the document spells it
    /// as a literal such as `true`. Telling the reader what to type is the
    /// whole job of the diagnostic this feeds, so the quoting is not cosmetic.
    fn spelled(&self) -> String {
        match self.pinned_value() {
            Value::String(_) => format!("{:?}", self.value),
            _ => self.value.to_owned(),
        }
    }
}

impl SettingDescriptor {
    /// The named choice the effective settings hold for this Setting, or `None`
    /// when they hold a value the schema does not name — which is what an Open
    /// Setting reads as whenever it is holding one of the values only its own
    /// declaration could describe, and what a Fixed Setting that grew a value
    /// without growing a choice for it would read as. Either way a client shows
    /// it honestly rather than dying on the draw.
    pub fn effective(&self, settings: &EffectiveSettings) -> Option<&'static SettingChoice> {
        self.effective_index(settings)
            .map(|index| &self.values.named()[index])
    }

    /// What the value in force reads as, which is what a surface presenting the
    /// Setting draws: the named choice it is on, or an Open Setting's own
    /// spelling of a value the schema never named. `None` is a Fixed Setting
    /// holding a value outside its choices — a schema that fell behind its own
    /// types, which a client says rather than guesses at.
    pub fn spelling(&self, settings: &EffectiveSettings) -> Option<Cow<'static, str>> {
        if let Some(choice) = self.effective(settings) {
            return Some(Cow::Borrowed(choice.value));
        }
        match self.values {
            SettingValues::Fixed(_) => None,
            SettingValues::Open { spell, .. } => Some(Cow::Owned(spell(settings))),
        }
    }

    /// The named choice one step on from the one in force, wrapping past the
    /// last, which is how a reader cycles a Setting through its values. A value
    /// the schema does not name has no neighbours, so the first choice is where
    /// the step lands instead.
    pub fn next_choice(&self, settings: &EffectiveSettings) -> Option<&'static SettingChoice> {
        let named = self.values.named();
        let index = match self.effective_index(settings) {
            Some(current) => (current + 1) % named.len(),
            None => 0,
        };
        named.get(index)
    }

    /// The surface this Setting's unnamed values are chosen at, which is what a
    /// row acts on when a reader opens it.
    pub fn chosen_at(&self) -> Option<SettingChoiceSurface> {
        self.values.chosen_at()
    }

    fn effective_index(&self, settings: &EffectiveSettings) -> Option<usize> {
        self.values
            .named()
            .iter()
            .position(|choice| pins_effective_value(&choice.mutation(), settings))
    }

    /// The accepted values, phrased for a diagnostic's "why" clause. Read off
    /// the Setting's own values, so one that grows a value cannot leave a
    /// diagnostic still naming the old set. An Open Setting's description of
    /// what else it takes stands as the last item, which is the only truthful
    /// thing to say where there is no word to tell the reader to type.
    fn expected(&self) -> String {
        let mut values = self
            .values
            .named()
            .iter()
            .map(SettingChoice::spelled)
            .collect::<Vec<_>>();
        if let SettingValues::Open { accepts, .. } = self.values {
            values.push(accepts.to_owned());
        }
        match values.split_last() {
            None => "a value this Setting accepts".to_owned(),
            Some((last, [])) => last.clone(),
            Some((last, [first])) => format!("one of {first} or {last}"),
            Some((last, rest)) => format!("one of {}, or {last}", rest.join(", ")),
        }
    }
}

/// Whether a pin would leave the Setting exactly where the effective settings
/// already have it.
fn pins_effective_value(mutation: &SettingMutation, settings: &EffectiveSettings) -> bool {
    match mutation {
        SettingMutation::AppearanceTheme { value } => {
            value.as_ref() == Some(&settings.appearance.theme)
        }
        SettingMutation::AppearanceMode { value } => *value == Some(settings.appearance.mode),
        SettingMutation::AppearanceLandingPage { value } => {
            *value == Some(settings.appearance.landing_page)
        }
        SettingMutation::AppearanceShowIcons { value } => {
            *value == Some(settings.appearance.show_icons)
        }
        SettingMutation::TextSelectionCopy { value } => {
            *value == Some(settings.text_selection.copy)
        }
        SettingMutation::TranscriptDefaultFoldPosture { value } => {
            *value == Some(settings.transcript.default_fold_posture)
        }
        SettingMutation::TranscriptReasoningVisibility { value } => {
            *value == Some(settings.transcript.reasoning_visibility)
        }
        SettingMutation::TranscriptCommandAutoExpand { value } => {
            *value == Some(settings.transcript.command_auto_expand)
        }
        SettingMutation::SessionContentWidth { value } => {
            *value == Some(settings.session.content_width)
        }
        SettingMutation::SessionTitleErrand { value } => {
            value.as_ref() == Some(&settings.session.title.errand)
        }
        SettingMutation::SessionTitleEmoji { value } => {
            *value == Some(settings.session.title.emoji)
        }
        SettingMutation::SidebarInitialVisibility { value } => {
            *value == Some(settings.sidebar.initial_visibility)
        }
        SettingMutation::SidebarInitialWidth { value } => {
            *value == Some(settings.sidebar.initial_width)
        }
        SettingMutation::SidebarInitialScope { value } => {
            *value == Some(settings.sidebar.initial_scope)
        }
        SettingMutation::SidebarAutoSettle { value } => {
            *value == Some(settings.sidebar.auto_settle)
        }
        SettingMutation::ProviderCodexEnabled { value } => {
            *value == Some(settings.provider.codex.enabled)
        }
        SettingMutation::ProviderCodexReasoningSummary { value } => {
            *value == Some(settings.provider.codex.reasoning_summary)
        }
        SettingMutation::ProviderCodexApprovalPolicy { value } => {
            *value == Some(settings.provider.codex.approval_policy)
        }
        SettingMutation::ProviderCodexSandboxMode { value } => {
            *value == Some(settings.provider.codex.sandbox_mode)
        }
        SettingMutation::ProviderCopilotEnabled { value } => {
            *value == Some(settings.provider.copilot.enabled)
        }
        SettingMutation::ProviderCopilotPermissions { value } => {
            *value == Some(settings.provider.copilot.permissions)
        }
        SettingMutation::ProviderClaudeEnabled { value } => {
            *value == Some(settings.provider.claude.enabled)
        }
        SettingMutation::ProviderClaudePermissionMode { value } => {
            *value == Some(settings.provider.claude.permission_mode)
        }
        SettingMutation::ServingEnabled { value } => *value == Some(settings.serving.enabled),
        SettingMutation::ServingPort { value } => *value == Some(settings.serving.port),
        SettingMutation::ServingBindAddress { value } => {
            *value == Some(settings.serving.bind_address)
        }
    }
}

/// Every defined Setting. The panel, the loader, and future overlays all read
/// this one table: a Setting arrives as its typed field on
/// [`EffectiveSettings`], its entry here, and the [`SettingMutation`] variant
/// through which a client edits it — the key each of them spells is a constant
/// above, so the three can never drift apart.
pub const SCHEMA: &[SettingDescriptor] = &[
    SettingDescriptor {
        key: APPEARANCE_THEME,
        label: "Theme",
        description: "The Theme every open view is painted in",
        group: SettingGroup::Appearance,
        scope: SettingScope::Client,
        values: SettingValues::Open {
            named: &[SettingChoice {
                value: "system",
                build_mutation: || SettingMutation::AppearanceTheme {
                    value: Some("system".to_owned()),
                },
            }],
            accepts: "a Theme name",
            spell: |settings| settings.appearance.theme.clone(),
            chosen_at: Some(SettingChoiceSurface::Theme {
                current: |settings| settings.appearance.theme.clone(),
                pin: |theme| SettingMutation::AppearanceTheme { value: Some(theme) },
            }),
        },
        reset: SettingMutation::AppearanceTheme { value: None },
        apply: |settings, value| {
            apply_value(value, |theme| {
                settings.appearance.theme = theme;
            })
        },
    },
    SettingDescriptor {
        key: APPEARANCE_MODE,
        label: "Mode",
        description: "Whether Themes follow the terminal or use a dark or light variant",
        group: SettingGroup::Appearance,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "system",
                build_mutation: || SettingMutation::AppearanceMode {
                    value: Some(AppearanceMode::System),
                },
            },
            SettingChoice {
                value: "dark",
                build_mutation: || SettingMutation::AppearanceMode {
                    value: Some(AppearanceMode::Dark),
                },
            },
            SettingChoice {
                value: "light",
                build_mutation: || SettingMutation::AppearanceMode {
                    value: Some(AppearanceMode::Light),
                },
            },
        ]),
        reset: SettingMutation::AppearanceMode { value: None },
        apply: |settings, value| {
            apply_value(value, |mode| {
                settings.appearance.mode = mode;
            })
        },
    },
    SettingDescriptor {
        key: APPEARANCE_LANDING_PAGE,
        label: "Landing page",
        description: "Minimal shows the composer alone; Fancy adds the Japanese Suru banner",
        group: SettingGroup::Appearance,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "Minimal",
                build_mutation: || SettingMutation::AppearanceLandingPage {
                    value: Some(LandingPage::Minimal),
                },
            },
            SettingChoice {
                value: "Fancy",
                build_mutation: || SettingMutation::AppearanceLandingPage {
                    value: Some(LandingPage::Fancy),
                },
            },
        ]),
        reset: SettingMutation::AppearanceLandingPage { value: None },
        apply: |settings, value| {
            apply_value(value, |landing_page| {
                settings.appearance.landing_page = landing_page;
            })
        },
    },
    SettingDescriptor {
        key: APPEARANCE_SHOW_ICONS,
        label: "Show icons",
        description: "Show Nerd Font icons. Requires a Nerd Font in your terminal.",
        group: SettingGroup::Appearance,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "false",
                build_mutation: || SettingMutation::AppearanceShowIcons { value: Some(false) },
            },
            SettingChoice {
                value: "true",
                build_mutation: || SettingMutation::AppearanceShowIcons { value: Some(true) },
            },
        ]),
        reset: SettingMutation::AppearanceShowIcons { value: None },
        apply: |settings, value| {
            apply_value(value, |show_icons| {
                settings.appearance.show_icons = show_icons;
            })
        },
    },
    SettingDescriptor {
        key: TRANSCRIPT_DEFAULT_FOLD_POSTURE,
        label: "Default Fold posture",
        description: "How a Session view opens: folded to its markers, or expanded in full",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "folded",
                build_mutation: || SettingMutation::TranscriptDefaultFoldPosture {
                    value: Some(FoldPosture::Folded),
                },
            },
            SettingChoice {
                value: "expanded",
                build_mutation: || SettingMutation::TranscriptDefaultFoldPosture {
                    value: Some(FoldPosture::Expanded),
                },
            },
        ]),
        reset: SettingMutation::TranscriptDefaultFoldPosture { value: None },
        apply: |settings, value| {
            apply_value(value, |posture| {
                settings.transcript.default_fold_posture = posture;
            })
        },
    },
    SettingDescriptor {
        key: TRANSCRIPT_REASONING_VISIBILITY,
        label: "Reasoning visibility",
        description: "Whether a Transcript hides Reasoning or draws it",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "hidden",
                build_mutation: || SettingMutation::TranscriptReasoningVisibility {
                    value: Some(ReasoningVisibility::Hidden),
                },
            },
            SettingChoice {
                value: "shown",
                build_mutation: || SettingMutation::TranscriptReasoningVisibility {
                    value: Some(ReasoningVisibility::Shown),
                },
            },
        ]),
        reset: SettingMutation::TranscriptReasoningVisibility { value: None },
        apply: |settings, value| {
            apply_value(value, |visibility| {
                settings.transcript.reasoning_visibility = visibility;
            })
        },
    },
    SettingDescriptor {
        key: TRANSCRIPT_COMMAND_AUTO_EXPAND,
        label: "Command auto-expansion",
        description: "When an Active Command grows into its live output tail",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Open {
            named: &[SettingChoice {
                // This is the named off choice. A SettingChoice shows the
                // Config Document spelling, which is the boolean `false` the
                // Setting accepts rather than a second string spelling.
                value: "false",
                build_mutation: || SettingMutation::TranscriptCommandAutoExpand {
                    value: Some(CommandAutoExpand::Off),
                },
            }],
            accepts: "a whole number of milliseconds",
            spell: |settings| match settings.transcript.command_auto_expand {
                CommandAutoExpand::Off => "false".to_owned(),
                CommandAutoExpand::AfterMillis(milliseconds) => {
                    COMMAND_AUTO_EXPAND_NUMERIC.spell(milliseconds)
                }
            },
            chosen_at: Some(SettingChoiceSurface::Numeric(COMMAND_AUTO_EXPAND_NUMERIC)),
        },
        reset: SettingMutation::TranscriptCommandAutoExpand { value: None },
        apply: |settings, value| {
            apply_value(value, |command_auto_expand| {
                settings.transcript.command_auto_expand = command_auto_expand;
            })
        },
    },
    SettingDescriptor {
        key: SESSION_CONTENT_WIDTH,
        label: "Session content width",
        description: "Whether the Session Content Column fills the terminal or has a maximum",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Open {
            named: &[SettingChoice {
                value: "fill",
                build_mutation: || SettingMutation::SessionContentWidth {
                    value: Some(SessionContentWidth::Fill),
                },
            }],
            accepts: "an integer of at least 50",
            spell: |settings| match settings.session.content_width {
                SessionContentWidth::Fill => "fill".to_owned(),
                SessionContentWidth::Maximum(maximum) => {
                    SESSION_CONTENT_WIDTH_NUMERIC.spell(maximum)
                }
            },
            chosen_at: Some(SettingChoiceSurface::Numeric(SESSION_CONTENT_WIDTH_NUMERIC)),
        },
        reset: SettingMutation::SessionContentWidth { value: None },
        apply: |settings, value| {
            apply_value(value, |content_width| {
                settings.session.content_width = content_width;
            })
        },
    },
    SettingDescriptor {
        key: SESSION_TITLE_ERRAND,
        label: "Title derivation",
        description: "Which Agent Selection derives a Session's Title, if any",
        group: SettingGroup::General,
        scope: SettingScope::Server,
        values: SettingValues::Open {
            named: &[
                SettingChoice {
                    value: "session",
                    build_mutation: || SettingMutation::SessionTitleErrand {
                        value: Some(TitleErrand::FollowSession),
                    },
                },
                SettingChoice {
                    value: "off",
                    build_mutation: || SettingMutation::SessionTitleErrand {
                        value: Some(TitleErrand::Off),
                    },
                },
            ],
            accepts: "an Agent Selection",
            spell: |settings| {
                let errand = &settings.session.title.errand;
                match errand {
                    // The pin's own Model, not the one an Errand would resolve
                    // to: a row reports the choice the reader made, and what a
                    // live catalog makes of it is the Errand's business.
                    TitleErrand::Pinned(selection) => {
                        format!("{} · {}", selection.provider, selection.model)
                    }
                    // Every other value spells itself the way a reader would
                    // have typed it, read off the one place those words are
                    // written down rather than repeated here.
                    TitleErrand::FollowSession | TitleErrand::Off => errand
                        .named()
                        .expect("every value but a pinned Selection has a word")
                        .to_owned(),
                }
            },
            chosen_at: Some(SettingChoiceSurface::AgentSelection {
                current: |settings| match &settings.session.title.errand {
                    TitleErrand::Pinned(selection) => Some(selection.clone()),
                    TitleErrand::FollowSession | TitleErrand::Off => None,
                },
                pin: |selection| SettingMutation::SessionTitleErrand {
                    value: Some(TitleErrand::Pinned(selection)),
                },
            }),
        },
        reset: SettingMutation::SessionTitleErrand { value: None },
        apply: |settings, value| {
            apply_value(value, |errand| {
                settings.session.title.errand = errand;
            })
        },
    },
    SettingDescriptor {
        key: SIDEBAR_INITIAL_VISIBILITY,
        label: "Sidebar at launch",
        description: "Whether a TUI opens with the Sidebar beside its main view",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "shown",
                build_mutation: || SettingMutation::SidebarInitialVisibility {
                    value: Some(SidebarVisibility::Shown),
                },
            },
            SettingChoice {
                value: "hidden",
                build_mutation: || SettingMutation::SidebarInitialVisibility {
                    value: Some(SidebarVisibility::Hidden),
                },
            },
        ]),
        reset: SettingMutation::SidebarInitialVisibility { value: None },
        apply: |settings, value| {
            apply_value(value, |visibility| {
                settings.sidebar.initial_visibility = visibility;
            })
        },
    },
    SettingDescriptor {
        key: SIDEBAR_INITIAL_WIDTH,
        label: "Sidebar width at launch",
        description: "How many columns wide a TUI's Sidebar opens",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Open {
            named: &[],
            accepts: "an integer of at least 24",
            spell: |settings| SIDEBAR_INITIAL_WIDTH_NUMERIC.spell(settings.sidebar.initial_width),
            chosen_at: Some(SettingChoiceSurface::Numeric(SIDEBAR_INITIAL_WIDTH_NUMERIC)),
        },
        reset: SettingMutation::SidebarInitialWidth { value: None },
        apply: |settings, value| {
            let Some(width) = value.as_u64().filter(|width| *width >= 24) else {
                return false;
            };
            settings.sidebar.initial_width = width;
            true
        },
    },
    SettingDescriptor {
        key: SIDEBAR_INITIAL_SCOPE,
        label: "Sidebar scope at launch",
        description: "Which Workspaces a TUI's Sidebar lists when it opens",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "all_workspaces",
                build_mutation: || SettingMutation::SidebarInitialScope {
                    value: Some(SidebarScope::AllWorkspaces),
                },
            },
            SettingChoice {
                value: "current_workspace",
                build_mutation: || SettingMutation::SidebarInitialScope {
                    value: Some(SidebarScope::CurrentWorkspace),
                },
            },
            SettingChoice {
                value: "everywhere",
                build_mutation: || SettingMutation::SidebarInitialScope {
                    value: Some(SidebarScope::Everywhere),
                },
            },
        ]),
        reset: SettingMutation::SidebarInitialScope { value: None },
        apply: |settings, value| {
            apply_value(value, |scope| {
                settings.sidebar.initial_scope = scope;
            })
        },
    },
    SettingDescriptor {
        key: SIDEBAR_AUTO_SETTLE,
        label: "Settle idle Sessions",
        description: "Whether a Session settles itself once it has been left alone long enough",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        // One Setting rather than an enablement beside a threshold, per ADR
        // 0012: the pair would admit a threshold pinned beside the `off` that
        // makes it mean nothing, and the scalar cannot be read two ways.
        values: SettingValues::Open {
            named: &[SettingChoice {
                value: "off",
                build_mutation: || SettingMutation::SidebarAutoSettle {
                    value: Some(AutoSettle::Off),
                },
            }],
            accepts: "a whole number of days, at least 1",
            spell: |settings| match settings.sidebar.auto_settle {
                AutoSettle::Off => "off".to_owned(),
                AutoSettle::Idle(days) => SIDEBAR_AUTO_SETTLE_NUMERIC.spell(days),
            },
            chosen_at: Some(SettingChoiceSurface::Numeric(SIDEBAR_AUTO_SETTLE_NUMERIC)),
        },
        reset: SettingMutation::SidebarAutoSettle { value: None },
        apply: |settings, value| {
            apply_value(value, |auto_settle| {
                settings.sidebar.auto_settle = auto_settle;
            })
        },
    },
    // Each Provider's Enablement is ordered immediately before that Provider's
    // other Settings, which is the order the loader's diagnostics and this
    // table itself read in; the settings panel presents Enablement on the
    // Provider's own row rather than in this order.
    SettingDescriptor {
        key: TEXT_SELECTION_COPY,
        label: "Copy Text Selection",
        description: "Copy when a drag ends, or manually with Ctrl+C or right-click",
        group: SettingGroup::General,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "release",
                build_mutation: || SettingMutation::TextSelectionCopy {
                    value: Some(TextSelectionCopy::Release),
                },
            },
            SettingChoice {
                value: "manual",
                build_mutation: || SettingMutation::TextSelectionCopy {
                    value: Some(TextSelectionCopy::Manual),
                },
            },
        ]),
        reset: SettingMutation::TextSelectionCopy { value: None },
        apply: |settings, value| apply_value(value, |copy| settings.text_selection.copy = copy),
    },
    SettingDescriptor {
        key: PROVIDER_CODEX_ENABLED,
        label: "Codex Provider",
        description: "Whether Suru offers Codex, or leaves it entirely alone",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "true",
                build_mutation: || SettingMutation::ProviderCodexEnabled { value: Some(true) },
            },
            SettingChoice {
                value: "false",
                build_mutation: || SettingMutation::ProviderCodexEnabled { value: Some(false) },
            },
        ]),
        reset: SettingMutation::ProviderCodexEnabled { value: None },
        apply: |settings, value| {
            apply_value(value, |enabled| {
                settings.provider.codex.enabled = enabled;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CODEX_REASONING_SUMMARY,
        // A Provider-scoped Setting is read under the Provider it configures,
        // which names it, so the label names the Setting alone.
        label: "Reasoning summary",
        description: "How much Reasoning summary detail each Turn asks Codex for",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "auto",
                build_mutation: || SettingMutation::ProviderCodexReasoningSummary {
                    value: Some(ReasoningSummaryDetail::Auto),
                },
            },
            SettingChoice {
                value: "concise",
                build_mutation: || SettingMutation::ProviderCodexReasoningSummary {
                    value: Some(ReasoningSummaryDetail::Concise),
                },
            },
            SettingChoice {
                value: "detailed",
                build_mutation: || SettingMutation::ProviderCodexReasoningSummary {
                    value: Some(ReasoningSummaryDetail::Detailed),
                },
            },
            SettingChoice {
                value: "none",
                build_mutation: || SettingMutation::ProviderCodexReasoningSummary {
                    value: Some(ReasoningSummaryDetail::None),
                },
            },
        ]),
        reset: SettingMutation::ProviderCodexReasoningSummary { value: None },
        apply: |settings, value| {
            apply_value(value, |detail| {
                settings.provider.codex.reasoning_summary = detail;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CODEX_APPROVAL_POLICY,
        label: "Approval policy",
        description: "When Codex asks before running a Tool",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "untrusted",
                build_mutation: || SettingMutation::ProviderCodexApprovalPolicy {
                    value: Some(CodexApprovalPolicy::Untrusted),
                },
            },
            SettingChoice {
                value: "on-request",
                build_mutation: || SettingMutation::ProviderCodexApprovalPolicy {
                    value: Some(CodexApprovalPolicy::OnRequest),
                },
            },
            SettingChoice {
                value: "never",
                build_mutation: || SettingMutation::ProviderCodexApprovalPolicy {
                    value: Some(CodexApprovalPolicy::Never),
                },
            },
        ]),
        reset: SettingMutation::ProviderCodexApprovalPolicy { value: None },
        apply: |settings, value| {
            apply_value(value, |policy| {
                settings.provider.codex.approval_policy = policy;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CODEX_SANDBOX_MODE,
        label: "Sandbox mode",
        description: "What Codex may access without an Approval",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "read-only",
                build_mutation: || SettingMutation::ProviderCodexSandboxMode {
                    value: Some(CodexSandboxMode::ReadOnly),
                },
            },
            SettingChoice {
                value: "workspace-write",
                build_mutation: || SettingMutation::ProviderCodexSandboxMode {
                    value: Some(CodexSandboxMode::WorkspaceWrite),
                },
            },
            SettingChoice {
                value: "danger-full-access",
                build_mutation: || SettingMutation::ProviderCodexSandboxMode {
                    value: Some(CodexSandboxMode::DangerFullAccess),
                },
            },
        ]),
        reset: SettingMutation::ProviderCodexSandboxMode { value: None },
        apply: |settings, value| {
            apply_value(value, |mode| {
                settings.provider.codex.sandbox_mode = mode;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_COPILOT_ENABLED,
        label: "Copilot Provider",
        description: "Whether Suru offers Copilot, or leaves it entirely alone",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "true",
                build_mutation: || SettingMutation::ProviderCopilotEnabled { value: Some(true) },
            },
            SettingChoice {
                value: "false",
                build_mutation: || SettingMutation::ProviderCopilotEnabled { value: Some(false) },
            },
        ]),
        reset: SettingMutation::ProviderCopilotEnabled { value: None },
        apply: |settings, value| {
            apply_value(value, |enabled| {
                settings.provider.copilot.enabled = enabled;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_COPILOT_PERMISSIONS,
        label: "Permissions",
        description: "When Copilot asks before using a Tool",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "ask",
                build_mutation: || SettingMutation::ProviderCopilotPermissions {
                    value: Some(CopilotPermissions::Ask),
                },
            },
            SettingChoice {
                value: "allowAll",
                build_mutation: || SettingMutation::ProviderCopilotPermissions {
                    value: Some(CopilotPermissions::AllowAll),
                },
            },
        ]),
        reset: SettingMutation::ProviderCopilotPermissions { value: None },
        apply: |settings, value| {
            apply_value(value, |permissions| {
                settings.provider.copilot.permissions = permissions;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CLAUDE_ENABLED,
        label: "Claude Provider",
        description: "Whether Suru offers Claude, or leaves it entirely alone",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "true",
                build_mutation: || SettingMutation::ProviderClaudeEnabled { value: Some(true) },
            },
            SettingChoice {
                value: "false",
                build_mutation: || SettingMutation::ProviderClaudeEnabled { value: Some(false) },
            },
        ]),
        reset: SettingMutation::ProviderClaudeEnabled { value: None },
        apply: |settings, value| {
            apply_value(value, |enabled| {
                settings.provider.claude.enabled = enabled;
            })
        },
    },
    SettingDescriptor {
        key: PROVIDER_CLAUDE_PERMISSION_MODE,
        label: "Permission mode",
        description: "When Claude asks before using a Tool",
        group: SettingGroup::Providers,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "default",
                build_mutation: || SettingMutation::ProviderClaudePermissionMode {
                    value: Some(ClaudePermissionMode::Default),
                },
            },
            SettingChoice {
                value: "acceptEdits",
                build_mutation: || SettingMutation::ProviderClaudePermissionMode {
                    value: Some(ClaudePermissionMode::AcceptEdits),
                },
            },
            SettingChoice {
                value: "dontAsk",
                build_mutation: || SettingMutation::ProviderClaudePermissionMode {
                    value: Some(ClaudePermissionMode::DontAsk),
                },
            },
            SettingChoice {
                value: "bypassPermissions",
                build_mutation: || SettingMutation::ProviderClaudePermissionMode {
                    value: Some(ClaudePermissionMode::BypassPermissions),
                },
            },
            SettingChoice {
                value: "auto",
                build_mutation: || SettingMutation::ProviderClaudePermissionMode {
                    value: Some(ClaudePermissionMode::Auto),
                },
            },
        ]),
        reset: SettingMutation::ProviderClaudePermissionMode { value: None },
        apply: |settings, value| {
            apply_value(value, |mode| {
                settings.provider.claude.permission_mode = mode
            })
        },
    },
    // The experimental Settings stand last, as the tab presenting them does.
    SettingDescriptor {
        key: SESSION_TITLE_EMOJI,
        label: "Session name Emojis",
        description: "Whether the Emoji derived for a Session is drawn wherever it is named",
        group: SettingGroup::Experimental,
        scope: SettingScope::Client,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "hidden",
                build_mutation: || SettingMutation::SessionTitleEmoji {
                    value: Some(EmojiVisibility::Hidden),
                },
            },
            SettingChoice {
                value: "shown",
                build_mutation: || SettingMutation::SessionTitleEmoji {
                    value: Some(EmojiVisibility::Shown),
                },
            },
        ]),
        reset: SettingMutation::SessionTitleEmoji { value: None },
        apply: |settings, value| {
            apply_value(value, |visibility| {
                settings.session.title.emoji = visibility;
            })
        },
    },
    SettingDescriptor {
        key: SERVING_ENABLED,
        label: "Serving",
        description: "Whether this Server accepts paired Servers on its second listener",
        group: SettingGroup::Experimental,
        scope: SettingScope::Server,
        values: SettingValues::Fixed(&[
            SettingChoice {
                value: "false",
                build_mutation: || SettingMutation::ServingEnabled { value: Some(false) },
            },
            SettingChoice {
                value: "true",
                build_mutation: || SettingMutation::ServingEnabled { value: Some(true) },
            },
        ]),
        reset: SettingMutation::ServingEnabled { value: None },
        apply: |settings, value| {
            apply_value(value, |enabled| {
                settings.serving.enabled = enabled;
            })
        },
    },
    SettingDescriptor {
        key: SERVING_PORT,
        label: "Serving port",
        description: "The TCP port the Serving listener binds",
        group: SettingGroup::Experimental,
        scope: SettingScope::Server,
        values: SettingValues::Open {
            named: &[],
            accepts: "a port from 0 to 65535",
            spell: |settings| settings.serving.port.to_string(),
            chosen_at: Some(SettingChoiceSurface::Numeric(SERVING_PORT_NUMERIC)),
        },
        reset: SettingMutation::ServingPort { value: None },
        apply: |settings, value| {
            apply_value(value, |port| {
                settings.serving.port = port;
            })
        },
    },
    SettingDescriptor {
        key: SERVING_BIND_ADDRESS,
        label: "Serving bind address",
        description: "Which local network address the Serving listener binds",
        group: SettingGroup::Experimental,
        scope: SettingScope::Server,
        values: SettingValues::Open {
            named: &[
                SettingChoice {
                    value: "127.0.0.1",
                    build_mutation: || SettingMutation::ServingBindAddress {
                        value: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    },
                },
                SettingChoice {
                    value: "::1",
                    build_mutation: || SettingMutation::ServingBindAddress {
                        value: Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
                    },
                },
                SettingChoice {
                    value: "0.0.0.0",
                    build_mutation: || SettingMutation::ServingBindAddress {
                        value: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                    },
                },
                SettingChoice {
                    value: "::",
                    build_mutation: || SettingMutation::ServingBindAddress {
                        value: Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
                    },
                },
            ],
            accepts: "an IP address",
            spell: |settings| settings.serving.bind_address.to_string(),
            chosen_at: None,
        },
        reset: SettingMutation::ServingBindAddress { value: None },
        apply: |settings, value| {
            apply_value(value, |address| {
                settings.serving.bind_address = address;
            })
        },
    },
];

/// The Setting through which the user turns one Provider on or off, which is
/// what a surface presenting the Provider itself edits. Looked up by the key
/// the schema keys Enablement on, so a Provider and its Enablement are tied by
/// the Provider's own identity rather than by a second table to keep in step.
pub fn provider_enablement(provider: &ProviderId) -> Option<&'static SettingDescriptor> {
    let key = format!("provider.{provider}.enabled");
    SCHEMA.iter().find(|descriptor| descriptor.key == key)
}

/// Everything else one Provider is configured by, in schema order: the
/// Settings a surface presenting the Provider reveals under it, its Enablement
/// left out because the Provider itself is that Setting's surface. Keyed on the
/// Provider's own identity like [`provider_enablement`], so a Setting joins the
/// Provider it names by being written down once.
pub fn provider_settings(provider: &ProviderId) -> Vec<&'static SettingDescriptor> {
    let prefix = format!("provider.{provider}.");
    // Read off `provider_enablement` rather than spelled again here, so the
    // key the Provider's own row stands for is written down once.
    let enablement = provider_enablement(provider).map(|descriptor| descriptor.key);
    SCHEMA
        .iter()
        .filter(|descriptor| {
            descriptor.key.starts_with(&prefix) && Some(descriptor.key) != enablement
        })
        .collect()
}

fn apply_value<T: serde::de::DeserializeOwned>(value: &Value, write: impl FnOnce(T)) -> bool {
    match serde_json::from_value(value.clone()) {
        Ok(value) => {
            write(value);
            true
        }
        Err(_) => false,
    }
}

/// Resolves the config root the way the state directory resolves: an explicit
/// `SURU_CONFIG_DIR` wins, then `$XDG_CONFIG_HOME/suru/`, then the literal
/// `~/.config/suru/` on every platform — raw XDG, replicating opencode,
/// deliberately not the platform-native dirs used for state and data.
pub fn resolve_config_root(
    suru_config_dir: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = suru_config_dir.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = xdg_config_home.filter(|dir| !dir.is_empty()) {
        let xdg = Path::new(xdg);
        // The XDG base-directory spec says a relative XDG_CONFIG_HOME is
        // invalid and must be ignored.
        if xdg.is_absolute() {
            return Some(xdg.join("suru"));
        }
    }
    home_dir.map(|home| home.join(".config").join("suru"))
}

/// The Config Document stack the server reads and — as the file's only writer
/// — edits. Cloneable so every request handler shares the one root and the one
/// writer, which makes each read–edit–write of the document the atomic unit
/// even while handlers run concurrently.
#[derive(Clone)]
pub struct ConfigDocuments {
    /// `None` when no config root resolved: Suru runs on built-in defaults and
    /// has nowhere to pin a Setting.
    root: Option<PathBuf>,
    writer: Arc<Mutex<()>>,
}

impl ConfigDocuments {
    pub fn new(root: Option<&Path>) -> Self {
        Self {
            root: root.map(Path::to_path_buf),
            writer: Arc::new(Mutex::new(())),
        }
    }

    /// Collapses the stack into the effective-settings snapshot.
    pub fn load(&self) -> SettingsSnapshot {
        load(self.root.as_deref())
    }

    /// Applies one typed mutation to the winning Config Document — creating
    /// the document, and any intermediate objects, when they are missing — and
    /// returns the snapshot the reloaded stack now yields.
    pub fn mutate(
        &self,
        mutation: &SettingMutation,
    ) -> Result<SettingsSnapshot, SettingsMutationError> {
        let Some(root) = self.root.as_deref() else {
            return Err(SettingsMutationError::NoConfigRoot);
        };
        let _writer = self
            .writer
            .lock()
            .expect("Config Document writer lock is not poisoned");
        let (key, value) = pin_for(mutation);
        write_pin(root, key, value.as_ref())?;
        Ok(load(Some(root)))
    }
}

/// Why a typed mutation could not reach the Config Document.
#[derive(Debug)]
pub enum SettingsMutationError {
    /// No config root resolved, so there is nowhere to pin a Setting.
    NoConfigRoot,
    /// The document on disk cannot take a surgical edit: it does not parse, or
    /// something that is not an object stands where a Setting's group belongs.
    /// Suru never rewrites a document to fix it.
    NotEditable { path: PathBuf, reason: String },
    /// The filesystem refused the read or the write the edit needed.
    Io { path: PathBuf, message: String },
}

impl fmt::Display for SettingsMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigRoot => write!(
                formatter,
                "no config root is configured, so there is nowhere to pin a Setting"
            ),
            Self::NotEditable { path, reason } => {
                write!(formatter, "Config Document {path:?} {reason}")
            }
            Self::Io { path, message } => {
                write!(formatter, "Config Document {path:?} {message}")
            }
        }
    }
}

impl std::error::Error for SettingsMutationError {}

/// The Setting a mutation targets, spelled as the schema's dotted key, and the
/// JSON that pins it — `None` to remove the pin.
fn pin_for(mutation: &SettingMutation) -> (&'static str, Option<Value>) {
    fn pinned<T: serde::Serialize>(value: &Option<T>) -> Option<Value> {
        value
            .as_ref()
            .map(|value| serde_json::to_value(value).expect("Setting values always serialize"))
    }
    match mutation {
        SettingMutation::TextSelectionCopy { value } => (TEXT_SELECTION_COPY, pinned(value)),
        SettingMutation::AppearanceTheme { value } => (APPEARANCE_THEME, pinned(value)),
        SettingMutation::AppearanceMode { value } => (APPEARANCE_MODE, pinned(value)),
        SettingMutation::AppearanceLandingPage { value } => {
            (APPEARANCE_LANDING_PAGE, pinned(value))
        }
        SettingMutation::AppearanceShowIcons { value } => (APPEARANCE_SHOW_ICONS, pinned(value)),
        SettingMutation::TranscriptDefaultFoldPosture { value } => {
            (TRANSCRIPT_DEFAULT_FOLD_POSTURE, pinned(value))
        }
        SettingMutation::TranscriptReasoningVisibility { value } => {
            (TRANSCRIPT_REASONING_VISIBILITY, pinned(value))
        }
        SettingMutation::TranscriptCommandAutoExpand { value } => {
            (TRANSCRIPT_COMMAND_AUTO_EXPAND, pinned(value))
        }
        SettingMutation::SessionContentWidth { value } => (SESSION_CONTENT_WIDTH, pinned(value)),
        SettingMutation::SessionTitleErrand { value } => (SESSION_TITLE_ERRAND, pinned(value)),
        SettingMutation::SessionTitleEmoji { value } => (SESSION_TITLE_EMOJI, pinned(value)),
        SettingMutation::SidebarInitialVisibility { value } => {
            (SIDEBAR_INITIAL_VISIBILITY, pinned(value))
        }
        SettingMutation::SidebarInitialWidth { value } => (SIDEBAR_INITIAL_WIDTH, pinned(value)),
        SettingMutation::SidebarInitialScope { value } => (SIDEBAR_INITIAL_SCOPE, pinned(value)),
        SettingMutation::SidebarAutoSettle { value } => (SIDEBAR_AUTO_SETTLE, pinned(value)),
        SettingMutation::ProviderCodexEnabled { value } => (PROVIDER_CODEX_ENABLED, pinned(value)),
        SettingMutation::ProviderCodexReasoningSummary { value } => {
            (PROVIDER_CODEX_REASONING_SUMMARY, pinned(value))
        }
        SettingMutation::ProviderCodexApprovalPolicy { value } => {
            (PROVIDER_CODEX_APPROVAL_POLICY, pinned(value))
        }
        SettingMutation::ProviderCodexSandboxMode { value } => {
            (PROVIDER_CODEX_SANDBOX_MODE, pinned(value))
        }
        SettingMutation::ProviderCopilotEnabled { value } => {
            (PROVIDER_COPILOT_ENABLED, pinned(value))
        }
        SettingMutation::ProviderCopilotPermissions { value } => {
            (PROVIDER_COPILOT_PERMISSIONS, pinned(value))
        }
        SettingMutation::ProviderClaudeEnabled { value } => {
            (PROVIDER_CLAUDE_ENABLED, pinned(value))
        }
        SettingMutation::ProviderClaudePermissionMode { value } => {
            (PROVIDER_CLAUDE_PERMISSION_MODE, pinned(value))
        }
        SettingMutation::ServingEnabled { value } => (SERVING_ENABLED, pinned(value)),
        SettingMutation::ServingPort { value } => (SERVING_PORT, pinned(value)),
        SettingMutation::ServingBindAddress { value } => (SERVING_BIND_ADDRESS, pinned(value)),
    }
}

/// Reads the winning Config Document, edits it, and writes it back. Parsing,
/// editing, and serializing happen in this one scope because CST handles are
/// not `Send`; an edit that changes nothing writes nothing.
fn write_pin(
    config_dir: &Path,
    key: &str,
    value: Option<&Value>,
) -> Result<(), SettingsMutationError> {
    // The document a pin lands in is the one the loader reads back: the
    // highest-precedence document of the stack, or the primary name when the
    // user has no Config Document yet.
    let path = discover_documents(config_dir, &mut Vec::new())
        .pop()
        .unwrap_or_else(|| config_dir.join(PRIMARY_CONFIG_FILE));
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(SettingsMutationError::Io {
                path,
                message: format!("could not be read: {error}"),
            });
        }
    };
    // A document that does not exist yet is edited as if it were an empty
    // object, so the first pin has somewhere to land. An edit that leaves that
    // object as empty as it found it — an unset of something never pinned —
    // writes nothing, so a reset on a fresh install creates no file.
    let before = existing.as_deref().unwrap_or(EMPTY_DOCUMENT);
    let edited = edited_document(before, &path, key, value)?;
    if edited == before {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| SettingsMutationError::Io {
            path: path.clone(),
            message: format!("could not be created: its directory does not exist ({error})"),
        })?;
    }
    fs::write(&path, edited).map_err(|error| SettingsMutationError::Io {
        path,
        message: format!("could not be written: {error}"),
    })
}

/// The document's text after the edit: the pin written where the schema's key
/// path says it belongs, or removed from there, with every other byte of the
/// document left as its author wrote it.
fn edited_document(
    text: &str,
    path: &Path,
    key: &str,
    value: Option<&Value>,
) -> Result<String, SettingsMutationError> {
    let not_editable = |reason: &str| SettingsMutationError::NotEditable {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    };
    let document = CstRootNode::parse(text, &parse_options())
        .map_err(|error| not_editable(&format!("is not valid JSONC: {error}")))?;
    let mut object = match document.object_value() {
        Some(object) => object,
        // A document holding nothing but comments still has room for a pin.
        None if document.value().is_none() => document.object_value_or_set(),
        None => return Err(not_editable("does not hold an object at its top level")),
    };
    let mut names = key.split('.').collect::<Vec<_>>();
    let name = names.pop().expect("every schema key names a Setting");
    // Each group on the way in, with the object holding it, so an unset can
    // take the groups that existed only for the pin it removes.
    let mut groups: Vec<(CstObject, &str, CstObject)> = Vec::new();
    for group in names {
        let holder = object.clone();
        object = match value {
            Some(_) => holder.object_value_or_create(group).ok_or_else(|| {
                not_editable(&format!(
                    "holds something other than an object at {group:?}, where this Setting belongs"
                ))
            })?,
            // Nothing to unset below a group the document never wrote.
            None => match holder.object_value(group) {
                Some(group) => group,
                None => return Ok(text.to_owned()),
            },
        };
        groups.push((holder, group, object.clone()));
    }
    match value {
        Some(value) => match object.get(name) {
            Some(property) => property.set_value(input_value(value)),
            None => {
                object.append(name, input_value(value));
            }
        },
        None => {
            let Some(property) = object.get(name) else {
                return Ok(text.to_owned());
            };
            property.remove();
            // A group that held only the removed pin goes with it, so the
            // document stays as sparse as the user's own hand would keep it. A
            // group still holding a comment stays: that comment is the
            // author's to remove, not Suru's.
            for (holder, group_name, group) in groups.into_iter().rev() {
                let is_spent = group.properties().is_empty()
                    && !group.children().iter().any(CstNode::is_comment);
                if !is_spent {
                    break;
                }
                if let Some(property) = holder.get(group_name) {
                    property.remove();
                }
            }
        }
    }
    Ok(document.to_string())
}

fn input_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input_value).collect()),
        Value::Object(entries) => CstInputValue::Object(
            entries
                .iter()
                .map(|(name, value)| (name.clone(), input_value(value)))
                .collect(),
        ),
    }
}

/// JSONC per the spec: comments and trailing commas, nothing looser. The
/// loader and the editor read every document the same way.
fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

/// Loads the Config Document stack under `config_dir` and collapses it into
/// the effective-settings snapshot. `None` — no configured root — loads pure
/// defaults, which is also what any broken document degrades to.
fn load(config_dir: Option<&Path>) -> SettingsSnapshot {
    let mut snapshot = SettingsSnapshot {
        settings: EffectiveSettings::default(),
        pinned: Vec::new(),
        diagnostics: Vec::new(),
    };
    let Some(config_dir) = config_dir else {
        return snapshot;
    };
    for document in discover_documents(config_dir, &mut snapshot.diagnostics) {
        apply_document(&document, &mut snapshot);
    }
    snapshot
}

/// Emits every diagnostic to the Log at the severity it carries.
pub fn log_diagnostics(diagnostics: &[SettingsDiagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.severity {
            SettingsDiagnosticSeverity::Error => tracing::error!(
                file = %diagnostic.file.display(),
                key = diagnostic.key.as_deref(),
                "configuration problem: {}",
                diagnostic.message
            ),
            SettingsDiagnosticSeverity::Warning => tracing::warn!(
                file = %diagnostic.file.display(),
                key = diagnostic.key.as_deref(),
                "configuration problem: {}",
                diagnostic.message
            ),
        }
    }
}

/// The ordered Config Document stack under one root, lowest precedence first.
/// Depth one today: `suru.jsonc`, or `suru.json` when it is the only file,
/// with a diagnostic when both exist and the fallback is ignored.
fn discover_documents(
    config_dir: &Path,
    diagnostics: &mut Vec<SettingsDiagnostic>,
) -> Vec<PathBuf> {
    let primary = config_dir.join(PRIMARY_CONFIG_FILE);
    let fallback = config_dir.join(FALLBACK_CONFIG_FILE);
    match (primary.is_file(), fallback.is_file()) {
        (true, true) => {
            diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Warning,
                file: fallback,
                key: None,
                message: format!("ignored because {PRIMARY_CONFIG_FILE} exists and wins"),
            });
            vec![primary]
        }
        (true, false) => vec![primary],
        (false, true) => vec![fallback],
        (false, false) => Vec::new(),
    }
}

fn apply_document(path: &Path, snapshot: &mut SettingsSnapshot) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: format!("ignored because it could not be read: {error}"),
            });
            return;
        }
    };
    let root = match jsonc_parser::parse_to_serde_value(&text, &parse_options()) {
        Ok(root) => root,
        Err(error) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: format!("ignored because it is not valid JSONC: {error}"),
            });
            return;
        }
    };
    match root {
        None | Some(Value::Null) => {}
        Some(Value::Object(entries)) => {
            for (name, value) in &entries {
                apply_key(path, "", name, value, snapshot);
            }
        }
        Some(_) => {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Error,
                file: path.to_path_buf(),
                key: None,
                message: "ignored because its top level is not an object".to_owned(),
            });
        }
    }
}

fn apply_key(
    path: &Path,
    prefix: &str,
    name: &str,
    value: &Value,
    snapshot: &mut SettingsSnapshot,
) {
    let key = if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    };
    // A property name containing a dot would collide with the nested spelling
    // the schema defines; admitting it as a second spelling would leave pins
    // the future format-preserving editor cannot target.
    if name.contains('.') {
        snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because Settings nest as objects, not dotted names".to_owned(),
        });
        return;
    }
    if let Some(descriptor) = SCHEMA.iter().find(|descriptor| descriptor.key == key) {
        if (descriptor.apply)(&mut snapshot.settings, value) {
            if !snapshot.pinned.contains(&key) {
                snapshot.pinned.push(key);
            }
        } else {
            snapshot.diagnostics.push(SettingsDiagnostic {
                severity: SettingsDiagnosticSeverity::Warning,
                file: path.to_path_buf(),
                key: Some(key),
                message: format!("ignored because its value is not {}", descriptor.expected()),
            });
        }
        return;
    }
    let is_group = SCHEMA
        .iter()
        .any(|descriptor| descriptor.key.starts_with(&format!("{key}.")));
    if !is_group {
        snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because it is not a known Setting".to_owned(),
        });
        return;
    }
    match value {
        Value::Object(entries) => {
            for (name, value) in entries {
                apply_key(path, &key, name, value, snapshot);
            }
        }
        _ => snapshot.diagnostics.push(SettingsDiagnostic {
            severity: SettingsDiagnosticSeverity::Warning,
            file: path.to_path_buf(),
            key: Some(key),
            message: "ignored because it should be an object of Settings".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::TranscriptSettings;

    /// The JSON a pin writes into the Config Document, which is what the
    /// loader will read back and what a client shows.
    fn pinned_value(mutation: &SettingMutation) -> Value {
        serde_json::to_value(mutation).expect("Setting mutations always serialize")["value"].clone()
    }

    /// One Setting written twice, once in each shape, so that a guard reading
    /// both is reading the same Setting and never two that differ in some
    /// second way. It borrows the Fold posture Setting's key, typed field, and
    /// pins, so each stand-in pins, applies, and resets exactly as the shipped
    /// Setting does; only what it says it accepts differs.
    const fn posture_stand_in(values: SettingValues) -> SettingDescriptor {
        SettingDescriptor {
            key: TRANSCRIPT_DEFAULT_FOLD_POSTURE,
            label: "Default Fold posture",
            description: "How a Session view opens",
            group: SettingGroup::General,
            scope: SettingScope::Client,
            values,
            reset: SettingMutation::TranscriptDefaultFoldPosture { value: None },
            apply: |settings, value| {
                apply_value(value, |posture| {
                    settings.transcript.default_fold_posture = posture;
                })
            },
        }
    }

    /// The one posture both stand-ins name, leaving the other a value one of
    /// them must describe and the other cannot express at all.
    const FOLDED: &[SettingChoice] = &[SettingChoice {
        value: "folded",
        build_mutation: || SettingMutation::TranscriptDefaultFoldPosture {
            value: Some(FoldPosture::Folded),
        },
    }];

    /// A Setting shaped the way a fixed choice list cannot express one: part of
    /// what it holds the schema can name, and the rest it can only describe. No
    /// shipped Setting is open yet — the first is the one this shape was added
    /// for — so the guards below run over this stand-in as well, and cover the
    /// shape rather than only the Settings that happen to ship today.
    static OPEN_STAND_IN: SettingDescriptor = posture_stand_in(SettingValues::Open {
        named: FOLDED,
        accepts: "a posture Suru worked out for itself",
        spell: |settings| match settings.transcript.default_fold_posture {
            FoldPosture::Folded => "folded".to_owned(),
            FoldPosture::Expanded => "expanded".to_owned(),
        },
        // A posture Suru worked out is nothing a reader picks, which is the
        // half of the Open shape the shipped Setting does not cover.
        chosen_at: None,
    });

    /// The same Setting written the only way the schema could write one before
    /// an Open Setting was possible: its choices are the whole of what it
    /// accepts, so a value outside them is one it can neither name nor spell.
    static FIXED_STAND_IN: SettingDescriptor = posture_stand_in(SettingValues::Fixed(FOLDED));

    /// Every Setting the generic guards run over: the schema itself, and the
    /// stand-in for the shape no shipped Setting has yet.
    fn every_setting() -> impl Iterator<Item = &'static SettingDescriptor> {
        SCHEMA.iter().chain(std::iter::once(&OPEN_STAND_IN))
    }

    /// A choice is shown as the Config Document spells it, with JSON's own
    /// quoting elided: a string choice is its contents, and any other JSON
    /// value — a boolean above all — is its literal spelling.
    #[test]
    fn every_choice_is_spelled_the_way_the_config_document_spells_it() {
        for descriptor in every_setting() {
            for choice in descriptor.values.named() {
                let spelled = match pinned_value(&choice.mutation()) {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                assert_eq!(
                    spelled, choice.value,
                    "{} offers {:?} but pins something else",
                    descriptor.key, choice.value
                );
            }
        }
    }

    #[test]
    fn every_choice_a_setting_offers_becomes_the_effective_value_it_names() {
        for descriptor in every_setting() {
            for choice in descriptor.values.named() {
                let mut settings = EffectiveSettings::default();
                assert!(
                    (descriptor.apply)(&mut settings, &pinned_value(&choice.mutation())),
                    "{} rejects its own choice {:?}",
                    descriptor.key,
                    choice.value
                );
                assert_eq!(
                    descriptor.effective(&settings).map(|choice| choice.value),
                    Some(choice.value),
                    "{} does not read back the choice it just took",
                    descriptor.key
                );
            }
        }
    }

    /// A built-in default is a value the Setting can say out loud: a named
    /// choice for a Fixed Setting, which has nothing else to hold, and either
    /// that or a spelling of its own for an Open one.
    #[test]
    fn every_setting_reads_a_value_for_its_built_in_default() {
        let defaults = EffectiveSettings::default();
        for descriptor in every_setting() {
            assert!(
                descriptor.spelling(&defaults).is_some(),
                "{} defaults to a value it can neither name nor spell",
                descriptor.key
            );
        }
    }

    #[test]
    fn cycling_a_setting_walks_its_choices_in_order_and_wraps_past_the_last() {
        for descriptor in every_setting() {
            let mut settings = EffectiveSettings::default();
            let mut walked = Vec::new();
            for _ in 0..descriptor.values.named().len() {
                let next = descriptor
                    .next_choice(&settings)
                    .expect("a Setting always offers somewhere to step");
                assert!(
                    (descriptor.apply)(&mut settings, &pinned_value(&next.mutation())),
                    "{} rejects the choice it stepped to",
                    descriptor.key
                );
                walked.push(next.value);
            }
            let offered = descriptor
                .values
                .named()
                .iter()
                .map(|choice| choice.value)
                .collect::<Vec<_>>();
            assert_eq!(
                walked.len(),
                offered.len(),
                "{} does not return to where it started",
                descriptor.key
            );
            for value in &offered {
                assert!(
                    walked.contains(value),
                    "{} never steps onto {value:?}",
                    descriptor.key
                );
            }
        }
    }

    #[test]
    fn a_rejected_value_is_diagnosed_with_the_values_the_setting_accepts() {
        let expected = SCHEMA
            .iter()
            .map(SettingDescriptor::expected)
            .collect::<Vec<_>>();
        assert_eq!(
            expected,
            vec![
                "one of \"system\" or a Theme name".to_owned(),
                "one of \"system\", \"dark\", or \"light\"".to_owned(),
                "one of \"Minimal\" or \"Fancy\"".to_owned(),
                "one of false or true".to_owned(),
                "one of \"folded\" or \"expanded\"".to_owned(),
                "one of \"hidden\" or \"shown\"".to_owned(),
                "one of false or a whole number of milliseconds".to_owned(),
                "one of \"fill\" or an integer of at least 50".to_owned(),
                // A Setting the schema can only partly enumerate names what it
                // can and describes the rest, in the same breath.
                "one of \"session\", \"off\", or an Agent Selection".to_owned(),
                "one of \"shown\" or \"hidden\"".to_owned(),
                "an integer of at least 24".to_owned(),
                "one of \"all_workspaces\", \"current_workspace\", or \"everywhere\"".to_owned(),
                "one of \"off\" or a whole number of days, at least 1".to_owned(),
                // A boolean Setting is diagnosed as accepting `true` or
                // `false`, unquoted, because that is what the reader must type.
                "one of \"release\" or \"manual\"".to_owned(),
                "one of true or false".to_owned(),
                "one of \"auto\", \"concise\", \"detailed\", or \"none\"".to_owned(),
                "one of true or false".to_owned(),
                "one of true or false".to_owned(),
                "one of \"hidden\" or \"shown\"".to_owned(),
                "one of false or true".to_owned(),
                "a port from 0 to 65535".to_owned(),
                "one of \"127.0.0.1\", \"::1\", \"0.0.0.0\", \"::\", or an IP address".to_owned(),
            ]
        );
    }

    /// A Setting the schema cannot enumerate has no word to tell the reader to
    /// type for part of what it accepts, so the diagnostic names what kind of
    /// thing belongs there instead — beside every value it can still spell out.
    #[test]
    fn a_setting_the_schema_cannot_enumerate_is_diagnosed_with_what_it_accepts() {
        assert_eq!(
            OPEN_STAND_IN.expected(),
            "one of \"folded\" or a posture Suru worked out for itself"
        );
    }

    #[test]
    fn command_auto_expansion_opens_at_five_hundred_milliseconds_and_pins_the_delay() {
        let descriptor = SCHEMA
            .iter()
            .find(|descriptor| descriptor.key == TRANSCRIPT_COMMAND_AUTO_EXPAND)
            .expect("the Command auto-expansion Setting is defined");
        let Some(SettingChoiceSurface::Numeric(choice)) = descriptor.chosen_at() else {
            panic!("the auto-expansion delay is chosen at the numeric editor");
        };
        let mut settings = EffectiveSettings::default();

        assert_eq!(descriptor.spelling(&settings).as_deref(), Some("false"));
        assert_eq!(
            choice.seed(&settings),
            CommandAutoExpand::DEFAULT_MILLIS.to_string(),
            "enabling auto-expansion starts at the suggested 500ms delay"
        );

        let mutation = choice.accept("275").expect("a millisecond delay is valid");
        assert!((descriptor.apply)(&mut settings, &pinned_value(&mutation)));
        assert_eq!(
            settings.transcript.command_auto_expand,
            CommandAutoExpand::AfterMillis(275)
        );
        assert_eq!(descriptor.spelling(&settings).as_deref(), Some("275ms"));
    }

    /// The reading a surface draws a Setting by: a named choice while it is on
    /// one, and the Setting's own words for a value the schema never named.
    #[test]
    fn an_open_setting_spells_the_value_it_holds_even_where_it_names_none() {
        let mut settings = EffectiveSettings::default();
        assert_eq!(OPEN_STAND_IN.spelling(&settings).as_deref(), Some("folded"));

        settings.transcript.default_fold_posture = FoldPosture::Expanded;
        assert_eq!(
            OPEN_STAND_IN
                .effective(&settings)
                .map(|choice| choice.value),
            None,
            "a value the Setting names no choice for is on no choice"
        );
        assert_eq!(
            OPEN_STAND_IN.spelling(&settings).as_deref(),
            Some("expanded"),
            "and the Setting spells it for whatever is drawing it"
        );
        assert_eq!(
            OPEN_STAND_IN
                .next_choice(&settings)
                .map(|choice| choice.value),
            Some("folded"),
            "cycling off a value with no neighbours lands on the first named one"
        );
    }

    /// A Fixed Setting's choices are the whole of what it accepts, so a value
    /// outside them is a schema that fell behind its own types. It reads as
    /// nothing at all rather than as a value the Setting made up, which is what
    /// leaves a client free to say so.
    #[test]
    fn a_fixed_setting_spells_nothing_for_a_value_it_does_not_name() {
        let settings = EffectiveSettings {
            transcript: TranscriptSettings {
                default_fold_posture: FoldPosture::Expanded,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            FIXED_STAND_IN
                .effective(&settings)
                .map(|choice| choice.value),
            None
        );
        assert_eq!(FIXED_STAND_IN.spelling(&settings), None);
    }

    /// The Setting the Open shape was added for holds an Agent Selection the
    /// reader picks, which is the half of that shape the stand-in above cannot
    /// cover: the value has no word to cycle onto, so the Setting names the
    /// surface it is chosen at and the pin that choice becomes.
    #[test]
    fn a_pinned_agent_selection_reads_back_as_the_selection_the_reader_chose() {
        let descriptor = SCHEMA
            .iter()
            .find(|descriptor| descriptor.key == SESSION_TITLE_ERRAND)
            .expect("the Title derivation Setting is defined");
        let chosen = AgentSelection {
            provider: crate::protocol::ProviderId::new("codex"),
            model: crate::protocol::ModelId::new("gpt-5-mini"),
            options: Vec::new(),
        };
        let Some(SettingChoiceSurface::AgentSelection { current, pin }) = descriptor.chosen_at()
        else {
            panic!("an Agent Selection is chosen at the Model picker");
        };

        let mut settings = EffectiveSettings::default();
        assert_eq!(
            current(&settings),
            None,
            "a Setting holding one of its named values opens the picker on nothing in particular"
        );
        assert!((descriptor.apply)(
            &mut settings,
            &pinned_value(&pin(chosen.clone()))
        ));

        assert_eq!(
            settings.session.title.errand,
            TitleErrand::Pinned(chosen.clone()),
            "the pin puts the reader's own Selection in force"
        );
        assert_eq!(
            current(&settings),
            Some(chosen.clone()),
            "and reopening the picker lands on the Model they chose"
        );
        assert_eq!(
            descriptor.effective(&settings).map(|choice| choice.value),
            None,
            "a pinned Selection is none of the values the schema names"
        );
        assert_eq!(
            descriptor.spelling(&settings).as_deref(),
            Some("codex · gpt-5-mini"),
            "so the Setting spells it for whatever is drawing it"
        );
        assert_eq!(
            descriptor.next_choice(&settings).map(|choice| choice.value),
            Some("session"),
            "and cycling off it returns to the first value the schema names"
        );
    }

    /// Half of the guard that a Provider cannot ship without a working
    /// Enablement: every `provider.<id>.enabled` entry must actually reach the
    /// gate that reads it. The other half — that every built-in Provider has
    /// such an entry at all — lives beside the hosted Provider list in
    /// `server`, because that is the list which grows.
    #[test]
    fn every_provider_enablement_setting_reaches_the_gate_that_reads_it() {
        let mut checked = 0;
        for descriptor in SCHEMA {
            let Some(provider) = descriptor
                .key
                .strip_prefix("provider.")
                .and_then(|rest| rest.strip_suffix(".enabled"))
                .map(crate::protocol::ProviderId::new)
            else {
                continue;
            };
            let mut settings = EffectiveSettings::default();
            assert!(
                settings.provider_enabled(&provider),
                "{} must leave its Provider on unless the user says otherwise",
                descriptor.key
            );
            assert!(
                (descriptor.apply)(&mut settings, &Value::Bool(false)),
                "{} must accept a JSON boolean",
                descriptor.key
            );
            assert!(
                !settings.provider_enabled(&provider),
                "{} does not reach the gate that reads Provider `{provider}`'s Enablement",
                descriptor.key
            );
            checked += 1;
        }
        assert!(checked > 0, "no Provider Enablement Setting is defined");
    }

    #[test]
    fn resetting_a_setting_unpins_the_key_that_setting_owns() {
        for descriptor in every_setting() {
            let (key, value) = pin_for(&descriptor.reset);
            assert_eq!(key, descriptor.key);
            assert_eq!(value, None, "a reset never writes a value");
        }
    }

    /// An absolute directory on the platform under test. Absoluteness is
    /// platform-defined — a POSIX `/home/user` is relative to Windows — and
    /// [`resolve_config_root`] turns on exactly that judgement, so the
    /// fixtures have to be rooted the way the running platform roots paths.
    #[cfg(windows)]
    const OVERRIDE_DIR: &str = r"C:\tmp\override";
    #[cfg(windows)]
    const XDG_CONFIG_HOME: &str = r"C:\Users\user\.xdg";
    #[cfg(windows)]
    const HOME_DIR: &str = r"C:\Users\user";

    #[cfg(not(windows))]
    const OVERRIDE_DIR: &str = "/tmp/override";
    #[cfg(not(windows))]
    const XDG_CONFIG_HOME: &str = "/home/user/.xdg";
    #[cfg(not(windows))]
    const HOME_DIR: &str = "/home/user";

    #[test]
    fn suru_config_dir_overrides_every_other_config_root() {
        let root = resolve_config_root(
            Some(OsStr::new(OVERRIDE_DIR)),
            Some(OsStr::new(XDG_CONFIG_HOME)),
            Some(Path::new(HOME_DIR)),
        );
        assert_eq!(root, Some(PathBuf::from(OVERRIDE_DIR)));
    }

    #[test]
    fn xdg_config_home_hosts_the_suru_directory() {
        let root = resolve_config_root(
            None,
            Some(OsStr::new(XDG_CONFIG_HOME)),
            Some(Path::new(HOME_DIR)),
        );
        assert_eq!(root, Some(Path::new(XDG_CONFIG_HOME).join("suru")));
    }

    #[test]
    fn relative_or_empty_xdg_config_home_falls_back_to_the_home_config_directory() {
        for invalid in ["relative/config", ""] {
            let root =
                resolve_config_root(None, Some(OsStr::new(invalid)), Some(Path::new(HOME_DIR)));
            assert_eq!(root, Some(Path::new(HOME_DIR).join(".config").join("suru")));
        }
    }

    #[test]
    fn without_any_environment_there_is_no_config_root() {
        assert_eq!(resolve_config_root(None, None, None), None);
    }
}
