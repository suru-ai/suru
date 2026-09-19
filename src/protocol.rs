use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use uuid::Uuid;

mod workspace_paths;
pub use workspace_paths::{MANAGED_WORKTREE_DIRECTORY, PathStyle, WorkspacePaths};

pub const PROTOCOL_VERSION: u32 = 58;
mod source_control;
pub use crate::approval::{Approval, ApprovalOutcome, ApprovalSubject, CommandAction, Decision};
pub use crate::questionnaire::{
    Answer, Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireOutcome,
    QuestionnaireSubmission,
};
pub use source_control::*;
pub const SERVER_SHUTDOWN_EVENT: &str = "server_shutdown";
pub const SETTINGS_SNAPSHOT_EVENT: &str = "settings_snapshot";
pub const MODEL_CATALOG_EVENT: &str = "model_catalog";
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
session_identity!(ViewSessionOperationId);

session_identity!(QuestionnaireId);
session_identity!(ApprovalId);

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
    /// The Provider remains usable, but its environment has a compatibility
    /// condition worth fixing. Models stay selectable; this is guidance, not
    /// Provider Unavailability.
    Warning {
        message: String,
    },
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
    pub id: WorkspaceId,
    /// Presentation only: identity is unchanged when a main root becomes known.
    pub path: PathBuf,
    pub repository: Option<Repository>,
    pub source_control: SourceControlAvailability,
    /// The Workspace's Icon, as the `workspaces` table holds it: absent until
    /// a derivation lands one for good. It rides every copy of a Workspace —
    /// discovery, a listed Session, an open Session's own — because the table
    /// is its one source of truth (see the **Icon** glossary entry and ADR
    /// 0027 for why there is no broader Workspace registry beside it).
    #[serde(default)]
    pub icon: Option<String>,
}

impl Workspace {
    pub fn directory(path: PathBuf) -> Self {
        Self {
            id: WorkspaceId::directory(&path),
            path,
            repository: None,
            source_control: SourceControlAvailability::NotDetected,
            icon: None,
        }
    }
    pub fn main_unknown(&self) -> bool {
        self.repository
            .as_ref()
            .is_some_and(|repo| matches!(repo.location, RepositoryLocation::UnknownMain))
    }
}
impl From<PathBuf> for Workspace {
    fn from(path: PathBuf) -> Self {
        Self::directory(path)
    }
}

/// The exact directory an Agent executes in, interpreted only by its owning Server.
/// It is independent of Workspace grouping and is fixed when the first Turn begins.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDirectory {
    pub path: PathBuf,
}

/// A path a Client asks its Outlook Server to interpret as a Workspace. A
/// relative path is read from `base`, or from the Server process's current
/// directory when no base is supplied.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveWorkspaceRequest {
    #[serde(default)]
    pub checkout_id: Option<CheckoutId>,
    #[serde(default)]
    pub remembered_execution_directory: Option<ExecutionDirectory>,
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    pub base: Option<PathBuf>,
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
/// Execution Directory. Including both identities makes crossing Provider
/// or execution contexts visible at every caller boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalog {
    pub provider: ProviderId,
    pub execution_directory: ExecutionDirectory,
    pub skills: Vec<SkillDescriptor>,
    pub capabilities: SkillCatalogCapabilities,
    pub status: SkillCatalogStatus,
}

/// The client context whose current Skill Catalog it wants. The server
/// canonicalizes the Execution Directory before consulting the Provider, so spelling
/// variants of one directory cannot create separate authority domains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillCatalogRequest {
    pub provider: ProviderId,
    pub execution_directory: ExecutionDirectory,
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

/// Which lightness variant a Theme paints with. `System` follows the attached
/// terminal when it can and otherwise takes the dark variant.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceMode {
    #[default]
    System,
    Dark,
    Light,
}

/// Whether the Landing includes the Japanese Suru banner above its composer.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum LandingPage {
    #[default]
    Minimal,
    Fancy,
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

/// Whether a running Command grows from its one-line row into a live output
/// tail on its own. `Off` keeps disclosure entirely in the reader's hands;
/// `AfterMillis` promotes a command that has remained Active for that long.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CommandAutoExpand {
    #[default]
    Off,
    AfterMillis(u64),
}

impl CommandAutoExpand {
    /// The threshold offered when a reader opens the numeric surface from the
    /// off state, so enabling the behavior begins at a useful latency.
    pub const DEFAULT_MILLIS: u64 = 500;

    pub const fn after_millis(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::AfterMillis(milliseconds) => Some(milliseconds),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum CommandAutoExpandDocument {
    Enabled(bool),
    AfterMillis(u64),
}

impl Serialize for CommandAutoExpand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => CommandAutoExpandDocument::Enabled(false),
            Self::AfterMillis(milliseconds) => {
                CommandAutoExpandDocument::AfterMillis(*milliseconds)
            }
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CommandAutoExpand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match CommandAutoExpandDocument::deserialize(deserializer)? {
            CommandAutoExpandDocument::Enabled(false) => Ok(Self::Off),
            CommandAutoExpandDocument::Enabled(true) => Err(serde::de::Error::custom(
                "true does not specify when Commands should expand",
            )),
            CommandAutoExpandDocument::AfterMillis(milliseconds) => {
                Ok(Self::AfterMillis(milliseconds))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSettings {
    pub default_fold_posture: FoldPosture,
    pub reasoning_visibility: ReasoningVisibility,
    pub command_auto_expand: CommandAutoExpand,
}

/// Which Agent Selection derives a Session's Title and a Workspace's Icon,
/// which is also whether Suru derives either at all.
///
/// One Setting rather than two, because two would admit a state that
/// contradicts itself — deriving turned off while a Model stands pinned for it
/// — and would give the settings panel two rows for one intent. Titles and
/// Icons share this one Setting rather than each having their own: both are
/// Provider-authored small talk asked of the same Errand Selection at the
/// same moment (Session creation), and a user who wants neither, or wants a
/// cheap Model to write both, is expressing one preference.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DerivationErrand {
    /// The built-in default: each Session's Title, and its Workspace's Icon
    /// where it has none, are derived by the Provider that Session already
    /// uses, at that Provider's Errand Selection. A Session that has selected
    /// no Provider is left alone, because Suru will not pick one the user did
    /// not choose.
    #[default]
    FollowSession,
    /// Suru derives no Titles and no Icons, and makes no Provider call on its
    /// own behalf for either.
    Off,
    /// Every Session's Title, and every Workspace's Icon, is derived by this
    /// Provider and Model, whatever the Session itself uses — including a
    /// Session that uses nothing. The Selection is resolved against the live
    /// Model catalog like any other Errand Selection, so a Model that has gone
    /// gives way to that Provider's default rather than failing the Errand.
    Pinned(AgentSelection),
}

impl DerivationErrand {
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

/// How a Config Document spells a [`DerivationErrand`]: one of the two words
/// for the values the schema names, or the Agent Selection itself for the one
/// it cannot. The Selection is written plainly rather than under a tag,
/// because a Config Document is written by hand and an Agent Selection is
/// already an object no word could be mistaken for.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum DerivationErrandDocument {
    Named(String),
    Pinned(AgentSelection),
}

impl Serialize for DerivationErrand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match (self.named(), self) {
            (Some(word), _) => DerivationErrandDocument::Named(word.to_owned()),
            (None, Self::Pinned(selection)) => DerivationErrandDocument::Pinned(selection.clone()),
            (None, _) => unreachable!("every value but a pinned Selection has a word"),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DerivationErrand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match DerivationErrandDocument::deserialize(deserializer)? {
            DerivationErrandDocument::Named(word) => [Self::FollowSession, Self::Off]
                .into_iter()
                .find(|value| value.named() == Some(word.as_str()))
                .ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "{word:?} is not a way of deriving a Title or an Icon"
                    ))
                }),
            DerivationErrandDocument::Pinned(selection) => Ok(Self::Pinned(selection)),
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

/// How Suru derives a Session's Title and a Workspace's Icon.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DerivationSettings {
    pub errand: DerivationErrand,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    pub content_width: SessionContentWidth,
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

/// Which Workspaces a TUI's Sidebar lists.
///
/// The Setting this spells governs only the scope a Sidebar launches with: the
/// selector above the list is the reader's to move afterwards, and — as with
/// the Sidebar's own visibility — nothing writes that choice back.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarScope {
    /// The reader's whole body of work, whichever Workspace it is rooted in.
    #[default]
    AllWorkspaces,
    /// Only the Workspace the client itself runs in.
    CurrentWorkspace,
    /// Every Workspace on every Origin the client can reach.
    Everywhere,
}

