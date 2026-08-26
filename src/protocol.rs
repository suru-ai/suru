use std::{
    fmt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 22;
pub const SERVER_SHUTDOWN_EVENT: &str = "server_shutdown";
pub const SETTINGS_SNAPSHOT_EVENT: &str = "settings_snapshot";
pub const SKILL_CATALOG_UPDATED_EVENT: &str = "skill_catalog_updated";
pub const SESSION_CATALOG_SNAPSHOT_EVENT: &str = "session_catalog_snapshot";
pub const SESSION_CATALOG_UPDATED_EVENT: &str = "session_catalog_updated";
pub const SESSION_SNAPSHOT_EVENT: &str = "session_snapshot";
pub const SESSION_UPDATED_EVENT: &str = "session_updated";

macro_rules! session_identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

session_identity!(SessionId);
session_identity!(PromptId);
session_identity!(TurnId);
session_identity!(MessageId);
session_identity!(ActivityId);
session_identity!(AgentSelectionOperationId);

macro_rules! named_identity {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

named_identity!(AgentId);
named_identity!(ProviderId);
named_identity!(ModelId);
named_identity!(ModelOptionId);
named_identity!(ModelOptionChoiceId);
named_identity!(SkillId);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOptionRole {
    ReasoningEffort,
    Speed,
    Context,
    Verbosity,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionChoice {
    pub id: ModelOptionChoiceId,
    pub label: String,
    pub description: Option<String>,
    pub availability: ModelAvailability,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelOptionKind {
    Select {
        choices: Vec<ModelOptionChoice>,
        default: ModelOptionChoiceId,
    },
    Toggle {
        default: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionDescriptor {
    pub id: ModelOptionId,
    pub label: String,
    pub description: Option<String>,
    pub role: ModelOptionRole,
    pub kind: ModelOptionKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub id: ModelId,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub availability: ModelAvailability,
    pub options: Vec<ModelOptionDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentSelectionMaterializationError {
    MissingOption { option: ModelOptionId },
    DuplicateOption { option: ModelOptionId },
    UnknownOption { option: ModelOptionId },
    InvalidOptionValue { option: ModelOptionId },
}

impl fmt::Display for AgentSelectionMaterializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOption { option } => {
                write!(formatter, "Model Option `{option}` is missing")
            }
            Self::DuplicateOption { option } => {
                write!(
                    formatter,
                    "Model Option `{option}` is selected more than once"
                )
            }
            Self::UnknownOption { option } => {
                write!(formatter, "Model Option `{option}` is unknown")
            }
            Self::InvalidOptionValue { option } => {
                write!(
                    formatter,
                    "Model Option `{option}` has no such available value"
                )
            }
        }
    }
}

impl std::error::Error for AgentSelectionMaterializationError {}

impl ModelDescriptor {
    pub fn default_agent_selection(&self) -> AgentSelection {
        AgentSelection {
            provider: self.provider.clone(),
            model: self.id.clone(),
            options: self
                .options
                .iter()
                .map(ModelOptionDescriptor::default_selection)
                .collect(),
        }
    }

    pub fn materialize_agent_selection(
        &self,
        current: Option<&AgentSelection>,
    ) -> Result<AgentSelection, AgentSelectionMaterializationError> {
        let Some(current) = current
            .filter(|selection| selection.provider == self.provider && selection.model == self.id)
        else {
            return Ok(self.default_agent_selection());
        };
        let options = self
            .options
            .iter()
            .map(|descriptor| {
                let mut matching = current
                    .options
                    .iter()
                    .filter(|selection| selection.id == descriptor.id);
                let selection = matching.next().ok_or_else(|| {
                    AgentSelectionMaterializationError::MissingOption {
                        option: descriptor.id.clone(),
                    }
                })?;
                if matching.next().is_some() {
                    return Err(AgentSelectionMaterializationError::DuplicateOption {
                        option: descriptor.id.clone(),
                    });
                }
                if !descriptor.value_is_available(&selection.value) {
                    return Err(AgentSelectionMaterializationError::InvalidOptionValue {
                        option: descriptor.id.clone(),
                    });
                }
                Ok(selection.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(selection) = current.options.iter().find(|selection| {
            !self
                .options
                .iter()
                .any(|descriptor| descriptor.id == selection.id)
        }) {
            return Err(AgentSelectionMaterializationError::UnknownOption {
                option: selection.id.clone(),
            });
        }
        Ok(AgentSelection {
            provider: self.provider.clone(),
            model: self.id.clone(),
            options,
        })
    }
}

impl ModelOptionDescriptor {
    fn default_selection(&self) -> ModelOptionSelection {
        ModelOptionSelection {
            id: self.id.clone(),
            value: match &self.kind {
                ModelOptionKind::Select { default, .. } => ModelOptionValue::Select {
                    choice: default.clone(),
                },
                ModelOptionKind::Toggle { default } => {
                    ModelOptionValue::Toggle { enabled: *default }
                }
            },
        }
    }

    fn value_is_available(&self, value: &ModelOptionValue) -> bool {
        match (&self.kind, value) {
            (ModelOptionKind::Toggle { .. }, ModelOptionValue::Toggle { .. }) => true,
            (ModelOptionKind::Select { choices, .. }, ModelOptionValue::Select { choice }) => {
                choices.iter().any(|candidate| {
                    candidate.id == *choice
                        && candidate.availability == ModelAvailability::Available
                })
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelOptionValue {
    Select { choice: ModelOptionChoiceId },
    Toggle { enabled: bool },
}

/// Why a Provider cannot be used right now. Each reason names a condition the
/// user fixes outside Suru — installing the Provider's CLI, signing in to it,
/// or moving to a version Suru speaks — which the next catalog refresh
/// re-evaluates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUnavailability {
    NotInstalled,
    NotSignedIn,
    IncompatibleVersion,
}

impl ProviderUnavailability {
    /// The reason as a client states it to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::NotSignedIn => "not signed in",
            Self::IncompatibleVersion => "incompatible version",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCatalogStatus {
    Fresh,
    Refreshing,
    Stale {
        message: String,
    },
    Failed {
        message: String,
    },
    /// The Provider cannot be used at all until the user fixes `reason`
    /// outside Suru. Whatever Models the catalog still holds stay listed so
    /// the Provider keeps its place, but none of them may be selected.
    Unavailable {
        reason: ProviderUnavailability,
        message: String,
    },
    /// The user turned this Provider off, so Suru never consulted it: it lists
    /// no Models because none were asked for, and it reports no condition
    /// because none was looked for.
    ///
    /// Deliberately a sibling of `Unavailable` rather than a fourth
    /// [`ProviderUnavailability`] reason. That enum's contract is a condition
    /// the user fixes *outside* Suru and which the next catalog refresh
    /// re-evaluates; Enablement is neither — it is a Setting the user changes
    /// inside Suru and it takes effect on the next settings snapshot. Folding
    /// it in would make that enum's own documentation false and would wire
    /// recovery through a refresh Enablement does not need.
    ///
    /// Like `Unavailable`, it outranks every other status — a refresh
    /// notionally in flight included — because nothing about a Provider Suru
    /// never consulted may be offered.
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelCatalog {
    pub provider: ProviderId,
    /// The Provider's name as the user reads it, declared by its runtime —
    /// "Codex" rather than `codex`. Rides the catalog so a client showing what
    /// the catalog holds prints the runtime's own name for a Provider without
    /// having to ask what the runtimes are.
    pub display_name: String,
    pub models: Vec<ModelDescriptor>,
    pub status: ProviderCatalogStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub providers: Vec<ProviderModelCatalog>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

impl SessionRevision {
    pub const INITIAL: Self = Self(1);

    pub fn immediately_follows(self, previous: Self) -> bool {
        previous.0.checked_add(1) == Some(self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionCatalogRevision(pub u64);

impl SessionCatalogRevision {
    pub const INITIAL: Self = Self(1);

    pub fn immediately_follows(self, previous: Self) -> bool {
        previous.0.checked_add(1) == Some(self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PromptOrder(pub u64);

impl PromptOrder {
    pub const INITIAL: Self = Self(1);
}

/// Milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionTimestamp(pub u64);

impl SessionTimestamp {
    /// The moment now, read the way a Session's own timestamps are stamped, so
    /// anything measuring how long ago one of them was is measuring against the
    /// same clock they were written from.
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: PathBuf,
}

/// Safe presentation metadata for one user-invocable Skill. The Provider keeps
/// every native path, command name, and configuration detail behind its own
/// runtime boundary; clients receive only this opaque identity and the words
/// they need to choose and distinguish the Skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDescriptor {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    pub scope: Option<String>,
}

/// A delivery context in which a Provider can honor Skill Invocations. Initial
/// is distinct from Steer even though an initial Prompt currently enters the
/// Session store with steer delivery: Providers such as Claude support one and
/// not the other.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPromptDelivery {
    Initial,
    Queue,
    Steer,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalogCapabilities {
    /// The maximum number of distinct Skills one Prompt may invoke. None means
    /// the Provider declares no Suru-enforced limit.
    pub max_distinct_invocations: Option<u32>,
    pub supported_deliveries: Vec<SkillPromptDelivery>,
}

/// Whether the entries in a Skill Catalog are current invocation authority.
/// Skill discovery is deliberately independent of Provider Availability: a
/// failed or stale Skill Catalog never prevents an ordinary Prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillCatalogStatus {
    Loading,
    Fresh { warning: Option<String> },
    Refreshing,
    Stale { message: String },
    Unavailable { message: String },
}

/// The effective user-invocable Skills offered by one Provider in exactly one
/// Workspace. Including both identities in the value makes crossing Provider
/// or Workspace contexts visible at every caller boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalog {
    pub provider: ProviderId,
    pub workspace: Workspace,
    pub skills: Vec<SkillDescriptor>,
    pub capabilities: SkillCatalogCapabilities,
    pub status: SkillCatalogStatus,
}

/// The client context whose current Skill Catalog it wants. The server
/// canonicalizes the Workspace before consulting the Provider, so spelling
/// variants of one directory cannot create separate authority domains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalogRequest {
    pub provider: ProviderId,
    pub workspace: Workspace,
}

/// The byte range occupied by one recognized `$skill-name` marker in the
/// original UTF-8 Prompt. The end is exclusive, matching Rust string ranges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillMarkerSpan {
    pub start: u32,
    pub end: u32,
}

pub(crate) fn skill_names_equal(left: &str, right: &str) -> bool {
    left.chars()
        .flat_map(char::to_lowercase)
        .eq(right.chars().flat_map(char::to_lowercase))
}

pub(crate) fn skill_marker_matches(marker: &str, name: &str) -> bool {
    marker
        .strip_prefix('$')
        .is_some_and(|visible| skill_names_equal(visible, name))
}

/// A safe binding between visible Prompt text and one Provider-owned Skill.
/// It deliberately carries no Provider-native identifier. Repeated markers
/// remain separate records so historical presentation preserves every marker;
/// Provider lowering may later deduplicate identities in first-appearance
/// order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInvocation {
    pub skill_id: SkillId,
    pub name: String,
    pub scope: Option<String>,
    pub marker: SkillMarkerSpan,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionSelection {
    pub id: ModelOptionId,
    pub value: ModelOptionValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSelection {
    pub provider: ProviderId,
    pub model: ModelId,
    pub options: Vec<ModelOptionSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    pub agent: AgentId,
    pub selection: AgentSelection,
}

/// Where a Setting applies: a Client Setting governs a client's presentation,
/// a Server Setting governs server or Provider behavior. Dormant data until a
/// machine-local overlay distinguishes the two, but declared from day one so
/// that overlay never reshapes the schema.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    Client,
    Server,
}

/// The default Fold posture a Session view opens with.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldPosture {
    #[default]
    Folded,
    Expanded,
}

/// How much Reasoning summary detail a Turn requests from Codex.
///
/// Codex resolves a Turn's summary detail as what the Turn asked for or,
/// failing that, the Model's own default — and every Model in the current
/// catalog ships that default as `none`. A Turn that states no preference
/// therefore gets Reasoning with no summary at all: nothing streams and the
/// completed block carries an empty summary. Suru's default is `auto`, which
/// lets the Model choose how much to say.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryDetail {
    #[default]
    Auto,
    Concise,
    Detailed,
    None,
}

/// Whether a Transcript draws the Reasoning a Session stored.
///
/// Hidden is the built-in default: thinking is the agent's working-out, and a
/// Transcript leads with the work and the answer rather than the account of
/// how the agent got there. Showing it is a reader's deliberate choice.
///
/// Either way it is presentation and nothing more: the blocks keep arriving,
/// keep being stored, and keep being what a Provider was asked for, so a
/// reader who turns Reasoning on is shown every block that arrived while it
/// was off. Distinct from the `provider.codex.reasoningSummary` Setting, which
/// decides how much Reasoning a Turn asks for in the first place.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVisibility {
    #[default]
    Hidden,
    Shown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSettings {
    pub default_fold_posture: FoldPosture,
    pub reasoning_visibility: ReasoningVisibility,
}

/// Which Agent Selection derives a Session's Title, which is also whether Suru
/// derives one at all.
///
/// One Setting rather than two, because two would admit a state that
/// contradicts itself — titling turned off while a Model stands pinned for it —
/// and would give the settings panel two rows for one intent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TitleErrand {
    /// The built-in default: each Session's Title is derived by the Provider
    /// that Session already uses, at that Provider's Errand Selection. A
    /// Session that has selected no Provider is left alone, because Suru will
    /// not pick one the user did not choose.
    #[default]
    FollowSession,
    /// Suru derives no Titles and makes no Provider call on its own behalf.
    Off,
    /// Every Session's Title is derived by this Provider and Model, whatever
    /// the Session itself uses — including a Session that uses nothing. The
    /// Selection is resolved against the live Model catalog like any other
    /// Errand Selection, so a Model that has gone gives way to that Provider's
    /// default rather than failing the Errand.
    Pinned(AgentSelection),
}

impl TitleErrand {
    /// The word a Config Document spells this value with, and `None` for the
    /// one value no word can spell. This is the only place those words are
    /// written down: serializing reads them off here, deserializing matches
    /// against them, and anything drawing the Setting spells a named value the
    /// way a reader would have typed it.
    pub fn named(&self) -> Option<&'static str> {
        match self {
            Self::FollowSession => Some("session"),
            Self::Off => Some("off"),
            Self::Pinned(_) => None,
        }
    }
}

/// How a Config Document spells a [`TitleErrand`]: one of the two words for the
/// values the schema names, or the Agent Selection itself for the one it
/// cannot. The Selection is written plainly rather than under a tag, because a
/// Config Document is written by hand and an Agent Selection is already an
/// object no word could be mistaken for.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum TitleErrandDocument {
    Named(String),
    Pinned(AgentSelection),
}

impl Serialize for TitleErrand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.named(), self) {
            (Some(word), _) => TitleErrandDocument::Named(word.to_owned()),
            (None, Self::Pinned(selection)) => TitleErrandDocument::Pinned(selection.clone()),
            (None, _) => unreachable!("every value but a pinned Selection has a word"),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TitleErrand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match TitleErrandDocument::deserialize(deserializer)? {
            TitleErrandDocument::Named(word) => [Self::FollowSession, Self::Off]
                .into_iter()
                .find(|value| value.named() == Some(word.as_str()))
                .ok_or_else(|| {
                    serde::de::Error::custom(format!("{word:?} is not a way of deriving a Title"))
                }),
            TitleErrandDocument::Pinned(selection) => Ok(Self::Pinned(selection)),
        }
    }
}

/// The reader's chosen width for the Session Content Column. `Fill` uses all
/// normally padded terminal columns; `Maximum` centers a column capped at the
/// given width. A maximum is always at least 50 columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionContentWidth {
    Fill,
    Maximum(u64),
}

impl Default for SessionContentWidth {
    fn default() -> Self {
        Self::Maximum(80)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SessionContentWidthDocument {
    Named(String),
    Maximum(u64),
}

impl Serialize for SessionContentWidth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fill => SessionContentWidthDocument::Named("fill".to_owned()),
            Self::Maximum(maximum) => SessionContentWidthDocument::Maximum(*maximum),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionContentWidth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match SessionContentWidthDocument::deserialize(deserializer)? {
            SessionContentWidthDocument::Named(word) if word == "fill" => Ok(Self::Fill),
            SessionContentWidthDocument::Named(word) => Err(serde::de::Error::custom(format!(
                "{word:?} is not a Session Content Column width"
            ))),
            SessionContentWidthDocument::Maximum(maximum) if maximum >= 50 => {
                Ok(Self::Maximum(maximum))
            }
            SessionContentWidthDocument::Maximum(maximum) => Err(serde::de::Error::custom(
                format!("{maximum} is below the 50-column minimum"),
            )),
        }
    }
}

/// How Suru derives a Session's Title.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TitleSettings {
    pub errand: TitleErrand,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    pub content_width: SessionContentWidth,
    pub title: TitleSettings,
}

/// Whether a TUI's Sidebar is on screen.
///
/// The Setting this spells governs only the first frame: a reader who toggles
/// the Sidebar afterwards is changing their view, not their configuration, so
/// nothing writes the choice back.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarVisibility {
    #[default]
    Shown,
    Hidden,
}

/// How long a Session may go untouched before it settles on its own, in whole
/// days. Never less than one: a threshold of zero would settle a Session the
/// moment its work stopped, which is not leaving work alone, it is emptying the
/// active list.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct IdleDays(pub u64);

impl IdleDays {
    pub const MINIMUM: u64 = 1;

    /// The threshold as the milliseconds a Session's timestamps are measured
    /// in, so reading a Session against it is a subtraction rather than a
    /// conversion at every call site.
    pub const fn millis(self) -> u64 {
        self.0.saturating_mul(24 * 60 * 60 * 1_000)
    }
}

impl Default for IdleDays {
    fn default() -> Self {
        Self(2)
    }
}

impl<'de> Deserialize<'de> for IdleDays {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let days = u64::deserialize(deserializer)?;
        if days < Self::MINIMUM {
            return Err(serde::de::Error::custom(format!(
                "{days} is below the one-day minimum"
            )));
        }
        Ok(Self(days))
    }
}

/// When a Session settles without the user saying so, having been left alone
/// long enough.
///
/// Nothing is stored for this and no clock fires for it: a client derives it
/// wherever it lists Sessions, from the Session's last activity and these two
/// values, so turning it off or moving the threshold reclassifies every Session
/// at once and prompting one brings it straight back.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSettleSettings {
    pub enabled: bool,
    pub idle_days: IdleDays,
}

impl Default for AutoSettleSettings {
    fn default() -> Self {
        // Spelled by hand rather than derived, because a derived `bool` is
        // `false` and Sessions settle themselves unless the user says not to.
        Self {
            enabled: true,
            idle_days: IdleDays::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarSettings {
    pub launch_visibility: SidebarVisibility,
    pub auto_settle: AutoSettleSettings,
}

/// Whether the user wants Suru to offer a Provider at all. Every Provider
/// carries one, and each spells its own built-in default by hand rather than
/// deriving it, because a derived `bool` is `false` and a Provider is on unless
/// the user says otherwise.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSettings {
    pub enabled: bool,
    pub reasoning_summary: ReasoningSummaryDetail,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reasoning_summary: ReasoningSummaryDetail::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotSettings {
    pub enabled: bool,
}

impl Default for CopilotSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSettings {
    pub enabled: bool,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub codex: CodexSettings,
    pub copilot: CopilotSettings,
    pub claude: ClaudeSettings,
}

/// The effective value of every defined Setting: what a Config Document
/// pinned where it did, the built-in default everywhere else.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSettings {
    pub transcript: TranscriptSettings,
    pub session: SessionSettings,
    pub sidebar: SidebarSettings,
    pub provider: ProviderSettings,
}

impl EffectiveSettings {
    /// Whether the user has left `provider` on. This is the one predicate every
    /// site consults before asking a Provider for anything, and the only place
    /// a Provider id is read back off the settings tree.
    ///
    /// A Provider this build hosts but the schema names no `enabled` Setting
    /// for reads as enabled: it has nothing the user could have turned off. A
    /// server-side test asserts every built-in Provider does have one, so that
    /// fallback catches a test double rather than a shipped Provider that
    /// silently lost its Setting.
    pub fn provider_enabled(&self, provider: &ProviderId) -> bool {
        match provider.as_str() {
            "codex" => self.provider.codex.enabled,
            "copilot" => self.provider.copilot.enabled,
            "claude" => self.provider.claude.enabled,
            _ => true,
        }
    }
}

/// A typed change to exactly one Setting: the whole surface through which a
/// client edits a Config Document. Every Setting the schema defines has its own
/// variant carrying its own value type, so a client can neither misspell a key
/// path nor pin a value the Setting cannot hold. A `value` pins that value even
/// when it equals the built-in default, so a deliberate choice survives a later
/// change of that default; `null` removes the pin and lets the default resume.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "setting", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingMutation {
    TranscriptDefaultFoldPosture {
        value: Option<FoldPosture>,
    },
    TranscriptReasoningVisibility {
        value: Option<ReasoningVisibility>,
    },
    SessionContentWidth {
        value: Option<SessionContentWidth>,
    },
    SessionTitleErrand {
        value: Option<TitleErrand>,
    },
    SidebarLaunchVisibility {
        value: Option<SidebarVisibility>,
    },
    SidebarAutoSettleEnabled {
        value: Option<bool>,
    },
    SidebarAutoSettleIdleDays {
        value: Option<IdleDays>,
    },
    ProviderCodexEnabled {
        value: Option<bool>,
    },
    ProviderCodexReasoningSummary {
        value: Option<ReasoningSummaryDetail>,
    },
    ProviderCopilotEnabled {
        value: Option<bool>,
    },
    ProviderClaudeEnabled {
        value: Option<bool>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsDiagnosticSeverity {
    /// One key was ignored; the rest of the Config Document applied.
    Warning,
    /// A whole Config Document was ignored.
    Error,
}

/// One configuration problem found while loading Config Documents, carrying
/// enough that the Log alone is sufficient to fix it: the file, the key path
/// when the problem is scoped to one key, and why the value was ignored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDiagnostic {
    pub severity: SettingsDiagnosticSeverity,
    pub file: PathBuf,
    pub key: Option<String>,
    pub message: String,
}

/// The effective-settings view the server pushes to every client on connect.
/// Carries the startup diagnostics so a client can surface configuration
/// problems without a side channel.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    pub settings: EffectiveSettings,
    /// Dotted schema key paths a Config Document pins, so a client can tell a
    /// deliberate choice from a built-in default without reading the file.
    pub pinned: Vec<String>,
    pub diagnostics: Vec<SettingsDiagnostic>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Active,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStatus {
    Pending,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    Steer,
    Queue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    /// Whether the Turn has Settled, and so accepts no further Provider output.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Agent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Streaming,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Active,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileChange {
    Add {
        path: PathBuf,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        moved_to: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Activity {
    Status {
        id: ActivityId,
        turn_id: TurnId,
        text: String,
    },
    Error {
        id: ActivityId,
        turn_id: TurnId,
        text: String,
    },
    Command {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        command: String,
        cwd: Option<PathBuf>,
        output: String,
        /// Whether Suru's cap cut the stored output short of what the Provider
        /// sent, so a client can say so without reading it out of `output`.
        output_truncated: bool,
        exit_status: Option<i32>,
    },
    FileChange {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        changes: Vec<FileChange>,
    },
    /// One block of Reasoning the Provider reported while working the Turn.
    Reasoning {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        /// The heading the Provider led the block with, once it has sent one.
        /// Carried apart from `content` so a client can head a folded block
        /// with it rather than parsing it back out of the prose.
        title: Option<String>,
        content: String,
        /// Whether Suru's cap cut the stored content short of what the Provider
        /// sent, so a client can say so without reading it out of `content`.
        content_truncated: bool,
        /// How long the Provider spent on the block, known only once it
        /// settles and only when it settled by completing.
        duration_ms: Option<u64>,
    },
}

impl Activity {
    pub const fn id(&self) -> ActivityId {
        match self {
            Self::Status { id, .. }
            | Self::Error { id, .. }
            | Self::Command { id, .. }
            | Self::FileChange { id, .. }
            | Self::Reasoning { id, .. } => *id,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::Status { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Command { turn_id, .. }
            | Self::FileChange { turn_id, .. }
            | Self::Reasoning { turn_id, .. } => *turn_id,
        }
    }

    /// The lifecycle this Activity settles through, or `None` for the kinds
    /// that report a moment rather than work in progress.
    pub const fn status(&self) -> Option<ActivityStatus> {
        match self {
            Self::Status { .. } | Self::Error { .. } => None,
            Self::Command { status, .. }
            | Self::FileChange { status, .. }
            | Self::Reasoning { status, .. } => Some(*status),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: SessionId,
    pub workspace: Workspace,
    pub agent_selection: Option<AgentSelection>,
    pub agent_selection_availability: ModelAvailability,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    #[serde(flatten)]
    pub session: Session,
    pub title: String,
    /// The single emoji standing for this Session beside its Title, derived
    /// with that Title and carried apart from it so a reader searching a
    /// listing matches the words rather than the character in front of them.
    /// Absent for every Session whose derivation was skipped, failed, or
    /// predates the feature.
    #[serde(default)]
    pub emoji: Option<String>,
    /// When the user set this Session aside as done for now, and `None` while
    /// it is active. The marker and the moment are one field because a Session
    /// is settled exactly when there is a moment it was settled at — and a
    /// reader ordering the Sessions that were set aside needs that moment as
    /// much as the fact of it.
    #[serde(default)]
    pub settled_at: Option<SessionTimestamp>,
    pub created_at: SessionTimestamp,
    pub updated_at: SessionTimestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "readability",
    content = "summary",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionListItem {
    Readable(SessionSummary),
    Unreadable(UnreadableSessionSummary),
}

impl SessionListItem {
    pub const fn id(&self) -> SessionId {
        match self {
            Self::Readable(summary) => summary.session.id,
            Self::Unreadable(summary) => summary.id,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Readable(summary) => &summary.title,
            Self::Unreadable(summary) => &summary.title,
        }
    }

    /// The Emoji standing for this Session, when it has one. A Session Suru
    /// could not read carries none, because an Emoji is stored beside a Title
    /// that only a readable Session has.
    pub fn emoji(&self) -> Option<&str> {
        match self {
            Self::Readable(summary) => summary.emoji.as_deref(),
            Self::Unreadable(_) => None,
        }
    }

    pub const fn created_at(&self) -> SessionTimestamp {
        match self {
            Self::Readable(summary) => summary.created_at,
            Self::Unreadable(summary) => summary.created_at,
        }
    }

    pub const fn updated_at(&self) -> SessionTimestamp {
        match self {
            Self::Readable(summary) => summary.updated_at,
            Self::Unreadable(summary) => summary.updated_at,
        }
    }

    /// When this Session was set aside as done for now, and `None` while it is
    /// active. A Session Suru could not read is never settled, because settling
    /// is a judgement about work a reader can still return to and prompting is
    /// what returns to it — neither of which an unreadable Session offers.
    pub const fn settled_at(&self) -> Option<SessionTimestamp> {
        match self {
            Self::Readable(summary) => summary.settled_at,
            Self::Unreadable(_) => None,
        }
    }

    pub const fn readable(&self) -> Option<&SessionSummary> {
        match self {
            Self::Readable(summary) => Some(summary),
            Self::Unreadable(_) => None,
        }
    }

    pub const fn workspace(&self) -> Option<&Workspace> {
        match self {
            Self::Readable(summary) => Some(&summary.session.workspace),
            Self::Unreadable(summary) => summary.workspace.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnreadableSessionSummary {
    pub id: SessionId,
    pub title: String,
    pub created_at: SessionTimestamp,
    pub updated_at: SessionTimestamp,
    pub workspace: Option<Workspace>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCatalogSnapshot {
    pub revision: SessionCatalogRevision,
    pub session_ids: Vec<SessionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCatalogChange {
    Created {
        session_id: SessionId,
    },
    Deleted {
        session_id: SessionId,
    },
    /// A Session's Title — and the Emoji standing beside it — was replaced by
    /// a derivation. It rides the catalog stream rather than the Session's own,
    /// because a client subscribes only to the Sessions it has open while the
    /// Title it draws is for every Session it lists.
    TitleChanged {
        session_id: SessionId,
        title: String,
        emoji: Option<String>,
    },
    /// A Session was set aside as done for now, or brought back. It rides the
    /// catalog stream for the same reason a Title does: every client lists the
    /// Session, and only some have it open.
    SettlementChanged {
        session_id: SessionId,
        settled_at: Option<SessionTimestamp>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCatalogUpdate {
    pub revision: SessionCatalogRevision,
    pub change: SessionCatalogChange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub id: PromptId,
    pub text: String,
    pub skill_invocations: Vec<SkillInvocation>,
    pub delivery: PromptDelivery,
    pub admission_order: PromptOrder,
    pub status: PromptStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub id: TurnId,
    pub prompt_id: PromptId,
    pub agent: Option<AgentIdentity>,
    pub status: TurnStatus,
    /// When the commit that delivered this Turn's opening Prompt landed, and
    /// when the commit that settled it landed. Both are absent on a Turn stored
    /// before Suru recorded Turn timing, so a client states how long a Turn
    /// worked only when it knows.
    pub started_at: Option<SessionTimestamp>,
    pub settled_at: Option<SessionTimestamp>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub turn_id: TurnId,
    pub role: MessageRole,
    pub status: MessageStatus,
    pub content: String,
    /// Safe invocation records captured with a user Message. Agent Messages
    /// always carry an empty list.
    pub skill_invocations: Vec<SkillInvocation>,
    /// Whether Suru's cap cut the stored content short of what the Provider
    /// sent, so a client can say so without reading it out of `content`.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItem {
    Message { message_id: MessageId },
    Activity { activity_id: ActivityId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub session: Session,
    pub revision: SessionRevision,
    pub prompts: Vec<Prompt>,
    pub turns: Vec<Turn>,
    pub messages: Vec<Message>,
    pub activities: Vec<Activity>,
    pub transcript: Vec<TranscriptItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    pub session_id: SessionId,
    pub revision: SessionRevision,
    pub changes: Vec<SessionChange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionChange {
    AgentSelectionChanged {
        selection: AgentSelection,
    },
    AgentSelectionAvailabilityChanged {
        availability: ModelAvailability,
    },
    PromptAdded {
        prompt: Prompt,
    },
    PromptDeliveryChanged {
        prompt_id: PromptId,
        delivery: PromptDelivery,
    },
    PromptStatusChanged {
        prompt_id: PromptId,
        status: PromptStatus,
    },
    TurnAdded {
        turn: Turn,
    },
    TurnAgentChanged {
        turn_id: TurnId,
        agent: AgentIdentity,
    },
    MessageAdded {
        message: Message,
    },
    MessageContentAppended {
        message_id: MessageId,
        content: String,
    },
    MessageTruncated {
        message_id: MessageId,
    },
    MessageCompleted {
        message_id: MessageId,
    },
    ActivityAdded {
        activity: Activity,
    },
    CommandOutputAppended {
        activity_id: ActivityId,
        content: String,
    },
    CommandOutputTruncated {
        activity_id: ActivityId,
    },
    CommandStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
        exit_status: Option<i32>,
    },
    FileChangeUpdated {
        activity_id: ActivityId,
        changes: Vec<FileChange>,
    },
    FileChangeStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
    },
    ReasoningTitleChanged {
        activity_id: ActivityId,
        title: String,
    },
    ReasoningContentAppended {
        activity_id: ActivityId,
        content: String,
    },
    ReasoningContentTruncated {
        activity_id: ActivityId,
    },
    ReasoningStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
        duration_ms: Option<u64>,
    },
    TurnStatusChanged {
        turn_id: TurnId,
        status: TurnStatus,
        /// Stamped by the settle commit itself, so every client settles the
        /// Turn at the moment the server did rather than when it read the
        /// change.
        settled_at: Option<SessionTimestamp>,
    },
    SessionStatusChanged {
        status: SessionStatus,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitialPrompt {
    pub id: PromptId,
    pub text: String,
    pub skill_invocations: Vec<SkillInvocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    pub agent_selection: Option<AgentSelection>,
    pub workspace: Workspace,
    pub prompt: InitialPrompt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitPromptRequest {
    pub prompt: InitialPrompt,
    pub delivery: PromptDelivery,
}

/// Whether a Session is being set aside as done for now, or brought back. The
/// intent is carried rather than a toggle so a client that has been looking at
/// a stale listing cannot flip a Session it meant to leave alone.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettleSessionRequest {
    pub settled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgentSelectionRequest {
    pub operation_id: AgentSelectionOperationId,
    pub selection: AgentSelection,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    InvalidCommand,
    EmptyPrompt,
    InvalidWorkspace,
    SessionNotFound,
    PromptConflict,
    PromptNotFound,
    PromptNotPending,
    TurnNotFound,
    TurnNotActive,
    TurnInterruptionFailed,
    AgentSelectionOperationConflict,
    AgentSelectionProviderConflict,
    InvalidSkillInvocation,
    ConfigRootUnavailable,
    ConfigDocumentNotEditable,
    ConfigDocumentWriteFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerIdentity {
    pub instance_id: Uuid,
    pub pid: u32,
    pub protocol_version: u32,
    pub build_identity: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Health {
    pub lifecycle: LifecycleState,
    pub landing_agent_selection: Option<AgentSelection>,
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl Health {
    pub fn new(identity: ServerIdentity, lifecycle: LifecycleState) -> Self {
        Self {
            lifecycle,
            landing_agent_selection: None,
            identity,
        }
    }

    pub fn with_landing_agent_selection(
        mut self,
        landing_agent_selection: Option<AgentSelection>,
    ) -> Self {
        self.landing_agent_selection = landing_agent_selection;
        self
    }
}

impl std::ops::Deref for Health {
    type Target = ServerIdentity;

    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

impl std::ops::DerefMut for Health {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identity
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthWire {
    instance_id: Uuid,
    pid: u32,
    lifecycle: LifecycleState,
    protocol_version: u32,
    build_identity: String,
    #[serde(default)]
    landing_agent_selection: Option<AgentSelection>,
}

impl<'de> Deserialize<'de> for Health {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HealthWire::deserialize(deserializer)?;
        Ok(Self::new(
            ServerIdentity {
                instance_id: wire.instance_id,
                pid: wire.pid,
                protocol_version: wire.protocol_version,
                build_identity: wire.build_identity,
            },
            wire.lifecycle,
        )
        .with_landing_agent_selection(wire.landing_agent_selection))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeDescriptor {
    pub base_url: String,
    pub token: String,
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl RuntimeDescriptor {
    pub fn new(base_url: String, token: String, identity: ServerIdentity) -> Self {
        Self {
            base_url,
            token,
            identity,
        }
    }

    pub fn health(&self, lifecycle: LifecycleState) -> Health {
        Health::new(self.identity.clone(), lifecycle)
    }
}

impl std::ops::Deref for RuntimeDescriptor {
    type Target = ServerIdentity;

    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

impl std::ops::DerefMut for RuntimeDescriptor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identity
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDescriptorWire {
    base_url: String,
    token: String,
    instance_id: Uuid,
    pid: u32,
    protocol_version: u32,
    build_identity: String,
}

impl<'de> Deserialize<'de> for RuntimeDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = RuntimeDescriptorWire::deserialize(deserializer)?;
        Ok(Self::new(
            wire.base_url,
            wire.token,
            ServerIdentity {
                instance_id: wire.instance_id,
                pid: wire.pid,
                protocol_version: wire.protocol_version,
                build_identity: wire.build_identity,
            },
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Manual,
    Replacement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerShutdown {
    pub instance_id: Uuid,
    pub reason: ShutdownReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleted {
    pub session_id: SessionId,
}

/// A Session set aside as done for now, or brought back, carried to a client
/// that may be listing that Session without having it open.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettlementChanged {
    pub session_id: SessionId,
    pub settled_at: Option<SessionTimestamp>,
}

/// A Session's Title — and the Emoji beside it — as a derivation left them,
/// carried to a client that may be listing that Session without having it open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitleChanged {
    pub session_id: SessionId,
    pub title: String,
    pub emoji: Option<String>,
}