/// When a Session settles on its own, having been left alone long enough.
/// `Off` leaves the settled shelf to the user's own say-so alone; `Idle`
/// settles a Session that has gone that many whole days untouched. An idle
/// threshold is always at least one day.
///
/// Nothing is stored for any of this and no clock fires for it: a client
/// derives settlement wherever it lists Sessions, from each Session's last
/// activity against this one value — so turning it off or moving the threshold
/// reclassifies every Session at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoSettle {
    Off,
    Idle(u64),
}

/// Whether the Server Reclaims Managed Worktrees, and the age threshold used
/// by age-based rules. Orphaned Worktrees use the same switch but no age.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoReclaim {
    Off,
    AfterDays(u64),
}

impl AutoReclaim {
    pub const MINIMUM_DAYS: u64 = 1;
}

impl Default for AutoReclaim {
    fn default() -> Self {
        Self::AfterDays(14)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum AutoReclaimDocument {
    Named(String),
    Days(u64),
}

impl Serialize for AutoReclaim {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => AutoReclaimDocument::Named("off".to_owned()),
            Self::AfterDays(days) => AutoReclaimDocument::Days(*days),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AutoReclaim {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match AutoReclaimDocument::deserialize(deserializer)? {
            AutoReclaimDocument::Named(word) if word == "off" => Ok(Self::Off),
            AutoReclaimDocument::Named(word) => Err(serde::de::Error::custom(format!(
                "{word:?} is not a Managed Worktree reclaim threshold"
            ))),
            AutoReclaimDocument::Days(days) if days >= Self::MINIMUM_DAYS => {
                Ok(Self::AfterDays(days))
            }
            AutoReclaimDocument::Days(days) => Err(serde::de::Error::custom(format!(
                "{days} is below the one-day minimum"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeSettings {
    pub auto_reclaim: AutoReclaim,
}

impl AutoSettle {
    pub const MINIMUM_IDLE_DAYS: u64 = 1;

    /// How long a Session must go untouched to settle itself, in the
    /// milliseconds a Session's timestamps are measured in, and `None` where
    /// nothing settles itself. This is the whole of what a classifier needs, so
    /// no caller reads the days back out or asks whether settling is on.
    pub const fn idle_millis(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Idle(days) => Some(days.saturating_mul(24 * 60 * 60 * 1_000)),
        }
    }
}

impl Default for AutoSettle {
    fn default() -> Self {
        Self::Idle(3)
    }
}

/// How a Config Document spells an [`AutoSettle`]: the one word for the value
/// that is not a duration, or the days themselves. The same scalar shape
/// `session.contentWidth` takes, and for the same reason — the value's own
/// shape says which of the two it is, so nothing has to be written down twice.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum AutoSettleDocument {
    Named(String),
    Idle(u64),
}

impl Serialize for AutoSettle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => AutoSettleDocument::Named("off".to_owned()),
            Self::Idle(days) => AutoSettleDocument::Idle(*days),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AutoSettle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match AutoSettleDocument::deserialize(deserializer)? {
            AutoSettleDocument::Named(word) if word == "off" => Ok(Self::Off),
            AutoSettleDocument::Named(word) => Err(serde::de::Error::custom(format!(
                "{word:?} is not a way of settling a Session on its own"
            ))),
            AutoSettleDocument::Idle(days) if days >= Self::MINIMUM_IDLE_DAYS => {
                Ok(Self::Idle(days))
            }
            // A threshold of zero would settle a Session the moment its work
            // stopped, which is not leaving work alone, it is emptying the
            // active list.
            AutoSettleDocument::Idle(days) => Err(serde::de::Error::custom(format!(
                "{days} is below the one-day minimum"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarSettings {
    pub initial_visibility: SidebarVisibility,
    pub initial_width: u64,
    pub initial_scope: SidebarScope,
    pub auto_settle: AutoSettle,
}

impl Default for SidebarSettings {
    fn default() -> Self {
        Self {
            initial_visibility: SidebarVisibility::default(),
            initial_width: 32,
            initial_scope: SidebarScope::default(),
            auto_settle: AutoSettle::default(),
        }
    }
}

/// Client presentation selected independently of whichever Server the reader
/// is looking into. Theme names stay open strings because built-ins are only
/// the first source; later tickets add Themes discovered from disk.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceSettings {
    pub theme: String,
    pub mode: AppearanceMode,
    pub landing_page: LandingPage,
    pub show_icons: bool,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: "system".to_owned(),
            mode: AppearanceMode::System,
            landing_page: LandingPage::Minimal,
            show_icons: false,
        }
    }
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
    pub approval_policy: CodexApprovalPolicy,
    pub sandbox_mode: CodexSandboxMode,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reasoning_summary: ReasoningSummaryDetail::default(),
            approval_policy: CodexApprovalPolicy::default(),
            sandbox_mode: CodexSandboxMode::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexApprovalPolicy {
    Untrusted,
    #[default]
    OnRequest,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotSettings {
    pub enabled: bool,
    pub permissions: CopilotPermissions,
}

impl Default for CopilotSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            permissions: CopilotPermissions::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CopilotPermissions {
    #[default]
    Ask,
    AllowAll,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSettings {
    pub enabled: bool,
    pub permission_mode: ClaudePermissionMode,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            permission_mode: ClaudePermissionMode::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudePermissionMode {
    #[default]
    Default,
    AcceptEdits,
    DontAsk,
    BypassPermissions,
    Auto,
}

impl ClaudePermissionMode {
    pub const fn as_wire_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub codex: CodexSettings,
    pub copilot: CopilotSettings,
    pub claude: ClaudeSettings,
}

/// How the Server's opt-in second listener is exposed. Keeping the bind
/// address typed means malformed addresses are rejected by Settings loading
/// before anything reaches the network boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServingSettings {
    pub enabled: bool,
    pub port: u16,
    pub bind_address: IpAddr,
}

/// The addresses a Serving user chose to advertise in a freshly issued
/// Invite. They are concrete socket addresses because this first Pairing
/// transport has no discovery or name-resolution contract of its own.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IssueInviteRequest {
    pub addresses: Vec<std::net::SocketAddr>,
}

/// The pasteable Invite together with the addresses it carries, returned by
/// the Server's local interface so a Client need not decode credential data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IssuedInvite {
    pub invite: String,
    pub addresses: Vec<std::net::SocketAddr>,
}

/// The non-secret facts a local Client may show before the reader decides to
/// trust and dial the Serving Server named by a pasted Invite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InvitePreview {
    pub hostname: String,
    pub fingerprint: String,
    pub addresses: Vec<std::net::SocketAddr>,
}

/// A pasted Invite to inspect locally. Inspection validates and decodes the
/// Invite but never dials an offered address or changes Pairing state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewInviteRequest {
    pub invite: String,
}

/// The connecting user's choices when redeeming an Invite. `addresses` must
/// be the Invite's offered addresses in the priority order to dial; an empty
/// list keeps the offered order. `name` defaults to the Serving machine's
/// hostname when omitted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedeemInviteRequest {
    pub invite: String,
    pub name: Option<String>,
    pub addresses: Vec<std::net::SocketAddr>,
}

/// A durable paired Serving Server as the connecting Server knows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub name: String,
    pub fingerprint: String,
    pub addresses: Vec<std::net::SocketAddr>,
    /// The last status observed by this Server, refreshed by explicit probes
    /// and Remote API use so it survives beyond the request that discovered it.
    pub status: RemoteStatus,
}

/// The Server whose world a Client is presently presenting. This is Client
/// state rather than wire state: Remote requests still travel through the
/// local Server's explicit proxy route.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Outlook {
    #[default]
    Local,
    Remote(String),
}

impl Outlook {
    pub fn remote_name(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Remote(name) => Some(name),
        }
    }
}

/// A Client-side reference to one Session together with the Server that owns
/// it. Session IDs are only unique within an origin Server, so callers must
/// retain both even while the current UI presents one Outlook at a time.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionReference {
    pub origin: Outlook,
    pub session_id: SessionId,
}

impl SessionReference {
    pub fn new(origin: Outlook, session_id: SessionId) -> Self {
        Self { origin, session_id }
    }
}

/// A durable redeeming Server as the Serving Server knows it. The key itself
/// remains credential material inside the Server; local Clients receive only
/// the stable fingerprint used to identify and remove the Peer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub id: String,
    pub fingerprint: String,
}

/// Whether a Remote can serve this Server's protocol. Kept on the successful
/// status response so a Client can present incompatibility without treating
/// the local Server request itself as failed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteStatus {
    Available,
    Unavailable,
    Revoked,
    ProtocolMismatch,
}

/// The outcome of removing a Remote. Removal always ends the Pairing on this
/// side; `acknowledged` says whether the Remote answered in time and dropped
/// its Peer record for this Server as well.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRemoval {
    pub name: String,
    pub acknowledged: bool,
}

/// The result of probing a Remote. The protocol version is known only after an
/// authenticated health response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHealth {
    pub protocol_version: Option<u32>,
    pub status: RemoteStatus,
}

impl Default for ServingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7777,
            // Unspecified dual-stack, because an Invite advertises the
            // machine's non-loopback addresses: a listener that cannot answer
            // on them would strand every Invite it issues.
            bind_address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        }
    }
}

/// When a reader copies a Text Selection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSelectionCopy {
    #[cfg_attr(not(windows), default)]
    Release,
    #[cfg_attr(windows, default)]
    Manual,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextSelectionSettings {
    pub copy: TextSelectionCopy,
}

/// The effective value of every defined Setting: what a Config Document
/// pinned where it did, the built-in default everywhere else.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSettings {
    pub text_selection: TextSelectionSettings,
    pub appearance: AppearanceSettings,
    pub transcript: TranscriptSettings,
    pub session: SessionSettings,
    /// How Suru derives a Session's Title and a Workspace's Icon. A top-level
    /// Setting rather than one scoped under `session`, because a Workspace
    /// outlives any one Session and the Errand this governs derives both.
    pub derivation: DerivationSettings,
    pub sidebar: SidebarSettings,
    pub worktree: WorktreeSettings,
    pub provider: ProviderSettings,
    pub serving: ServingSettings,
}

/// The Provider-native controls that govern which tool uses require an
/// Approval. Each variant retains the complete tuple its Provider accepts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalPosture {
    Codex {
        approval_policy: CodexApprovalPolicy,
        sandbox_mode: CodexSandboxMode,
    },
    Claude {
        permission_mode: ClaudePermissionMode,
    },
    Copilot {
        permissions: CopilotPermissions,
    },
}

impl ApprovalPosture {
    pub fn for_provider(provider: &ProviderId, settings: &EffectiveSettings) -> Option<Self> {
        match provider.as_str() {
            "codex" => Some(Self::Codex {
                approval_policy: settings.provider.codex.approval_policy,
                sandbox_mode: settings.provider.codex.sandbox_mode,
            }),
            "claude" => Some(Self::Claude {
                permission_mode: settings.provider.claude.permission_mode,
            }),
            "copilot" => Some(Self::Copilot {
                permissions: settings.provider.copilot.permissions,
            }),
            _ => None,
        }
    }

    pub fn provider(&self) -> ProviderId {
        ProviderId::new(match self {
            Self::Codex { .. } => "codex",
            Self::Claude { .. } => "claude",
            Self::Copilot { .. } => "copilot",
        })
    }

    pub const fn cycle_primary(self) -> Self {
        match self {
            Self::Codex {
                approval_policy,
                sandbox_mode,
            } => Self::Codex {
                approval_policy: match approval_policy {
                    CodexApprovalPolicy::Untrusted => CodexApprovalPolicy::OnRequest,
                    CodexApprovalPolicy::OnRequest => CodexApprovalPolicy::Never,
                    CodexApprovalPolicy::Never => CodexApprovalPolicy::Untrusted,
                },
                sandbox_mode,
            },
            Self::Claude { permission_mode } => Self::Claude {
                permission_mode: match permission_mode {
                    ClaudePermissionMode::Default => ClaudePermissionMode::AcceptEdits,
                    ClaudePermissionMode::AcceptEdits => ClaudePermissionMode::DontAsk,
                    ClaudePermissionMode::DontAsk => ClaudePermissionMode::BypassPermissions,
                    ClaudePermissionMode::BypassPermissions => ClaudePermissionMode::Auto,
                    ClaudePermissionMode::Auto => ClaudePermissionMode::Default,
                },
            },
            Self::Copilot { permissions } => Self::Copilot {
                permissions: match permissions {
                    CopilotPermissions::Ask => CopilotPermissions::AllowAll,
                    CopilotPermissions::AllowAll => CopilotPermissions::Ask,
                },
            },
        }
    }

    pub fn summary(self) -> String {
        match self {
            Self::Codex {
                approval_policy,
                sandbox_mode,
            } => format!(
                "Codex {} · {}",
                match approval_policy {
                    CodexApprovalPolicy::Untrusted => "untrusted",
                    CodexApprovalPolicy::OnRequest => "on-request",
                    CodexApprovalPolicy::Never => "never",
                },
                match sandbox_mode {
                    CodexSandboxMode::ReadOnly => "read-only",
                    CodexSandboxMode::WorkspaceWrite => "workspace-write",
                    CodexSandboxMode::DangerFullAccess => "danger-full-access",
                }
            ),
            Self::Claude { permission_mode } => {
                format!("Claude {}", permission_mode.as_wire_value())
            }
            Self::Copilot { permissions } => format!(
                "Copilot {}",
                match permissions {
                    CopilotPermissions::Ask => "ask",
                    CopilotPermissions::AllowAll => "allowAll",
                }
            ),
        }
    }
}

/// The effective Approval Posture reported by a Session. `pinned` distinguishes
/// a Session override from a live reading of the Server Setting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionApprovalPosture {
    pub value: ApprovalPosture,
    pub pinned: bool,
    #[serde(default)]
    pub application: ApprovalPostureApplication,
}

/// How the effective posture has reached the Provider connection that owns
/// this Session. A requested value remains authoritative when native delivery
/// fails, while the status keeps clients from claiming it is already active.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPostureApplication {
    #[default]
    Applied,
    Applying,
    NextTurn,
    Failed,
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
    TextSelectionCopy {
        value: Option<TextSelectionCopy>,
    },
    AppearanceTheme {
        value: Option<String>,
    },
    AppearanceMode {
        value: Option<AppearanceMode>,
    },
    AppearanceLandingPage {
        value: Option<LandingPage>,
    },
    AppearanceShowIcons {
        value: Option<bool>,
    },
    TranscriptDefaultFoldPosture {
        value: Option<FoldPosture>,
    },
    TranscriptReasoningVisibility {
        value: Option<ReasoningVisibility>,
    },
    TranscriptCommandAutoExpand {
        value: Option<CommandAutoExpand>,
    },
    SessionContentWidth {
        value: Option<SessionContentWidth>,
    },
    DerivationErrand {
        value: Option<DerivationErrand>,
    },
    SidebarInitialVisibility {
        value: Option<SidebarVisibility>,
    },
    SidebarInitialWidth {
        value: Option<u64>,
    },
    SidebarInitialScope {
        value: Option<SidebarScope>,
    },
    SidebarAutoSettle {
        value: Option<AutoSettle>,
    },
    WorktreeAutoReclaim {
        value: Option<AutoReclaim>,
    },
    ProviderCodexEnabled {
        value: Option<bool>,
    },
    ProviderCodexReasoningSummary {
        value: Option<ReasoningSummaryDetail>,
    },
    ProviderCodexApprovalPolicy {
        value: Option<CodexApprovalPolicy>,
    },
    ProviderCodexSandboxMode {
        value: Option<CodexSandboxMode>,
    },
    ProviderCopilotEnabled {
        value: Option<bool>,
    },
    ProviderCopilotPermissions {
        value: Option<CopilotPermissions>,
    },
    ProviderClaudeEnabled {
        value: Option<bool>,
    },
    ProviderClaudePermissionMode {
        value: Option<ClaudePermissionMode>,
    },
    ServingEnabled {
        value: Option<bool>,
    },
    ServingPort {
        value: Option<u16>,
    },
    ServingBindAddress {
        value: Option<IpAddr>,
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
    /// Settled because the user asked the work to stop — an interrupted
    /// Session or a Subagent stopped on its own — rather than because it
    /// finished or went wrong. Only Subagent Activities settle this way:
    /// every other kind is closed by its Turn's own settle.
    Interrupted,
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
    Approval {
        id: ActivityId,
        turn_id: TurnId,
        approval: Approval,
        /// The Tool Activity this Approval gates, when orchestration could
        /// resolve the Provider's native identity to an existing row.
        tool_activity_id: Option<ActivityId>,
        /// The durable subject or reason reached Suru's storage cap. The live
        /// Provider request remains whole and owns Decision translation.
        #[serde(default)]
        detail_truncated: bool,
        outcome: ApprovalOutcome,
        decision: Option<Decision>,
        /// A Provider action that followed definitive Decision delivery failed.
        /// The Decision remains recorded because resending it is unsafe.
        #[serde(default)]
        follow_up_error: Option<String>,
    },
    Questionnaire {
        id: ActivityId,
        turn_id: TurnId,
        questionnaire: Questionnaire,
        outcome: QuestionnaireOutcome,
        answer: Option<Answer>,
    },
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
    /// One Subagent the Turn's Agent delegated work to. This row is all its
    /// spawner's Transcript carries of it: the Subagent's work lives in the
    /// child Session the row names, never interleaved here.
    Subagent {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        /// The Subagent's name — which kind of agent the Provider ran, as the
        /// reader should meet it.
        name: String,
        /// What the Subagent was asked to do, as its spawn described it.
        description: String,
        /// The latest Model the Provider confirmed for this Subagent. It is
        /// independent of the parent's mutable Agent Selection and remains
        /// absent until the Provider supplies evidence.
        #[serde(default)]
        model: Option<ModelId>,
        /// The Subagent's own Session: a child of the Session this row is in,
        /// and the way into everything the Subagent did.
        session_id: SessionId,
        /// How long the Subagent worked, timed by Suru from its spawn. Known
        /// only once it settles — by the Provider's own settle event or by a
        /// stop the user asked for — and absent on a row that a lost Provider
        /// connection closed, there being no moment the work truly ended.
        duration_ms: Option<u64>,
    },
}

impl Activity {
    pub const fn id(&self) -> ActivityId {
        match self {
            Self::Approval { id, .. }
            | Self::Questionnaire { id, .. }
            | Self::Status { id, .. }
            | Self::Error { id, .. }
            | Self::Command { id, .. }
            | Self::FileChange { id, .. }
            | Self::Reasoning { id, .. }
            | Self::Subagent { id, .. } => *id,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::Approval { turn_id, .. }
            | Self::Questionnaire { turn_id, .. }
            | Self::Status { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Command { turn_id, .. }
            | Self::FileChange { turn_id, .. }
            | Self::Reasoning { turn_id, .. }
            | Self::Subagent { turn_id, .. } => *turn_id,
        }
    }

    /// The lifecycle this Activity settles through, or `None` for the kinds
    /// that report a moment rather than work in progress.
    pub const fn status(&self) -> Option<ActivityStatus> {
        match self {
            Self::Approval { outcome, .. } => Some(match outcome {
                ApprovalOutcome::Pending
                | ApprovalOutcome::SubmissionRejected
                | ApprovalOutcome::Submitting => ActivityStatus::Active,
                ApprovalOutcome::Decided => ActivityStatus::Completed,
                _ => ActivityStatus::Failed,
            }),
            Self::Questionnaire { outcome, .. } => Some(match outcome {
                QuestionnaireOutcome::Pending
                | QuestionnaireOutcome::SubmissionRejected
                | QuestionnaireOutcome::Submitting => ActivityStatus::Active,
                QuestionnaireOutcome::Answered | QuestionnaireOutcome::Declined => {
                    ActivityStatus::Completed
                }
                _ => ActivityStatus::Failed,
            }),
            Self::Status { .. } | Self::Error { .. } => None,
            Self::Command { status, .. }
            | Self::FileChange { status, .. }
            | Self::Reasoning { status, .. }
            | Self::Subagent { status, .. } => Some(*status),
        }
    }
}

/// A measured context occupancy, independent of cumulative Usage and Cost.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextFill {
    pub occupied_tokens: u64,
    /// The raw Model window; zero is treated as unknown by consumers.
    pub capacity_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The latest reported occupancy of this Session alone.
    #[serde(default)]
    pub context_fill: Option<ContextFill>,
    pub id: SessionId,
    pub workspace: Workspace,
    pub execution_directory: ExecutionDirectory,
    pub checkout: Option<CheckoutAssociation>,
    pub agent_selection: Option<AgentSelection>,
    pub agent_selection_availability: ModelAvailability,
    /// The effective Approval Posture for this Session's immutable Provider.
    /// A pinned value is the durable Session override; otherwise it is the
    /// latest Server Setting. Sessions without a selected or supported
    /// Provider have no posture.
    #[serde(default)]
    pub approval_posture: Option<SessionApprovalPosture>,
    pub status: SessionStatus,
    /// When this Session's uninterrupted Working interval began, including
    /// work continued by surviving Subagents after its own Turn Settles.
    /// `None` means the whole Session subtree is no longer Working.
    #[serde(default)]
    pub working_since: Option<SessionTimestamp>,
    /// The Session whose Turn spawned this one, present exactly when this is a
    /// Subagent's Session. A child is reachable only through its parent: it
    /// joins no Session listing, refuses Prompts, and is deleted along with
    /// the parent it names.
    #[serde(default)]
    pub parent: Option<SessionId>,
}

impl Session {
    /// Whether this is a Subagent's Session — a child of the Session whose
    /// Turn spawned it. Every property of being one keys off this single
    /// reading: excluded from listings and the catalog, refusing Prompts, and
    /// deleted with its parent.
    pub const fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }
}

/// The latest Turn facts a Session listing needs to derive its Standing. The
/// status and Settle moment travel together so a client never combines facts
/// from different Turns.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LatestTurnStatus {
    pub status: TurnStatus,
    pub settled_at: Option<SessionTimestamp>,
}

/// The server facts from which a listed Session's Standing is derived. Kept as
/// one value so every catalog change replaces the reading whole and Viewed
/// can be compared with the matching latest Turn.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStandingInputs {
    /// Live Questionnaires and Approvals owned by descendants at any depth.
    #[serde(default)]
    pub subagent_interventions: Vec<SubagentInterventions>,
    /// Live Questionnaires awaiting an Answer in this Session.
    #[serde(default)]
    pub pending_questionnaires: Vec<QuestionnaireId>,
    /// Accepted submissions still awaiting Provider delivery; drafts stay disabled.
    #[serde(default)]
    pub submitting_questionnaires: Vec<QuestionnaireId>,
    /// Session revision at which live Questionnaire availability last changed.
    #[serde(default)]
    pub pending_questionnaires_revision: SessionRevision,
    /// Live Approvals awaiting a Decision in this Session.
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalId>,
    /// Accepted Decisions still awaiting Provider delivery.
    #[serde(default)]
    pub submitting_approvals: Vec<ApprovalId>,
    /// Session revision at which live Approval availability last changed.
    #[serde(default)]
    pub pending_approvals_revision: SessionRevision,
    #[serde(default)]
    pub latest_turn: Option<LatestTurnStatus>,
    /// When any Client last reported this Session open in its main view.
    /// Shared server state rather than per-Client presentation.
    #[serde(default)]
    pub viewed_at: Option<SessionTimestamp>,
}

impl SessionStandingInputs {
    pub fn pending_questionnaire_count(&self) -> usize {
        self.pending_questionnaires.len()
            + self
                .subagent_interventions
                .iter()
                .map(|entry| entry.pending_questionnaires.len())
                .sum::<usize>()
    }

    pub fn pending_approval_count(&self) -> usize {
        self.pending_approvals.len()
            + self
                .subagent_interventions
                .iter()
                .map(|entry| entry.pending_approvals.len())
                .sum::<usize>()
    }

    pub(crate) fn from_turns(turns: &[Turn]) -> Self {
        Self {
            subagent_interventions: Vec::new(),
            pending_questionnaires: Vec::new(),
            submitting_questionnaires: Vec::new(),
            pending_questionnaires_revision: SessionRevision(0),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: SessionRevision(0),
            latest_turn: turns.last().map(|turn| LatestTurnStatus {
                status: turn.status,
                settled_at: turn.settled_at,
            }),
            viewed_at: None,
        }
    }

    pub(crate) fn latest_turn_settled_as(&self, status: TurnStatus) -> bool {
        self.latest_turn.is_some_and(|latest| {
            latest.status == status
                && latest.settled_at.is_some_and(|settled_at| {
                    self.viewed_at.is_none_or(|viewed| settled_at > viewed)
                })
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    /// Ephemeral owning-Server reading, absent until checkout observation.
    #[serde(default)]
    pub checkout_state: Option<CheckoutSummary>,
    #[serde(flatten)]
    pub session: Session,
    pub title: String,
    /// The Icon Catalog name standing beside this Session's Title, derived
    /// with that Title and carried apart from it so a reader searching a
    /// listing matches the words rather than the glyph in front of them.
    /// Absent for every Session whose derivation was skipped, failed, or
    /// predates the feature. Resolved to a glyph only at draw time, and
    /// drawn nowhere at all where the Catalog no longer carries the name.
    #[serde(default)]
    pub icon: Option<String>,
    /// When the user set this Session aside as done for now, and `None` while
    /// it is active. The marker and the moment are one field because a Session
    /// is settled exactly when there is a moment it was settled at — and a
    /// reader ordering the Sessions that were set aside needs that moment as
    /// much as the fact of it.
    #[serde(default)]
    pub settled_at: Option<SessionTimestamp>,
    /// The latest Turn facts used to derive this Session's Standing, and no
    /// latest Turn for a Session whose first Prompt has not begun one. Derived
    /// from the Turns rather than stored beside them, just like Working.
    #[serde(default)]
    pub standing_inputs: SessionStandingInputs,
    /// What this Session and its Subagent subtree have consumed together, and
    /// `None` where nothing has reported anything — which is every Session
    /// stored before Usage recording.
    ///
    /// Derived from the Turns rather than stored beside them, exactly as the
    /// Session's `working_since` is, so a listing can never disagree with the
    /// Session's own Transcript.
    #[serde(default)]
    pub total_usage: Option<UsageTotal>,
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
    /// Boxed because a readable summary carries everything a Session states
    /// about itself — its Selection, its Workspace, its total Usage — while
    /// the unreadable one carries what little survived a Session Suru could
    /// not decode.
    Readable(Box<SessionSummary>),
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

    /// The Icon Catalog name standing for this Session, when it has one. A
    /// Session Suru could not read carries none, because an Icon is stored
    /// beside a Title that only a readable Session has.
    pub fn icon(&self) -> Option<&str> {
        match self {
            Self::Readable(summary) => summary.icon.as_deref(),
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

    /// When this Session's uninterrupted Working interval began across its
    /// whole Subagent subtree, and `None` where nothing is working. A Session
    /// Suru could not read is never working, because work it cannot read is
    /// work it cannot run.
    pub const fn working_since(&self) -> Option<SessionTimestamp> {
        match self {
            Self::Readable(summary) => summary.session.working_since,
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
    pub workspace_paths: WorkspacePaths,
    pub revision: SessionCatalogRevision,
    pub session_ids: Vec<SessionId>,
    /// One current reading per Worktree represented by the catalog. A client
    /// takes this set whole when it joins or rejoins the stream.
    pub checkout_states: Vec<CheckoutSummary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCatalogChange {
    /// Stored content proved unreadable; clients refresh the containing root.
    Invalidated {
        session_id: SessionId,
    },
    /// The owning Server's current reading of one Worktree.
    /// This is live presentation state rather than the recovery revision
    /// retained with the Session, so clients take it whole and may clear it.
    CheckoutStateChanged {
        checkout_id: CheckoutId,
        checkout_state: Option<CheckoutSummary>,
    },
    Created {
        session_id: SessionId,
    },
    Deleted {
        session_id: SessionId,
    },
    /// A Session's Title — and the Icon standing beside it — was replaced by
    /// a derivation. It rides the catalog stream rather than the Session's own,
    /// because a client subscribes only to the Sessions it has open while the
    /// Title it draws is for every Session it lists.
    TitleChanged {
        session_id: SessionId,
        title: String,
        icon: Option<String>,
    },
    /// A Session was set aside as done for now, or brought back. It rides the
    /// catalog stream for the same reason a Title does: every client lists the
    /// Session, and only some have it open.
    SettlementChanged {
        session_id: SessionId,
        settled_at: Option<SessionTimestamp>,
    },
    /// A Session's uninterrupted subtree Working interval began or ended, so
    /// what [`Session::working_since`] reads changed. It rides the catalog
    /// stream for the same reason the others do — every client lists the
    /// Session, and only some have it open — and it carries the new reading
    /// whole, so a listing in hand says the right thing before the ask that
    /// carries what the commit's own stamp moved.
    WorkingChanged {
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    },
    /// The inputs from which a listed Session's Standing is read changed when
    /// its latest Turn Settled or its live Questionnaires changed. It carries
    /// the reading whole so every client
    /// can revise its listing in place without asking for it again first.
    StandingInputsChanged {
        session_id: SessionId,
        inputs: SessionStandingInputs,
    },
    /// A Session's total — its own Turns and its Subagent subtree together —
    /// moved. It rides the catalog stream for the same reason Working does:
    /// every client lists the Session, and only some have it open. It carries
    /// the whole reading, and moves nothing else about the row, so a listing
    /// in hand is current the moment it lands.
    UsageChanged {
        session_id: SessionId,
        total_usage: Option<UsageTotal>,
    },
    /// A Workspace's Icon was derived, or — once issue #360 lands a way to
    /// choose one — set. It rides the catalog stream because every client
    /// listing a Session rooted in that Workspace draws the same Icon beside
    /// it, whether or not that Session is open, and the Landing draws it
    /// beside the Workspace the client currently works in.
    WorkspaceIconChanged {
        workspace_id: WorkspaceId,
        icon: Option<String>,
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

/// What one interrupt actually did. A Session that is Working only because it
/// owes a Turn to a Prompt it has not delivered has no Turn to stop, so
/// interrupting it withdraws that Prompt instead; the client that asked is
/// told which Prompt so it can return the text to its own composer, where
/// every other viewer only sees the Prompt go (ADR 0024).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "outcome")]
pub enum InterruptOutcome {
    /// The interrupt stopped work already under way: an active Turn, or the
    /// Subagents that outlived one.
    StoppedWork,
    /// The interrupt withdrew a Prompt that had been admitted to begin a Turn
    /// and never delivered. The Prompt is carried as it now stands, Cancelled.
    WithdrewPrompt { prompt: Prompt },
}

/// The disjoint measurements a Provider reported for one Turn. An absent
/// field means the Provider did not state it; a reported zero remains
/// `Some(0)`, so absence is never fabricated into a number.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// A Provider-native usage figure retained for a future Provider-specific
    /// surface. Its unit belongs to that Provider and is deliberately not
    /// interpreted as dollars here.
    pub native_meter: Option<NativeMeter>,
    pub model_context_window: Option<u64>,
}

/// A non-negative figure in a Provider's own metering unit. Like [`Cost`], it
/// uses billionths internally so a fractional native figure remains exact
/// across protocol and persistence round-trips; unlike Cost, its unit is not
/// assumed to be dollars.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeMeter(u64);

impl NativeMeter {
    const FRACTIONS_PER_UNIT: f64 = 1_000_000_000.0;

    pub fn from_units(units: f64) -> Option<Self> {
        if !units.is_finite() || units < 0.0 || units > u64::MAX as f64 / Self::FRACTIONS_PER_UNIT {
            return None;
        }
        Some(Self((units * Self::FRACTIONS_PER_UNIT).round() as u64))
    }

    pub fn as_units(self) -> f64 {
        self.0 as f64 / Self::FRACTIONS_PER_UNIT
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }
}

impl Serialize for NativeMeter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.as_units())
    }
}

impl<'de> Deserialize<'de> for NativeMeter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let units = f64::deserialize(deserializer)?;
        Self::from_units(units)
            .ok_or_else(|| D::Error::custom("native meter must be a non-negative figure"))
    }
}

impl Usage {
    /// The compact figure shown to a reader: fresh input plus output and the
    /// reasoning within it. Cache traffic stays in the stored breakdown but
    /// does not inflate the displayed count.
    pub fn blended_tokens(&self) -> Option<u64> {
        blended_tokens(
            self.fresh_input_tokens,
            self.output_tokens,
            self.reasoning_tokens,
        )
    }
}

/// The one blend every surface states, taken in one place so a Turn's figure
/// and a Session's total can never be blended differently: fresh input plus
/// output and the reasoning within it, and absent where none of the three was
/// measured.
fn blended_tokens(
    fresh_input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
) -> Option<u64> {
    [fresh_input, output, reasoning]
        .into_iter()
        .flatten()
        .reduce(u64::saturating_add)
}

/// A non-negative dollar figure stored in billionths of one USD. The integer
/// representation keeps protocol equality and persistence exact while
/// retaining more precision than a compact surface can display.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Cost(u64);

impl Cost {
    const NANO_USD_PER_USD: f64 = 1_000_000_000.0;

    /// Admits a Provider's native USD figure, rejecting unknown, non-finite,
    /// negative, and unrepresentable values at the Provider boundary. A
    /// reported zero remains distinct from an absent, unknown Cost.
    pub fn from_usd(usd: f64) -> Option<Self> {
        if !usd.is_finite() || usd < 0.0 || usd > u64::MAX as f64 / Self::NANO_USD_PER_USD {
            return None;
        }
        Some(Self::from_nano_usd(
            (usd * Self::NANO_USD_PER_USD).round() as u64
        ))
    }

    pub const fn from_nano_usd(nano_usd: u64) -> Self {
        Self(nano_usd)
    }

    pub const fn nano_usd(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Adds one Cost to another, holding at the largest representable figure
    /// rather than wrapping. A total that ran past every dollar a Provider
    /// could charge is a figure nobody will read, but it must not turn into a
    /// small one.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub fn as_usd(self) -> f64 {
        self.nano_usd() as f64 / Self::NANO_USD_PER_USD
    }
}

impl Serialize for Cost {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.as_usd())
    }
}

impl<'de> Deserialize<'de> for Cost {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let usd = f64::deserialize(deserializer)?;
        Self::from_usd(usd)
            .ok_or_else(|| D::Error::custom("Cost must be a non-negative USD figure"))
    }
}

/// Who computed a stored Cost. This is independent of whether an account's
/// billing arrangement made the Turn marginally chargeable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Reported,
    Estimated,
}

/// The work one frozen Cost accounts for. A Turn measurement covers only the
/// Turn carrying it. A Session-subtree measurement is cumulative within one
/// Provider reporting lifetime and includes that Session and every descendant.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CostCoverage {
    Turn,
    SessionSubtree { reporting_lifetime: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostRecord {
    pub cost: Cost,
    pub basis: CostBasis,
    pub coverage: CostCoverage,
    pub recorded_at: SessionTimestamp,
    pub is_partial: bool,
}

/// Attribution accompanying a Turn's current Cost. Earlier independent
/// reporting lifetimes remain as records so reconnecting cannot erase frozen
/// historical amounts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostDetails {
    pub coverage: CostCoverage,
    pub recorded_at: SessionTimestamp,
    pub is_partial: bool,
    #[serde(default)]
    pub prior: Vec<CostRecord>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CostTotal {
    pub cost: Cost,
    pub is_partial: bool,
}

/// What a set of Turns consumed together: the token parts that add up, and
/// the Cost each Turn froze when it recorded its Usage. It is what a surface
/// states for a whole Session — its own Turns, and its Subagent subtree with
/// them.
///
/// Absence survives the sum, as it does on a Turn's [`Usage`]: a part no Turn
/// in the set reported stays absent rather than becoming a zero nobody
/// measured. Two of that Usage's fields are deliberately not here, because
/// neither is a quantity to add: a model context window belongs to one Turn's
/// model, and a native meter is in a unit only its Provider defines — the one
/// Provider that reports it already counts a whole Session, so summing two
/// would count the same spend twice.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTotal {
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cost: Option<Cost>,
    #[serde(default)]
    pub cost_is_partial: bool,
}

impl UsageTotal {
    /// What one Turn contributes, or `None` where it recorded nothing at all —
    /// every Turn stored before Usage recording, and every Turn whose Provider
    /// never reported a measurement.
    pub fn of_turn(turn: &Turn) -> Option<Self> {
        let usage = turn.usage.as_ref();
        if usage.is_none() && turn.cost.is_none() {
            return None;
        }
        Some(Self {
            fresh_input_tokens: usage.and_then(|usage| usage.fresh_input_tokens),
            cache_read_tokens: usage.and_then(|usage| usage.cache_read_tokens),
            cache_write_tokens: usage.and_then(|usage| usage.cache_write_tokens),
            output_tokens: usage.and_then(|usage| usage.output_tokens),
            reasoning_tokens: usage.and_then(|usage| usage.reasoning_tokens),
            cost: turn.cost,
            cost_is_partial: turn
                .cost_details
                .as_ref()
                .is_some_and(|details| details.is_partial)
                || usage.is_some() && turn.cost.is_none(),
        })
    }

    /// What every Turn in `turns` consumed together, or `None` where none of
    /// them recorded anything — which is what keeps a Session with no Usage
    /// off every surface that would otherwise state a zero.
    pub fn of_turns<'a>(turns: impl IntoIterator<Item = &'a Turn>) -> Option<Self> {
        turns
            .into_iter()
            .filter_map(Self::of_turn)
            .reduce(Self::saturating_add)
    }

    /// Adds two totals part by part, holding at the largest representable
    /// figure rather than wrapping.
    ///
    /// A summed Cost is over the Turns that had one: a Turn whose price was
    /// never known contributes no dollars rather than voiding the figure
    /// beside it, which is how a Session already totals its own Turns. Only a
    /// total where nothing at all was priced is absent.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            fresh_input_tokens: add_parts(self.fresh_input_tokens, other.fresh_input_tokens),
            cache_read_tokens: add_parts(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: add_parts(self.cache_write_tokens, other.cache_write_tokens),
            output_tokens: add_parts(self.output_tokens, other.output_tokens),
            reasoning_tokens: add_parts(self.reasoning_tokens, other.reasoning_tokens),
            cost: match (self.cost, other.cost) {
                (None, None) => None,
                (held, added) => Some(
                    held.unwrap_or(Cost::from_nano_usd(0))
                        .saturating_add(added.unwrap_or(Cost::from_nano_usd(0))),
                ),
            },
            cost_is_partial: self.cost_is_partial || other.cost_is_partial,
        }
    }

    /// The compact figure a reader is shown, on the same terms as one Turn's
    /// [`Usage::blended_tokens`].
    pub fn blended_tokens(&self) -> Option<u64> {
        blended_tokens(
            self.fresh_input_tokens,
            self.output_tokens,
            self.reasoning_tokens,
        )
    }
}

/// Adds one measured part to another, keeping absence where neither side
/// measured anything.
fn add_parts(held: Option<u64>, added: Option<u64>) -> Option<u64> {
    match (held, added) {
        (None, None) => None,
        (held, added) => Some(held.unwrap_or(0).saturating_add(added.unwrap_or(0))),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "TurnWire")]
pub struct Turn {
    pub id: TurnId,
    /// The Prompt whose delivery began this Turn, absent on a Turn no Prompt
    /// began — a Continuation, or a Subagent Session's Turn opened by its
    /// spawn.
    pub prompt_id: Option<PromptId>,
    pub agent: Option<AgentIdentity>,
    pub status: TurnStatus,
    /// When the commit that delivered this Turn's opening Prompt landed, and
    /// when the commit that settled it landed. Both are absent on a Turn stored
    /// before Suru recorded Turn timing, so a client states how long a Turn
    /// worked only when it knows.
    pub started_at: Option<SessionTimestamp>,
    pub settled_at: Option<SessionTimestamp>,
    /// Latest durable evidence that this Turn produced substantive output.
    #[serde(default)]
    pub last_output_at: Option<SessionTimestamp>,
    /// Absent on Turns stored before usage recording and on Turns whose
    /// Provider never reported a measurement.
    pub usage: Option<Usage>,
    /// Frozen when Usage is recorded; absent when no reliable dollar figure
    /// was available. A reported zero is known and distinct from absence.
    pub cost: Option<Cost>,
    pub cost_basis: Option<CostBasis>,
    #[serde(default)]
    pub cost_details: Option<CostDetails>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnWire {
    id: TurnId,
    prompt_id: Option<PromptId>,
    agent: Option<AgentIdentity>,
    status: TurnStatus,
    started_at: Option<SessionTimestamp>,
    settled_at: Option<SessionTimestamp>,
    #[serde(default)]
    last_output_at: Option<SessionTimestamp>,
    usage: Option<Usage>,
    cost: Option<Cost>,
    cost_basis: Option<CostBasis>,
    #[serde(default)]
    cost_details: Option<CostDetails>,
}

impl TryFrom<TurnWire> for Turn {
    type Error = &'static str;

    fn try_from(turn: TurnWire) -> Result<Self, Self::Error> {
        if turn.cost.is_some() != turn.cost_basis.is_some() {
            return Err("a Turn Cost requires exactly one Cost Basis");
        }
        Ok(Self {
            id: turn.id,
            prompt_id: turn.prompt_id,
            agent: turn.agent,
            status: turn.status,
            started_at: turn.started_at,
            settled_at: turn.settled_at,
            last_output_at: turn.last_output_at,
            usage: turn.usage,
            cost: turn.cost,
            cost_basis: turn.cost_basis,
            cost_details: turn.cost_details,
        })
    }
}

impl Turn {
    pub const fn has_valid_cost_attribution(&self) -> bool {
        self.cost.is_some() == self.cost_basis.is_some()
    }

    /// Whether this Turn is a Continuation: the one kind of Turn that begins
    /// without a Prompt. Named once here so every place that treats
    /// Continuations apart — admission, the steer sweep — asks the same
    /// question. A Subagent Session's Turn also carries no Prompt, but those
    /// Sessions take no Prompts at all, so the question never arises there.
    pub const fn is_continuation(&self) -> bool {
        self.prompt_id.is_none()
    }
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

/// Live intervention availability from one descendant, without its Approval
/// detail, Questions, Answers, or Decisions. Every ancestor names the
/// immediate child through which it is reached; the revision belongs to the
/// owning descendant Session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentInterventions {
    pub session_id: SessionId,
    pub via_session_id: SessionId,
    pub revision: SessionRevision,
    pub pending_questionnaires: Vec<QuestionnaireId>,
    pub submitting_questionnaires: Vec<QuestionnaireId>,
    pub pending_approvals: Vec<ApprovalId>,
    pub submitting_approvals: Vec<ApprovalId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub title: String,
    /// The Icon Catalog name standing beside this Session's Title. See
    /// [`SessionSummary::icon`].
    #[serde(default)]
    pub icon: Option<String>,
    pub session: Session,
    pub revision: SessionRevision,
    pub prompts: Vec<Prompt>,
    pub turns: Vec<Turn>,
    pub messages: Vec<Message>,
    pub activities: Vec<Activity>,
    pub transcript: Vec<TranscriptItem>,
    /// What this Session's Subagent subtree consumed, to any depth, and
    /// `None` while it has delegated nothing that reported anything. It is
    /// held apart from the Session's own Turns because only the server can
    /// see across Sessions: a child's Usage lands in the child's own Turns,
    /// and the server rolls it up here so any client reading this Session
    /// states the whole of what its work cost.
    #[serde(default)]
    pub subagent_usage: Option<UsageTotal>,
    /// Authoritative Cost for this Session tree after applying Cost Coverage.
    /// Derived from durable Turn measurements by the owning server.
    #[serde(default)]
    pub total_cost: Option<CostTotal>,
    /// Live Questionnaires and Approvals owned by descendants at any depth.
    #[serde(default)]
    pub subagent_interventions: Vec<SubagentInterventions>,
    /// Answerable Approvals owned by this Session.
    #[serde(default)]
    pub pending_approvals: Vec<ApprovalId>,
    /// Approvals whose accepted Decision is awaiting Provider delivery.
    #[serde(default)]
    pub submitting_approvals: Vec<ApprovalId>,
    /// Revision at which either live Approval list last changed.
    #[serde(default)]
    pub pending_approvals_revision: SessionRevision,
}

impl SessionSnapshot {
    pub fn subagent_questionnaire_count(&self) -> usize {
        self.subagent_interventions
            .iter()
            .map(|entry| entry.pending_questionnaires.len())
            .sum()
    }

    pub fn pending_questionnaires_in_subagent(&self, session_id: SessionId) -> usize {
        self.subagent_interventions
            .iter()
            .filter(|entry| entry.via_session_id == session_id)
            .map(|entry| entry.pending_questionnaires.len())
            .sum()
    }

    pub fn subagent_approval_count(&self) -> usize {
        self.subagent_interventions
            .iter()
            .map(|entry| entry.pending_approvals.len())
            .sum()
    }

    pub fn pending_approvals_in_subagent(&self, session_id: SessionId) -> usize {
        self.subagent_interventions
            .iter()
            .filter(|entry| entry.via_session_id == session_id)
            .map(|entry| entry.pending_approvals.len())
            .sum()
    }

    /// When this Session's uninterrupted Working interval began, using the
    /// server-derived subtree reading carried by the Session itself.
    pub const fn working_since(&self) -> Option<SessionTimestamp> {
        self.session.working_since
    }

    /// Everything this Session has consumed: its own Turns — failed and
    /// interrupted ones included — and the Subagent subtree rolled up beneath
    /// them. It is the one reading [`SessionSummary::total_usage`] carries,
    /// taken here so a listing and an open Session can never tell a reader
    /// different things about the same spend.
    pub fn total_usage(&self) -> Option<UsageTotal> {
        let mut total = match (UsageTotal::of_turns(&self.turns), self.subagent_usage) {
            (Some(own), Some(delegated)) => Some(own.saturating_add(delegated)),
            (own, delegated) => own.or(delegated),
        };
        if let Some(cost) = self.total_cost {
            let total = total.get_or_insert_with(UsageTotal::default);
            total.cost = Some(cost.cost);
            total.cost_is_partial = cost.is_partial;
        }
        total
    }
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
    WorkspaceChanged {
        workspace: Workspace,
        checkout: Option<CheckoutAssociation>,
    },
    TitleChanged {
        title: String,
        icon: Option<String>,
    },
    ContextFillChanged {
        context_fill: Option<ContextFill>,
    },
    AgentSelectionChanged {
        selection: AgentSelection,
    },
    AgentSelectionAvailabilityChanged {
        availability: ModelAvailability,
    },
    ApprovalPostureChanged {
        approval_posture: Option<SessionApprovalPosture>,
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
    /// Provider-confirmed identity for a Subagent's prompt-less Turn. Unlike
    /// an Agent Selection change, this is observation and is valid only for a
    /// child Session.
    SubagentAgentChanged {
        turn_id: TurnId,
        agent: AgentIdentity,
    },
    TurnUsageChanged {
        turn_id: TurnId,
        usage: Usage,
        cost: Option<Cost>,
        cost_basis: Option<CostBasis>,
        cost_coverage: Option<CostCoverage>,
        cost_is_partial: bool,
        /// Filled by the authoritative store when the change commits.
        cost_recorded_at: Option<SessionTimestamp>,
    },
    /// Filled by the authoritative store when Provider output commits.
    TurnOutputObserved {
        turn_id: TurnId,
        observed_at: SessionTimestamp,
    },
    /// Live intervention availability in this Session's Subagent subtree,
    /// rolled up whole by the server. It carries the new reading entire rather
    /// than a delta, because a client holding it must never have to reconstruct
    /// a descendant lifecycle it may have joined too late to observe.
    SubagentInterventionsChanged {
        subagent_interventions: Vec<SubagentInterventions>,
    },
    SubagentUsageChanged {
        subagent_usage: Option<UsageTotal>,
    },
    TotalCostChanged {
        total_cost: Option<CostTotal>,
    },
    /// The whole Working reading derived by the server across this Session's
    /// Subagent subtree. It is carried as one value so a client joining an
    /// update stream never has to reconstruct work it did not observe begin.
    SessionWorkingChanged {
        working_since: Option<SessionTimestamp>,
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
    QuestionnaireAccepted {
        activity_id: ActivityId,
    },
    QuestionnaireSettled {
        activity_id: ActivityId,
        outcome: QuestionnaireOutcome,
        answer: Option<Answer>,
    },
    DecisionAccepted {
        activity_id: ActivityId,
    },
    ApprovalSettled {
        activity_id: ActivityId,
        outcome: ApprovalOutcome,
        decision: Option<Decision>,
    },
    ApprovalFollowUpFailed {
        activity_id: ActivityId,
        error: String,
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
    SubagentDescriptionChanged {
        activity_id: ActivityId,
        description: String,
    },
    SubagentModelChanged {
        activity_id: ActivityId,
        model: ModelId,
    },
    SubagentStatusChanged {
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
    #[serde(default)]
    pub preparation_id: Option<PreparationId>,
    pub agent_selection: Option<AgentSelection>,
    pub execution_directory: ExecutionDirectory,
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

/// The user's own choice of a Session's Icon, by Icon Catalog name. Refused
/// where the Catalog does not carry that name, so a Session never stores an
/// Icon that would only ever draw as absent. Unlike a derived Icon, which only
/// ever fills an absence, a chosen Icon replaces whatever the Session already
/// carried — a user's choice always stands.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetSessionIconRequest {
    pub icon: String,
}

/// The user's own choice of a Workspace's Icon, by Icon Catalog name and the
/// Workspace's identity — carried in the body rather than a URL path segment,
/// since a [`WorkspaceId`]'s inner string may itself contain path characters.
/// Refused where the Catalog does not carry the named Icon, or where this
/// server knows no such Workspace at all. Unlike a derived Icon, which only
/// ever fills an absence, a chosen Icon replaces whatever the Workspace
/// already carried — a user's choice always stands.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetWorkspaceIconRequest {
    pub workspace_id: WorkspaceId,
    pub icon: String,
}

/// One report that a Client has a root Session open in its main view. The
/// identity makes transport retries one operation rather than later Views.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSessionRequest {
    pub operation_id: ViewSessionOperationId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgentSelectionRequest {
    pub operation_id: AgentSelectionOperationId,
    pub selection: AgentSelection,
}

/// Pins one Provider-native Approval Posture on a Session, or resets it to
/// follow the current Server Setting when `posture` is absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateApprovalPostureRequest {
    pub posture: Option<ApprovalPosture>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    InvalidCommand,
    EmptyPrompt,
    InvalidWorkspace,
    /// A chosen Icon named a Catalog entry the Icon Catalog does not carry.
    InvalidIcon,
    SessionNotFound,
    SubagentSession,
    /// Deletion was refused because this Session or a Subagent below it is
    /// still Working.
    WorkingSession,
    PromptConflict,
    PromptNotFound,
    PromptNotPending,
    /// An interrupt found nothing running: no active Turn, and no working
    /// Subagent anywhere below the Session.
    NothingToInterrupt,
    InterruptionFailed,
    QuestionnaireSubmissionFailed,
    DecisionSubmissionFailed,
    /// A per-Subagent stop named a Subagent whose Provider offers none.
    SubagentStopUnsupported,
    AgentSelectionOperationConflict,
    AgentSelectionProviderConflict,
    InvalidSkillInvocation,
    ConfigRootUnavailable,
    ConfigDocumentNotEditable,
    ConfigDocumentWriteFailed,
    ServingListenerFailed,
    InvalidInvite,
    UnsupportedInviteVersion,
    InviteExpired,
    InviteSpent,
    InviteSuperseded,
    InvalidInviteAddresses,
    InvalidRemoteName,
    RemoteNameConflict,
    RemoteNotFound,
    PeerNotFound,
    PairingConnectionFailed,
    PairingAuthenticationFailed,
    PairingProtocolMismatch,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionError {}

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
    pub workspace_paths: WorkspacePaths,
    pub lifecycle: LifecycleState,
    pub landing_agent_selection: Option<AgentSelection>,
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl Health {
    pub fn new(identity: ServerIdentity, lifecycle: LifecycleState) -> Self {
        Self {
            lifecycle,
            workspace_paths: WorkspacePaths::default(),
            landing_agent_selection: None,
            identity,
        }
    }

    pub fn with_workspace_paths(mut self, workspace_paths: WorkspacePaths) -> Self {
        self.workspace_paths = workspace_paths;
        self
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
    workspace_paths: WorkspacePaths,
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
        .with_landing_agent_selection(wire.landing_agent_selection)
        .with_workspace_paths(wire.workspace_paths))
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

/// A Session joined the catalog, carried to every client listing Sessions
/// whether or not it made the Session itself. It names the Session and no
/// more: what a row draws — a Title, a Workspace, when the work was made —
/// comes with the listing a client asks for in answer, because the catalog
/// carries the changes to a body of work rather than the work itself.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreated {
    pub session_id: SessionId,
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

/// A Session's uninterrupted subtree Working interval began or ended, carried
/// to a client that may be listing that Session without having it open: what a
/// listing says live work has been running for is only true while someone
/// announces it changing.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkingChanged {
    pub session_id: SessionId,
    pub working_since: Option<SessionTimestamp>,
}

/// The owning Server's current reading of one Worktree, carried independently
/// of whether any listing surface is open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutStateChanged {
    pub checkout_id: CheckoutId,
    pub checkout_state: Option<CheckoutSummary>,
}

/// A Session's complete Standing input, carried to a client that may be
/// listing that Session without having it open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStandingInputsChanged {
    pub session_id: SessionId,
    pub inputs: SessionStandingInputs,
}

/// A Session's total Usage — its own Turns and its Subagent subtree together
/// — as the server rolled it up, carried to a client that may be listing that
/// Session without having it open.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageChanged {
    pub session_id: SessionId,
    pub total_usage: Option<UsageTotal>,
}

/// A Session's Title — and the Icon beside it — as a derivation left them,
/// carried to a client that may be listing that Session without having it open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTitleChanged {
    pub session_id: SessionId,
    pub title: String,
    pub icon: Option<String>,
}

/// A Workspace's Icon as a derivation — or, later, a choice — left it, carried
/// to every client that may list a Session rooted there without having any of
/// them open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIconChanged {
    pub workspace_id: WorkspaceId,
    pub icon: Option<String>,
}
