use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv6Addr},
    ops::Range,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use uuid::Uuid;

mod workspace_paths;
pub use workspace_paths::{MANAGED_WORKTREE_DIRECTORY, PathStyle, WorkspacePaths};

pub const PROTOCOL_VERSION: u32 = 82;
mod attachment;
mod source_control;
mod standing;
pub use crate::approval::{Approval, ApprovalOutcome, ApprovalSubject, CommandAction, Decision};
pub use crate::questionnaire::{
    Answer, Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireOutcome,
    QuestionnaireSubmission,
};
pub use attachment::*;
pub use source_control::*;
pub use standing::{SessionStanding, StandingReading};
/// The response header naming a [`SessionError`]'s code beside its body, so
/// an answer that carries no body, such as one to a `HEAD`, still says why it
/// was refused.
pub const SESSION_ERROR_CODE_HEADER: &str = "x-suru-error-code";
pub const SERVER_SHUTDOWN_EVENT: &str = "server_shutdown";
pub const SETTINGS_SNAPSHOT_EVENT: &str = "settings_snapshot";
pub const MODEL_CATALOG_EVENT: &str = "model_catalog";
pub const SKILL_CATALOG_UPDATED_EVENT: &str = "skill_catalog_updated";
pub const SESSION_CATALOG_SNAPSHOT_EVENT: &str = "session_catalog_snapshot";
pub const SESSION_CATALOG_UPDATED_EVENT: &str = "session_catalog_updated";
pub const SESSION_SNAPSHOT_EVENT: &str = "session_snapshot";
pub const SESSION_UPDATED_EVENT: &str = "session_updated";
pub const SUBAGENT_TREE_SNAPSHOT_EVENT: &str = "subagent_tree_snapshot";
pub const SUBAGENT_TREE_UPDATED_EVENT: &str = "subagent_tree_updated";

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

/// Where one per-tree subscription stands in the changes it has carried. It
/// counts within one subscription's life only: every connection begins with a
/// snapshot, which is authoritative whatever revision it names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SubagentTreeRevision(pub u64);

impl SubagentTreeRevision {
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
    /// Boxed because a Repository's capabilities and location outweigh the
    /// rest of the Workspace several times over, and a Workspace rides along
    /// in many enums that would otherwise all be sized to it.
    pub repository: Option<Box<Repository>>,
    pub source_control: SourceControlAvailability,
    /// The Workspace's Icon, as the `workspaces` table holds it: absent until
    /// a derivation lands one for good. It rides every copy of a Workspace —
    /// discovery, a listed Session, an open Session's own — because the table
    /// is its one source of truth (see the **Icon** glossary entry and ADR
    /// 0027 for why there is no broader Workspace registry beside it).
    pub icon: Option<String>,
    /// The Workspace's Description, as the same `workspaces` table holds it:
    /// absent until a derivation lands one or someone sets one, and carried
    /// on every copy of a Workspace for the same reason its Icon is.
    pub description: Option<WorkspaceDescription>,
}

impl Workspace {
    pub fn directory(path: PathBuf) -> Self {
        Self {
            id: WorkspaceId::directory(&path),
            path,
            repository: None,
            source_control: SourceControlAvailability::NotDetected,
            icon: None,
            description: None,
        }
    }
    pub fn main_unknown(&self) -> bool {
        self.repository
            .as_ref()
            .is_some_and(|repo| matches!(repo.location, RepositoryLocation::UnknownMain))
    }
    /// Whether `named` names this Workspace, as a caller naming one by text
    /// names it: by its identity, or by the path it is presented by, each
    /// exactly as given — `atlas ` and `atlas` may be two directories.
    pub fn is_named_by(&self, named: &str) -> bool {
        self.id.0 == named || self.path == Path::new(named)
    }

    /// Whether `named` names this Workspace of another Server: by its
    /// identity, or by the very text its path is spelled in. That Server's
    /// path syntax is its own, so this machine's has no say in what names it
    /// — `/repo/a\b` and `/repo/a/b` are two directories on a Unix Remote,
    /// whatever a Windows reader's paths make of them.
    pub fn is_spelled_by(&self, named: &str) -> bool {
        self.id.0 == named || self.path.as_os_str() == std::ffi::OsStr::new(named)
    }
}

/// The Workspaces `workspaces` name, each once, in the order each is first
/// named — which is how a listing of Sessions, newest work first, offers the
/// Workspaces its Sessions work in.
pub fn distinct_workspaces<'a>(
    workspaces: impl IntoIterator<Item = &'a Workspace>,
) -> Vec<Workspace> {
    let mut distinct: Vec<Workspace> = Vec::new();
    for workspace in workspaces {
        if !distinct.iter().any(|known| known.id == workspace.id) {
            distinct.push(workspace.clone());
        }
    }
    distinct
}
impl From<PathBuf> for Workspace {
    fn from(path: PathBuf) -> Self {
        Self::directory(path)
    }
}

/// The most characters a Workspace's Description runs to, counted on it as
/// it is kept (see [`one_line_description`]): a set one running longer is
/// refused, saying so in [`description_too_long`]'s words, and a derived one
/// is cut to it. A Description is listed to Agents and drawn in a couple of
/// lines, so it stays a sentence or two.
pub const MAX_WORKSPACE_DESCRIPTION_CHARS: usize = 300;

/// A Workspace's Description as it is kept and counted: on one line, every
/// run of whitespace — line breaks included — collapsed to a single space,
/// and none at either end. The Server keeps what it is given this way and
/// measures [`MAX_WORKSPACE_DESCRIPTION_CHARS`] against the result, and a
/// Client counts what a reader writes the same way, so the two never
/// disagree about what fits. Text with nothing left once collapsed is no
/// Description at all.
pub fn one_line_description(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The sentence a Description `chars` long — counted as
/// [`one_line_description`] keeps it — is refused with, whoever refuses it.
pub fn description_too_long(chars: usize) -> String {
    format!(
        "A Description runs to at most {MAX_WORKSPACE_DESCRIPTION_CHARS} characters, and this \
         one runs to {chars}; say it in a sentence or two"
    )
}

/// A sentence or two saying what a Workspace is for (see the **Description**
/// glossary entry), and whether the user or a Sidekick set it rather than an
/// Errand deriving it. A set Description stands against every later
/// derivation; a derived one only ever filled an absence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDescription {
    pub text: String,
    pub set: bool,
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

/// The byte range of a Prompt's original UTF-8 text that stands for something
/// bound beside that text, such as the `$skill-name` a Skill Invocation was
/// written as. The end is exclusive, matching Rust string ranges.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

impl TextSpan {
    /// The span as byte offsets into the Prompt text.
    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

impl From<Range<usize>> for TextSpan {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start as u32,
            end: range.end as u32,
        }
    }
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
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInvocation {
    pub skill_id: SkillId,
    pub name: String,
    pub scope: Option<String>,
    /// Where the `$skill-name` it was written as stands in the Prompt text.
    pub span: TextSpan,
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

/// Whether a Transcript draws the Tool Calls a Session stored.
///
/// Shown is the built-in default: a Tool Call is work the agent did on the
/// reader's behalf, and a Transcript leads with the work. Hiding them is a
/// reader's deliberate choice of a quieter Transcript.
///
/// Either way it is presentation and nothing more: Tool Calls keep arriving
/// and keep being stored, so a reader who shows them again is shown every one
/// that arrived while they were hidden.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallVisibility {
    #[default]
    Shown,
    Hidden,
}

/// How a Transcript gathers runs of Commands, Tool Calls, and Reasoning into
/// Groups.
///
/// `Collapsed` and `Expanded` are the posture a Session view's Groups open at,
/// the way [`FoldPosture`] is for Folds: a default a view starts from, which
/// the reader then flips one Group at a time or all at once. `Off` forms no
/// Groups at all, so every Activity stands as its own row.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupPosture {
    #[default]
    Collapsed,
    Expanded,
    Off,
}

/// Whether a running Command or Tool Call grows from its one-line row into a
/// live output tail on its own. `Off` keeps disclosure entirely in the reader's
/// hands; `AfterMillis` promotes one that has remained Active for that long.
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSettings {
    pub default_fold_posture: FoldPosture,
    pub groups: GroupPosture,
    pub reasoning_visibility: ReasoningVisibility,
    pub tool_call_visibility: ToolCallVisibility,
    pub command_auto_expand: CommandAutoExpand,
    /// Whether an image Attachment is previewed as a picture where the
    /// terminal can draw one. On by default, because whether it can is probed
    /// rather than guessed; off, or where it cannot, the Attachment's dimmed
    /// line stands in for it.
    pub image_previews: bool,
}

impl Default for TranscriptSettings {
    fn default() -> Self {
        Self {
            default_fold_posture: FoldPosture::default(),
            groups: GroupPosture::default(),
            reasoning_visibility: ReasoningVisibility::default(),
            tool_call_visibility: ToolCallVisibility::default(),
            command_auto_expand: CommandAutoExpand::default(),
            image_previews: true,
        }
    }
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

/// Whether a TUI's Aside is on screen once a Session is open.
///
/// Like [`SidebarVisibility`], the Setting this spells governs only how the
/// client begins: a reader who shows or hides the Aside afterwards is changing
/// their view, not their configuration, so nothing writes the choice back.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AsideVisibility {
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

/// How a TUI's Aside begins. Both are launch Settings: the Aside adopts them
/// once, and the reader's own showing, hiding, and dragging rule after that.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AsideSettings {
    pub initial_visibility: AsideVisibility,
    pub initial_width: u64,
}

impl Default for AsideSettings {
    fn default() -> Self {
        Self {
            initial_visibility: AsideVisibility::default(),
            initial_width: 32,
        }
    }
}

/// How a Client presents the work a Sidekick does on the reader's behalf.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SidekickSettings {
    /// Whether Subsessions are left out wherever Sessions are listed — the
    /// Sidebar, its search, and the Session picker, on every Server listed —
    /// and reached through their Sidekick's Session instead, whose row then
    /// carries their Standing. Off by default, because a Subsession is as
    /// much the reader's work as any other until they say otherwise; one
    /// whose Sidekick's Session is gone is listed either way.
    pub hide_subsessions: bool,
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

/// Whether Provider Sessions are offered the Broker: Suru's own Tools, served
/// on the Server's loopback listener, through which an Agent reaches any
/// Provider Suru hosts. Off means no Provider start is handed the endpoint and
/// the endpoint answers nothing; a Provider already running keeps what it was
/// handed until its next launch.
///
/// The caps bound what the Broker spawns, each read at the moment of a spawn:
/// a spawn past either is refused, naming the cap, and never queued.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerSettings {
    pub enabled: bool,
    /// How many Sessions deep a tree may stand through the Broker, its
    /// top-level Session counting as the first: a spawn that would place its
    /// Subagent deeper is refused.
    pub max_depth: u32,
    /// How many brokered Subagents may work at once anywhere beneath one
    /// top-level Session: a spawn while that many work is refused. Native
    /// Subagents are not counted, because Suru cannot refuse them.
    pub max_concurrent_subagents: u32,
}

impl Default for BrokerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: 3,
            max_concurrent_subagents: 6,
        }
    }
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

/// The word that names every Server at once wherever an Origin is named —
/// a listing's scope of Everywhere — and so a name no Remote may take,
/// whatever its case.
pub const EVERYWHERE: &str = "everywhere";

/// Whether `name` is [`EVERYWHERE`], in any case.
pub fn names_everywhere(name: &str) -> bool {
    name.eq_ignore_ascii_case(EVERYWHERE)
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

/// A Session on a Remote, by the name the Server naming it knows that Remote
/// by and the Session's identity there.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSession {
    pub origin: String,
    pub session_id: SessionId,
}

/// A durable redeeming Server as the Serving Server knows it. The key itself
/// remains credential material inside the Server; local Clients receive only
/// the stable fingerprint used to identify and remove the Peer, and the name
/// the Peer gave itself when it redeemed its Invite — its machine's hostname —
/// by which what a Sidekick on it sends here is attributed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub id: String,
    pub fingerprint: String,
    pub name: String,
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
    pub aside: AsideSettings,
    pub sidekick: SidekickSettings,
    pub worktree: WorktreeSettings,
    pub provider: ProviderSettings,
    pub serving: ServingSettings,
    pub broker: BrokerSettings,
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
/// a Session override from a live reading of the Server Setting. A Subagent's
/// `pinned` is its spawner's, since a Subagent's posture is never set on
/// itself.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionApprovalPosture {
    pub value: ApprovalPosture,
    pub pinned: bool,
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
    TranscriptGroups {
        value: Option<GroupPosture>,
    },
    TranscriptReasoningVisibility {
        value: Option<ReasoningVisibility>,
    },
    TranscriptToolCallVisibility {
        value: Option<ToolCallVisibility>,
    },
    TranscriptCommandAutoExpand {
        value: Option<CommandAutoExpand>,
    },
    TranscriptImagePreviews {
        value: Option<bool>,
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
    AsideInitialVisibility {
        value: Option<AsideVisibility>,
    },
    AsideInitialWidth {
        value: Option<u64>,
    },
    SidekickHideSubsessions {
        value: Option<bool>,
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
    BrokerEnabled {
        value: Option<bool>,
    },
    BrokerMaxDepth {
        value: Option<u32>,
    },
    BrokerMaxConcurrentSubagents {
        value: Option<u32>,
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

/// Who a Message is from: the user, the Session's own Agent, or — for a
/// Delegation — the Agent that delegated to this Subagent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Agent,
    /// A Delegation standing in a Subagent's Transcript: the instruction its
    /// delegating Agent gave it through the Provider, from that Agent rather
    /// than from the user. One opens each Turn a spawn or a resume begins, as
    /// a user Message opens the Turn a Prompt begins. It arrives whole, so it
    /// never streams.
    Delegation(Delegator),
}

impl MessageRole {
    /// The Agent that sent a Delegation; `None` for every other Message.
    pub const fn delegator(&self) -> Option<&Delegator> {
        match self {
            Self::Delegation(delegator) => Some(delegator),
            Self::User | Self::Agent => None,
        }
    }
}

/// The Agent a Delegation is from, as the Delegation names it wherever it
/// stands: the Subagent's parent, or a sibling Subagent that sent it more.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Delegator {
    /// The delegating Agent's own Session: the Subagent's parent, or another
    /// Subagent's Session when a sibling delegated. A client tells the two
    /// apart against the Subagent's own `parent`, and reaches the delegating
    /// Agent through it.
    pub session_id: SessionId,
    /// The delegating Agent's name as the reader meets it on its Subagent
    /// rows — which kind of agent the Provider ran — when it is a Subagent;
    /// absent for a top-level Session's own Agent, which is named by nothing
    /// but its Session.
    #[serde(default)]
    pub name: Option<String>,
}

/// Who sent a Prompt, or gave a Questionnaire its Answer, on the user's
/// behalf, where the user did not themselves. It is carried on the Prompt and
/// on the user Message the Prompt becomes, and on the Questionnaire beside its
/// Answer, as typed data, so every client draws such words apart from the
/// user's own without reading it out of the text. A Session begun on the
/// user's behalf names it too, as the one that began it.
///
/// An act a Sidekick performs on a Remote travels there with its author, as
/// part of the Session API Servers speak to each other (ADR 0044); the Remote
/// believes nothing of it but that a Sidekick sent it, and names that
/// Sidekick by the Peer it came from.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Author {
    /// A Sidekick of the Server holding the Session: the Agent of the Session
    /// `session_id`, which a reader may follow back to, named by that
    /// Session's Title as it stood when the Sidekick acted. `session_id` names
    /// a Session of that same Server, as everything a Transcript names by its
    /// identity alone does; a Sidekick of another Server is a
    /// [`Self::PeerSidekick`].
    Sidekick {
        session_id: SessionId,
        title: String,
    },
    /// A Sidekick on the Peer `peer`, which acted here through the Pairing:
    /// named by the name the Serving Server knows that Peer by — the one it
    /// gave itself, told apart from every other Peer's — and by the Peer's
    /// key `fingerprint`, which tells it apart from a Peer once known by the
    /// same name, and by nothing the act claimed of the Sidekick's own
    /// Session, which lives on the Peer and is nothing a reader here may
    /// follow back to.
    PeerSidekick { peer: String, fingerprint: String },
}

/// The header an act's author travels between Servers in, as JSON of an
/// [`Author`]: set by a Server carrying its own Sidekick's act to a Remote,
/// and on the Remote by its Serving listener alone, which replaces whatever
/// the Peer claimed with the Peer it authenticated. A Client's request never
/// carries it.
pub const AUTHOR_HEADER: &str = "x-suru-author";

/// The header a newly admitted Prompt's answer says how the Session took it
/// in: `new_turn` where it begins a Turn of its own, `steer` where it steers
/// the Turn working, and `queued` where it waits behind that Turn — which a
/// Server carrying its Sidekick's Prompt to a Remote tells its Sidekick. A
/// retried admission that finds its Prompt already admitted says nothing.
pub const PROMPT_ADMISSION_HEADER: &str = "x-suru-prompt-admission";

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
    /// Settled because the work was asked to stop — an interrupted Session,
    /// a Subagent stopped on its own, or a Watch stopped before it finished —
    /// rather than because it finished or went wrong. Any Activity still
    /// running when its Turn is interrupted settles this way; one still
    /// running when its Turn settles any other way settles `Failed` (ADR 0039).
    Interrupted,
}

/// How a Watch settled, as recorded by its Watch Outcome. A Watch lost with
/// its Provider process wakes nothing and so records no outcome, which is why
/// there is no lost status here.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchOutcomeStatus {
    Completed,
    Failed,
    Stopped,
}

/// Whether a Compaction was the Provider's choice or the user's request. Suru
/// alone knows which Turns it began for a request, so this is read from the
/// Turn that holds the Compaction rather than from anything the Provider says
/// (ADR 0041).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// The Provider compacted on its own, inside whatever Turn it fell in.
    Automatic,
    /// The user asked for it, and it began a Turn of its own.
    Manual,
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

/// The name every harness knows the Broker by among its MCP servers. It is what
/// an Agent's Broker Tools go by — `mcp__suru__list_providers` to Claude — what
/// Suru recognizes the Broker's calls by where a harness asks Suru to permit
/// them, and the `server` of every Tool Call of a Broker Tool, whichever
/// Provider recorded it. It is the protocol's because a client reads it off a
/// Tool Call, to tell a wait on Subagents from work.
pub const BROKER_SERVER_NAME: &str = "suru";

/// The Broker Tool an Agent waits on its Subagents through. A Tool Call of it
/// stays open for as long as the wait does.
pub const WAIT_SUBAGENTS_TOOL: &str = "wait_subagents";

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
        /// Who answered or declined the Questionnaire on the user's behalf;
        /// absent for the user's own, and while it is neither.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<Author>,
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
    /// One use of a Tool that no more specific Activity records. A failure
    /// is a Failed row whose `output` carries the Provider's error text.
    ToolCall {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        /// The Tool's name as its Provider spells it.
        name: String,
        /// The MCP server hosting the Tool, where it has one.
        server: Option<String>,
        /// The Tool's arguments as one line of display text, rendered when
        /// they were recorded; empty until the Provider has sent them.
        input: String,
        /// Whether Suru's cap cut the stored input short of the rendering.
        input_truncated: bool,
        /// The text the Tool's result carried.
        output: String,
        /// Whether Suru's cap cut the stored output short of what the Provider
        /// sent, so a client can say so without reading it out of `output`.
        output_truncated: bool,
        /// How many parts of the result that were not text — images, audio,
        /// resources — were left out of `output`.
        omitted_parts: u32,
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
        /// Whether Suru spawned the Subagent through the Broker, on a Provider
        /// actor of its own, rather than the delegating Agent's own Provider
        /// spawning it (ADR 0035). A brokered Subagent may always be stopped
        /// on its own, whatever the delegating Agent's Provider allows for
        /// Subagents of its own, so a client offers its stop from the row
        /// regardless.
        #[serde(default)]
        brokered: bool,
        /// How long the Subagent worked, timed by Suru from its spawn. Known
        /// only once it settles — by the Provider's own settle event or by a
        /// stop the user asked for — and absent on a row that a lost Provider
        /// connection closed, there being no moment the work truly ended.
        duration_ms: Option<u64>,
    },
    /// How a Watch settled, recorded because its settling woke the Agent: it
    /// heads the Turn the Agent woke into, or stands in the Turn that was
    /// active when it arrived, so the reader can see why the Agent worked on.
    /// It is settled from the moment it is recorded, and it is no Command:
    /// the Command that started the Watch already stands where it ran.
    WatchOutcome {
        id: ActivityId,
        turn_id: TurnId,
        status: WatchOutcomeStatus,
        /// What the Watch was doing, in the words its Provider gave it.
        description: String,
        /// The Provider's own account of how the Watch settled, where it gave
        /// one: display text, never parsed.
        summary: Option<String>,
    },
    /// One occasion on which the Provider replaced what its Agent remembers of
    /// this Session with a summary, to free room in its context. It is Active
    /// while the Provider summarises and Settles like any Activity; each
    /// attempt is its own Compaction, so one that failed and was tried again
    /// stands as two. It belongs to the Session whose context was compacted,
    /// so a Subagent's stands in the Subagent's own Transcript.
    Compaction {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        trigger: CompactionTrigger,
        /// What the user asked the summary to keep, in their own words, where
        /// they asked anything: everything typed after `/compact`, as it was
        /// handed the Provider. Only a manual Compaction carries any, and it
        /// carries them from the moment it opens, however it ends.
        instructions: Option<String>,
        /// The Context Fill before and after, in tokens, where they are known
        /// — never guessed, so either may be absent. Each is the Provider's
        /// own count where it reported one, and otherwise the Session's own
        /// Context Fill as last read before the Compaction began and as first
        /// read once it Settled. `after_tokens` may exceed `before_tokens` on
        /// a short history; it is recorded as it is.
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
        /// Why a failed Compaction failed, in the Provider's words, where it
        /// gave any. Display text, never parsed.
        error: Option<String>,
        /// The summary a completed Compaction left the Agent with, where its
        /// Provider gave one, less any wrapping the Provider put around it for
        /// the Agent's benefit. Only a completed Compaction carries one.
        summary: Option<String>,
        /// Whether Suru's cap cut the stored summary short of what the
        /// Provider sent, so a client can say so without reading it out of
        /// `summary`.
        summary_truncated: bool,
    },
    /// One Session the Turn's Agent — a Sidekick — began: a Subsession. This
    /// row is the way into it from where it was begun, and all the Sidekick's
    /// Transcript carries of it: the Subsession is a top-level Session of its
    /// own, whose work lives there and never keeps this Turn working. It
    /// reports the moment of beginning rather than work in progress, so it
    /// has no status to settle.
    Subsession {
        id: ActivityId,
        turn_id: TurnId,
        /// The Subsession's own Session, on the Server holding this
        /// Transcript, or on the Remote `origin` names.
        session_id: SessionId,
        /// The Remote the Subsession was begun on, by the name the Server
        /// holding this Transcript knows it by; absent for one of that
        /// Server's own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        /// The Subsession's Title, which the Server keeps in step with the
        /// Title it derives for it, so the row names it as every listing does
        /// — for one on a Remote, as that Remote last said it while it was
        /// kept in view.
        title: String,
        /// What the Sidekick first asked of it: its first Prompt's text.
        prompt: String,
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
            | Self::ToolCall { id, .. }
            | Self::Reasoning { id, .. }
            | Self::Subagent { id, .. }
            | Self::WatchOutcome { id, .. }
            | Self::Compaction { id, .. }
            | Self::Subsession { id, .. } => *id,
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
            | Self::ToolCall { turn_id, .. }
            | Self::Reasoning { turn_id, .. }
            | Self::Subagent { turn_id, .. }
            | Self::WatchOutcome { turn_id, .. }
            | Self::Compaction { turn_id, .. }
            | Self::Subsession { turn_id, .. } => *turn_id,
        }
    }

    /// Whether this is the Tool Call an Agent waits on its Subagents in — a
    /// call of the Broker's [`WAIT_SUBAGENTS_TOOL`], whichever Provider
    /// recorded it — which is the wait itself rather than work beside it.
    pub fn is_wait_on_subagents(&self) -> bool {
        matches!(
            self,
            Self::ToolCall {
                server: Some(server),
                name,
                ..
            } if server == BROKER_SERVER_NAME && name == WAIT_SUBAGENTS_TOOL
        )
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
            Self::Status { .. } | Self::Error { .. } | Self::Subsession { .. } => None,
            Self::Command { status, .. }
            | Self::FileChange { status, .. }
            | Self::ToolCall { status, .. }
            | Self::Reasoning { status, .. }
            | Self::Subagent { status, .. }
            | Self::Compaction { status, .. } => Some(*status),
            Self::WatchOutcome { status, .. } => Some(match status {
                WatchOutcomeStatus::Completed => ActivityStatus::Completed,
                WatchOutcomeStatus::Failed => ActivityStatus::Failed,
                WatchOutcomeStatus::Stopped => ActivityStatus::Interrupted,
            }),
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

/// What occupies a Session's context, as its Provider attributes it when
/// asked: the Context Fill it measures, split by where the tokens came from.
/// It is read on request and never stored, so it describes the context only
/// at the moment it was asked for.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBreakdown {
    pub fill: ContextFill,
    /// The part of the window the Provider holds back from the Agent, such
    /// as a compaction buffer or an output reserve, where it says.
    pub reserved_tokens: Option<u64>,
    /// The occupied context by source, in the Provider's own order.
    pub parts: Vec<ContextPart>,
}

/// One source's share of a [`ContextBreakdown`], with the items it is made
/// of where the Provider names them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPart {
    pub source: ContextSource,
    pub tokens: u64,
    pub items: Vec<ContextItem>,
}

/// Where tokens occupying a context came from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContextSource {
    SystemPrompt,
    /// Definitions of the Tools the Provider builds in.
    SystemTools,
    /// Definitions of the Tools MCP servers offer.
    McpTools,
    /// Instruction files the Provider loads, such as `CLAUDE.md` or
    /// `AGENTS.md`.
    Instructions,
    Skills,
    /// Definitions of the Agents the Provider can delegate to.
    Agents,
    /// The conversation itself.
    Messages,
    /// A source Suru has no name of its own for, under the Provider's label.
    Other {
        label: String,
    },
}

/// A named contributor to a [`ContextPart`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextItem {
    pub label: String,
    pub tokens: u64,
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
    /// When this Session began Monitoring: nothing in its subtree is Working,
    /// but a Watch is still live and may wake its Agent into a Continuation.
    /// It counts from the later of when Working last ended and when the
    /// earliest live Watch started, so it never overlaps Working. `None`
    /// whenever the Session is Working or has no live Watch — and after every
    /// restart, since no Watch outlives its Provider process and the reading
    /// is never stored.
    #[serde(default)]
    pub monitoring_since: Option<SessionTimestamp>,
    /// The Session whose Turn spawned this one, present exactly when this is a
    /// Subagent's Session. A child is reachable only through its parent: it
    /// joins no Session listing, refuses Prompts, and is deleted along with
    /// the parent it names.
    #[serde(default)]
    pub parent: Option<SessionId>,
    /// Who began this Session on the user's behalf, present exactly when the
    /// user did not begin it themselves: a Subsession names the Sidekick
    /// whose Session began it, and a Session a Sidekick on a Peer began here
    /// names that Peer, heading its own tree since its Sidekick is
    /// elsewhere. Unlike `parent`, it changes nothing about the
    /// Session's work — it is listed, prompted, interrupted and deleted as
    /// any top-level Session is, and nothing it consumes is rolled up beneath
    /// whoever began it — so it is what the Session remembers rather than
    /// where it stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub begun_by: Option<Author>,
}

impl Author {
    /// The Sidekick's Session this author names, where it names one of this
    /// Server's: a Sidekick on a Peer names none.
    pub fn sidekick_session(&self) -> Option<SessionId> {
        match self {
            Self::Sidekick { session_id, .. } => Some(*session_id),
            Self::PeerSidekick { .. } => None,
        }
    }
}

impl Session {
    /// Whether this is a Subagent's Session — a child of the Session whose
    /// Turn spawned it. Every property of being one keys off this single
    /// reading: excluded from listings and the catalog, refusing Prompts, and
    /// deleted with its parent.
    pub const fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }

    /// The Sidekick's Session that began this one, when this is a Subsession.
    pub fn sidekick(&self) -> Option<SessionId> {
        self.begun_by.as_ref().and_then(Author::sidekick_session)
    }
}

#[cfg(test)]
impl Session {
    /// An idle top-level Session working at the root of `workspace`, with no
    /// Agent yet, for a test that reads only where a Session is.
    pub(crate) fn for_tests(workspace: Workspace) -> Self {
        Self {
            context_fill: None,
            id: SessionId::new(),
            execution_directory: ExecutionDirectory {
                path: workspace.path.clone(),
            },
            workspace,
            checkout: None,
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        }
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
    /// This Session's own Cost, read exactly as [`SessionSnapshot::own_cost`]
    /// is, so a listing and an open Session never disagree about it.
    #[serde(default)]
    pub own_cost: Option<CostTotal>,
    /// The Sessions a Sidekick's Session began on Remotes: each a Subsession
    /// there, heading its own tree and naming only the Peer it came from,
    /// which only this Server knows began here — so a reader hiding
    /// Subsessions finds by this the Sidekick's row to carry each in.
    /// Empty for every other Session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_subsessions: Vec<RemoteSession>,
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

    /// When this Session began Monitoring, and `None` where it is Working or
    /// has no live Watch. A Session Suru could not read is never Monitoring,
    /// for the same reason it is never Working.
    pub const fn monitoring_since(&self) -> Option<SessionTimestamp> {
        match self {
            Self::Readable(summary) => summary.session.monitoring_since,
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
    /// A Session began or stopped Monitoring, so what
    /// [`Session::monitoring_since`] reads changed. It rides the catalog
    /// stream beside Working, and for the same reason: a Watch starting or
    /// settling moves no Turn, so nothing else a listing hears says so.
    MonitoringChanged {
        session_id: SessionId,
        monitoring_since: Option<SessionTimestamp>,
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
    /// or its own Cost moved. It rides the catalog stream for the same reason Working does:
    /// every client lists the Session, and only some have it open. It carries
    /// the whole reading, and moves nothing else about the row, so a listing
    /// in hand is current the moment it lands.
    UsageChanged {
        session_id: SessionId,
        total_usage: Option<UsageTotal>,
        own_cost: Option<CostTotal>,
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
    /// A Workspace's Description was derived, set, or cleared. It rides the
    /// catalog stream for the reason its Icon does: every client may list a
    /// Session rooted in that Workspace, and the Workspace Picker draws the
    /// Description of whichever Workspace the reader is on.
    WorkspaceDescriptionChanged {
        workspace_id: WorkspaceId,
        description: Option<WorkspaceDescription>,
    },
    /// The Sessions a Sidekick's Session began on Remotes changed — it began
    /// another there, or one was found no longer held — carried whole. It
    /// rides the catalog stream because a client hiding Subsessions reads it
    /// wherever it lists the Sidekick's Session.
    RemoteSubsessionsChanged {
        session_id: SessionId,
        remote_subsessions: Vec<RemoteSession>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCatalogUpdate {
    pub revision: SessionCatalogRevision,
    pub change: SessionCatalogChange,
}

/// The whole tree of Sessions one top-level Session heads, as the per-tree
/// subscription opens with it: that Session, then every Subagent's Session
/// beneath the one that spawned it. Asked for through any Session in the
/// tree, at any depth, it answers for the same tree, and names its top-level
/// Session so a client can tell whether two Sessions share one.
///
/// A Sidekick's Session heads a tree answering for everything that Sidekick
/// has a hand in: beneath its own Subagents stand its Subsessions and every
/// other Session it has acted on, each with its own Subagents beneath it. A
/// Subsession's tree is its Sidekick's, so it is answered through a
/// Subsession too; a Session the Sidekick only acted on heads its own tree,
/// since several Sidekicks may have acted on it. A Session it has a hand in
/// on a Remote stands with the Subagents beneath it there, as that Remote
/// says of them, each named by that Remote as its `origin`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTreeSnapshot {
    pub revision: SubagentTreeRevision,
    pub top_level: SubagentTreeTopLevel,
    /// Every Subagent's Session in the tree, depth-first: each follows the
    /// Session that spawned it, its own descendants follow it, and siblings
    /// stand in the order they spawned. The top-level Session's own come
    /// first, then those of each of [`Self::sessions`] in turn.
    pub subagents: Vec<SubagentTreeEntry>,
    /// Where a Sidekick's Session heads the tree, its Subsessions and every
    /// other Session it has acted on, the one it acted on most recently
    /// first; empty for every other tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SubagentTreeSession>,
}

/// The top-level Session heading a tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTreeTopLevel {
    pub session_id: SessionId,
    pub title: String,
    /// When the top-level Session's uninterrupted Working interval began —
    /// the reading its Sidebar row's Working duration rises from, work
    /// continued by surviving Subagents included — and `None` while nothing
    /// in the tree is Working.
    #[serde(default)]
    pub working_since: Option<SessionTimestamp>,
    /// When the top-level Session began Monitoring — nothing in the tree
    /// Working, and a Watch live somewhere in it — the reading its Sidebar
    /// row's Monitoring duration rises from; `None` otherwise.
    #[serde(default)]
    pub monitoring_since: Option<SessionTimestamp>,
    /// The Marker of the top-level Session's own work, as an entry standing
    /// for it beneath a Sidekick would give it: `Active` while it works, and
    /// otherwise the outcome its latest Turn settled with. `None` for a
    /// Session that has neither worked nor is working.
    #[serde(default)]
    pub status: Option<ActivityStatus>,
    /// How long the top-level Session's own settled Turns worked, summed, as
    /// an entry standing for it beneath a Sidekick counts it.
    #[serde(default)]
    pub worked_ms: Option<u64>,
    /// When the work the top-level Session itself is doing now began, while
    /// it does any, and `None` otherwise: what such an entry's time counts up
    /// from [`Self::worked_ms`] from. It may begin later than
    /// [`Self::working_since`], whose interval its Subagents' carrying on
    /// opens too.
    #[serde(default)]
    pub own_working_since: Option<SessionTimestamp>,
    /// Whether the top-level Session's own Transcript holds a live Approval
    /// or Questionnaire. Its Subagents' Interventions are theirs to say.
    #[serde(default)]
    pub needs_intervention: bool,
    /// Whether the top-level Session is a Sidekick's, whose tree answers for
    /// every Session that Sidekick has a hand in — whether or not it has
    /// acted on any yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sidekick: bool,
}

/// One Session a Sidekick has a hand in, standing beneath the Sidekick's
/// Session in its tree: a Subsession it began, or another Session it acted
/// on — sent a Prompt, answered, interrupted, set aside, or brought back.
/// Reading a Session is no act. It is a top-level Session of its own, so
/// nothing in it rolls up into the Sidekick's entry, and it stands for as
/// long as it and the Sidekick's Session both exist, settled or not.
///
/// A Session on a Remote — one the Sidekick began or acted on there — names
/// that Remote as its `origin`, and stands as the Remote last said of it
/// while the Remote answers; one whose Remote does not answer now stands
/// `unanswered`, keeping what last said which Session it is and nothing that
/// would give its work as current.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTreeSession {
    pub session_id: SessionId,
    /// The Remote the Session lives on, by the name the Server heading the
    /// tree knows it by; absent for a Session of that Server's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Whether the Session's Remote does not answer now: its Title and
    /// Workspace are what the Remote last said, so a reader still knows which
    /// Session it is — empty where it said nothing since it was kept in view
    /// — and nothing of its work is given as current: no Model, Marker, time
    /// or Intervention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unanswered: bool,
    /// Whether the Sidekick's act on the Session is not known to have been
    /// done: carried to its Remote, whose answer never came back whole. It
    /// stands as one whose Remote does not answer does — by what names it,
    /// with nothing of its work given as current — until a read of that
    /// Remote finds the Session, or finds it holds none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unconfirmed: bool,
    pub title: String,
    /// Whether the Sidekick heading the tree began this Session: a
    /// Subsession, whose tree is its Sidekick's, so opening it leaves the
    /// tree as it stands. A Session the Sidekick only acted on heads a tree
    /// of its own. Nothing a reader is shown tells the two apart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subsession: bool,
    /// The presented root of the Workspace the Session works in, which a
    /// client names it by as it names any Workspace.
    pub workspace_path: PathBuf,
    /// The Icon Catalog name of that Workspace's Icon, where it has one.
    #[serde(default)]
    pub workspace_icon: Option<String>,
    /// The Model the Session's Agent Selection names, where it has one.
    #[serde(default)]
    pub model: Option<ModelId>,
    /// The Marker of the Session's work: `Active` while it is Working on a
    /// Turn of its own or on a Prompt waiting to begin one, and otherwise
    /// the outcome its latest Turn settled with — passing over a
    /// Continuation begun only to hold a Subagent's row. `None` for a
    /// Session that has neither worked nor is working.
    #[serde(default)]
    pub status: Option<ActivityStatus>,
    /// How long its settled Turns worked, summed, as a Subagent's entry
    /// counts it: what its time counts up from while it works, and the whole
    /// of it once it settles. `None` where Suru never learned when its work
    /// ended.
    pub worked_ms: Option<u64>,
    /// When the work it is doing now began, while it works, and `None` once
    /// it settles.
    pub working_since: Option<SessionTimestamp>,
    /// When the Session began Monitoring, and `None` otherwise.
    #[serde(default)]
    pub monitoring_since: Option<SessionTimestamp>,
    /// Whether the Session's own Transcript holds a live Approval or
    /// Questionnaire. Its Subagents' Interventions are theirs to say.
    #[serde(default)]
    pub needs_intervention: bool,
    /// The moment of the Sidekick's latest act on it — beginning it among
    /// them — by which the tree orders those not working.
    pub acted_at: SessionTimestamp,
}

/// One Subagent's Session in a tree. It stands where the Subagent's spawn
/// left its row in the spawner's Transcript and is named by that row, but its
/// Marker and time are read from the Subagent's own Session's Turns, never
/// from any one row: a resumed Subagent is still one entry, and a resume or a
/// Continuation of its Session sets it Working again (ADR 0031).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTreeEntry {
    pub session_id: SessionId,
    /// The Remote the Subagent's Session lives on, by the name the Server
    /// heading the tree knows it by — beneath a Session of that Remote in a
    /// Sidekick's tree — and absent for a Session of that Server's own. Its
    /// spawner lives on the same Server as it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The Session whose Turn spawned this Subagent: the top-level Session or
    /// another Subagent's.
    pub parent_session_id: SessionId,
    /// Where this Subagent stands among its spawner's Subagents, counting from
    /// zero in the order they spawned. It never changes once assigned.
    pub spawn_order: u32,
    /// Which kind of agent the Provider ran.
    pub name: String,
    /// What the Subagent was asked to do, as its spawn — or the Provider's
    /// latest update to it — described it. Where that describes nothing, the
    /// Subagent's Session's Title, or the Subagent's name where the Server
    /// holds no such Session.
    pub title: String,
    /// The latest Model the Provider confirmed for this Subagent, read from
    /// its rows in its spawner's Transcript. It is Provider evidence, never
    /// inherited from the parent's Agent Selection, and absent until the
    /// Provider supplies it (ADR 0025).
    #[serde(default)]
    pub model: Option<ModelId>,
    /// The Marker of the Subagent's Session's latest Turn: `Active` while
    /// that Turn works, and otherwise the outcome it settled with.
    pub status: ActivityStatus,
    /// How long the Subagent's settled Turns worked, summed. While it works
    /// this is the total its time counts up from; once it settles, the whole
    /// of it. `None` where it settled without Suru learning when its work
    /// ended.
    pub worked_ms: Option<u64>,
    /// When the Turn the Subagent works in began, while it works, and `None`
    /// once it settles. A client ticks a working entry's time up from
    /// [`Self::worked_ms`] from here, as it ticks a Sidebar row's Working
    /// duration from its `working_since`.
    pub working_since: Option<SessionTimestamp>,
    /// When the Subagent's Session began Monitoring — a Watch live in its
    /// subtree and nothing there Working — and `None` otherwise. A settled
    /// Subagent whose Watches outlive it reads Monitoring here, which its
    /// entry says in place of its time (ADR 0030).
    #[serde(default)]
    pub monitoring_since: Option<SessionTimestamp>,
    /// Whether this Subagent's own Session holds a live Approval or
    /// Questionnaire. It is never rolled up: a Subagent whose descendant waits
    /// on an Intervention does not say so itself.
    #[serde(default)]
    pub needs_intervention: bool,
}

/// One change to a tree after its snapshot. An entry never moves: a spawn
/// joins its spawner's Subagents last, and nothing else reorders them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentTreeChange {
    /// A Subagent spawned. Its spawner is already in the tree, and the entry
    /// comes last among that spawner's Subagents. A Subagent beneath a
    /// Remote's Session that moved is carried whole by this change too,
    /// replacing the entry held for it by its Origin and identity: the
    /// changes below that name a Subagent by its identity alone name one of
    /// the tree's own Server.
    SubagentSpawned { entry: SubagentTreeEntry },
    /// A Subagent's latest Turn began or Settled — a resume or a Continuation
    /// setting it Working again, or its work settling as completed, failed,
    /// or interrupted — or its Session began or stopped Monitoring, so its
    /// Marker and time moved, carried whole. A Turn settling with a Watch
    /// still live moves both at once, in one change.
    SubagentWorkingChanged {
        session_id: SessionId,
        status: ActivityStatus,
        worked_ms: Option<u64>,
        working_since: Option<SessionTimestamp>,
        #[serde(default)]
        monitoring_since: Option<SessionTimestamp>,
    },
    /// A Subagent's name or Title changed, carried whole.
    SubagentRetitled {
        session_id: SessionId,
        name: String,
        title: String,
    },
    /// The Provider confirmed a Subagent's Model, or confirmed another one.
    /// It never withdraws one: a Model, once known, stays until replaced.
    SubagentModelChanged {
        session_id: SessionId,
        model: ModelId,
    },
    /// The top-level Session's Title changed.
    TopLevelRetitled { title: String },
    /// The top-level Session began or stopped Working or Monitoring, or its
    /// own work moved: its new `working_since`, `monitoring_since`, Marker and
    /// time, carried whole, so Working giving way to Monitoring is one change.
    TopLevelWorkingChanged {
        working_since: Option<SessionTimestamp>,
        #[serde(default)]
        monitoring_since: Option<SessionTimestamp>,
        #[serde(default)]
        status: Option<ActivityStatus>,
        #[serde(default)]
        worked_ms: Option<u64>,
        #[serde(default)]
        own_working_since: Option<SessionTimestamp>,
    },
    /// Whether one Session of the tree — the top-level Session or any
    /// Subagent's — holds a live Intervention of its own changed.
    NeedsInterventionChanged {
        session_id: SessionId,
        needs_intervention: bool,
    },
    /// A Session the Sidekick heading the tree has a hand in joined it — the
    /// Sidekick began it or first acted on it — or one already in it moved:
    /// acted on again, retitled, regrouped, or its work, its Agent
    /// Selection's Model or its Interventions changed. Carried whole.
    SessionChanged { entry: SubagentTreeSession },
    /// A Session the tree stood beneath its Sidekick left it, and every
    /// Subagent's Session beneath it with it — on the Remote `origin` names,
    /// where it lived on one. One deleted leaves for good; one of a Remote
    /// may join again at once, carried whole with what stands beneath it,
    /// where what that Remote says of it moved in a way no other change can
    /// say.
    SessionLeft {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    /// The top-level Session was deleted, and every Session in its tree with
    /// it. It is the stream's last word: nothing follows it, and asking for
    /// the tree again finds no Session to answer for.
    TreeDeleted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentTreeUpdate {
    pub revision: SubagentTreeRevision,
    pub change: SubagentTreeChange,
}

impl SubagentTreeSnapshot {
    /// Takes up `change`, as every reader of the tree applies it after the
    /// snapshot: an entry is known by its Origin and identity together, so
    /// a Remote's Session or Subagent is never mistaken for one of the tree's
    /// own Server sharing its identity. The revision is the reader's to
    /// keep.
    pub fn apply(&mut self, change: SubagentTreeChange) {
        match change {
            SubagentTreeChange::SubagentSpawned { entry } => {
                match self.subagent_mut(entry.origin.as_deref(), entry.session_id) {
                    Some(held) => *held = entry,
                    None => self.subagents.push(entry),
                }
            }
            SubagentTreeChange::SubagentWorkingChanged {
                session_id,
                status,
                worked_ms,
                working_since,
                monitoring_since,
            } => {
                if let Some(entry) = self.subagent_mut(None, session_id) {
                    entry.status = status;
                    entry.worked_ms = worked_ms;
                    entry.working_since = working_since;
                    entry.monitoring_since = monitoring_since;
                }
            }
            SubagentTreeChange::SubagentRetitled {
                session_id,
                name,
                title,
            } => {
                if let Some(entry) = self.subagent_mut(None, session_id) {
                    entry.name = name;
                    entry.title = title;
                }
            }
            SubagentTreeChange::SubagentModelChanged { session_id, model } => {
                if let Some(entry) = self.subagent_mut(None, session_id) {
                    entry.model = Some(model);
                }
            }
            SubagentTreeChange::TopLevelRetitled { title } => self.top_level.title = title,
            SubagentTreeChange::TopLevelWorkingChanged {
                working_since,
                monitoring_since,
                status,
                worked_ms,
                own_working_since,
            } => {
                self.top_level.working_since = working_since;
                self.top_level.monitoring_since = monitoring_since;
                self.top_level.status = status;
                self.top_level.worked_ms = worked_ms;
                self.top_level.own_working_since = own_working_since;
            }
            SubagentTreeChange::NeedsInterventionChanged {
                session_id,
                needs_intervention,
            } => {
                if session_id == self.top_level.session_id {
                    self.top_level.needs_intervention = needs_intervention;
                } else if let Some(entry) = self.subagent_mut(None, session_id) {
                    entry.needs_intervention = needs_intervention;
                }
            }
            SubagentTreeChange::SessionChanged { entry } => {
                match self
                    .sessions
                    .iter_mut()
                    .find(|held| held.session_id == entry.session_id && held.origin == entry.origin)
                {
                    Some(held) => *held = entry,
                    None => self.sessions.push(entry),
                }
            }
            // The Session leaves with every Subagent beneath it, which lives
            // on the same Server as it.
            SubagentTreeChange::SessionLeft { session_id, origin } => {
                self.sessions
                    .retain(|held| held.session_id != session_id || held.origin != origin);
                let parents = self
                    .subagents
                    .iter()
                    .filter(|entry| entry.origin == origin)
                    .map(|entry| (entry.session_id, entry.parent_session_id))
                    .collect::<HashMap<_, _>>();
                self.subagents.retain(|entry| {
                    if entry.origin != origin {
                        return true;
                    }
                    let mut at = entry.session_id;
                    // A Subagent cannot be its own ancestor, so a walk longer
                    // than the tree is one round a cycle, and ends there.
                    for _ in 0..=parents.len() {
                        match parents.get(&at) {
                            Some(parent) if *parent == session_id => return false,
                            Some(parent) => at = *parent,
                            None => break,
                        }
                    }
                    true
                });
            }
            // Nothing follows the tree's last word.
            SubagentTreeChange::TreeDeleted => {}
        }
    }

    /// The entry of the Subagent `session_id` on the Remote `origin` names,
    /// or on the tree's own Server where it names none.
    fn subagent_mut(
        &mut self,
        origin: Option<&str>,
        session_id: SessionId,
    ) -> Option<&mut SubagentTreeEntry> {
        self.subagents
            .iter_mut()
            .find(|entry| entry.session_id == session_id && entry.origin.as_deref() == origin)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub id: PromptId,
    pub text: String,
    pub skill_invocations: Vec<SkillInvocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentBinding>,
    pub delivery: PromptDelivery,
    pub admission_order: PromptOrder,
    pub status: PromptStatus,
    /// Why the Session withdrew a Cancelled Prompt of its own accord, where
    /// that decides whose composer its text returns to. Absent for every
    /// other Prompt, including one withdrawn because someone asked: an
    /// interrupt says so to the client that sent it (ADR 0024).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawal: Option<PromptWithdrawal>,
    /// Who sent the Prompt on the user's behalf; absent for the user's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
}

/// Why the Session withdrew a Prompt no one asked it to give back, recorded
/// on the Prompt so the client that wrote it can tell, whatever it saw of
/// the Session meanwhile, that the text is owed back to its composer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "reason")]
pub enum PromptWithdrawal {
    /// The Prompt was held behind `turn_id`, the Turn a Compaction request
    /// began, which settled without its Compaction completing, so the
    /// Prompt never reached a context its writer expected compacted
    /// (ADR 0041).
    CompactionUnfinished { turn_id: TurnId },
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

/// A request that a Session's Provider compact its context now, which begins
/// a Turn of its own holding that one Compaction (ADR 0041). Only an idle,
/// top-level Session on a Provider that compacts on request accepts it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactSessionRequest {
    /// What the summary should keep, in the user's words: everything typed
    /// after `/compact`. A Provider that takes no instructions refuses a
    /// request carrying them rather than dropping them.
    #[serde(default)]
    pub instructions: Option<String>,
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
    /// began — a Continuation, a Subagent Session's Turn opened by its spawn,
    /// or one a Compaction request began.
    pub prompt_id: Option<PromptId>,
    /// Whether the user's request for a Compaction began this Turn (ADR
    /// 0041): one with no user Message, whose only content is that
    /// Compaction, which Settles as the Compaction does and accepts no steer.
    /// Only Suru knows which Turns it began this way, so whether a Compaction
    /// was manual is read from here.
    pub compaction_requested: bool,
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
    #[serde(default)]
    compaction_requested: bool,
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
        let turn = Self {
            id: turn.id,
            prompt_id: turn.prompt_id,
            compaction_requested: turn.compaction_requested,
            agent: turn.agent,
            status: turn.status,
            started_at: turn.started_at,
            settled_at: turn.settled_at,
            last_output_at: turn.last_output_at,
            usage: turn.usage,
            cost: turn.cost,
            cost_basis: turn.cost_basis,
            cost_details: turn.cost_details,
        };
        if !turn.has_valid_opening() {
            return Err("a Turn is begun by a Prompt or a Compaction request, never both");
        }
        Ok(turn)
    }
}

impl Turn {
    pub const fn has_valid_cost_attribution(&self) -> bool {
        self.cost.is_some() == self.cost_basis.is_some()
    }

    /// Whether this Turn names at most one thing that began it: a Prompt or
    /// a Compaction request, never both.
    pub const fn has_valid_opening(&self) -> bool {
        !(self.compaction_requested && self.prompt_id.is_some())
    }

    /// Whether this Turn is a Continuation: the one kind of Turn that begins
    /// with nothing asked — no Prompt, and no Compaction request. Named once
    /// here so every place that treats Continuations apart — admission, the
    /// steer sweep — asks the same question. A Subagent Session's Turn also
    /// carries no Prompt, but those Sessions take no Prompts at all, so the
    /// question never arises there.
    pub const fn is_continuation(&self) -> bool {
        self.prompt_id.is_none() && !self.compaction_requested
    }

    /// Whether a steer Prompt may join this Turn, which only a Turn a Prompt
    /// began accepts. A Continuation is settled by the next delivered Prompt,
    /// and a Turn a Compaction request began accepts no steer (ADR 0041), so
    /// a Prompt admitted while either runs begins a Turn of its own.
    pub const fn accepts_steer(&self) -> bool {
        self.prompt_id.is_some()
    }

    /// A Turn begun by no Prompt — a Continuation, or in a Subagent's Session
    /// the Turn a Delegation opens — run by `agent` where it is known: working,
    /// with nothing measured yet, and untimed until the commit that lands it
    /// stamps when it began.
    pub fn unprompted(agent: Option<AgentIdentity>) -> Self {
        Self {
            id: TurnId::new(),
            prompt_id: None,
            compaction_requested: false,
            agent,
            status: TurnStatus::Active,
            started_at: None,
            settled_at: None,
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }
    }

    /// The Turn a Compaction request begins (ADR 0041), run by `agent` where
    /// it is known: working, on nothing but the Compaction its Provider is
    /// about to report.
    pub fn requested_compaction(agent: Option<AgentIdentity>) -> Self {
        Self {
            compaction_requested: true,
            ..Self::unprompted(agent)
        }
    }

    /// How long this Turn worked, from the commit that began it to the one
    /// that settled it; `None` while it works, or where either moment went
    /// unrecorded.
    pub fn worked_ms(&self) -> Option<u64> {
        self.settled_at?.0.checked_sub(self.started_at?.0)
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
    /// and Delegations always carry an empty list.
    pub skill_invocations: Vec<SkillInvocation>,
    /// The Attachments bound to labels in a user Message's content, carried
    /// from the Prompt it was delivered from. Agent Messages and Delegations
    /// always carry an empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentBinding>,
    /// Whether Suru's cap cut the stored content short of what the Provider
    /// sent, so a client can say so without reading it out of `content`.
    pub truncated: bool,
    /// Who sent the Prompt a user Message was delivered from, on the user's
    /// behalf, carried from that Prompt; absent for the user's own, and for
    /// every Agent Message and Delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
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

/// One Watch live somewhere in a Session's subtree, as a reader viewing that
/// Session is told what it is Monitoring: in the words its Provider gave it,
/// and since when the Server heard it start. It is never stored, since no
/// Watch outlives the Provider process that runs it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSummary {
    pub description: String,
    pub started_at: SessionTimestamp,
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
    /// This Session's own Cost: the same Cost Coverage applied to its own
    /// Turns alone, so the Provider's account of its own conversation. A
    /// Claude Session's includes the native Subagents Claude ran inside its
    /// process, since Claude reports no split; a brokered Subagent's is never
    /// in it. Derived beside [`Self::total_cost`] and moved with it.
    #[serde(default)]
    pub own_cost: Option<CostTotal>,
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
    /// The Watches live anywhere in this Session's subtree, earliest first —
    /// what the Session is waiting on while it is Monitoring. Derived by the
    /// server beside [`Session::monitoring_since`], and like it never stored,
    /// so a Session read after a restart has none. Only an open Session needs
    /// them, so they ride the Session's own stream and no listing.
    #[serde(default)]
    pub watches: Vec<WatchSummary>,
    /// What the server stores for every Attachment this Session's Prompts
    /// and Messages bind, once each and ordered by id, so a client describes
    /// an Attachment beneath the Message binding it from the snapshot alone.
    /// Never the bytes: a client that wants those fetches them by id.
    #[serde(default)]
    pub attachments: Vec<AttachmentDescriptor>,
    /// The Turn in which this Session's Agent is waiting on its Subagents
    /// through the Broker's `wait_subagents`, while any such call is open —
    /// the latest Turn to have opened one where calls from several Turns are
    /// still open, since a call from a Turn that has settled lingers only
    /// until the Server learns its client has gone. Set and cleared by the
    /// Server, which sees every Provider's calls to the Broker, and never
    /// stored, since no call outlives the process answering it. It moves
    /// neither Working nor Monitoring: read through
    /// [`Self::only_waiting_on_subagents`], it only says what the Working
    /// Indicator's Working is spent on.
    #[serde(default)]
    pub waiting_on_subagents: Option<TurnId>,
}

impl SessionSnapshot {
    /// The descriptor of the Attachment stored under `id`, where this
    /// Session binds it and the server has described it.
    pub fn attachment(&self, id: &AttachmentId) -> Option<&AttachmentDescriptor> {
        self.attachments
            .iter()
            .find(|descriptor| &descriptor.id == id)
    }

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

    /// When this Session began Monitoring, using the server-derived reading
    /// carried by the Session itself.
    pub const fn monitoring_since(&self) -> Option<SessionTimestamp> {
        self.session.monitoring_since
    }

    /// Whether the only work open in this Session's current Turn is its
    /// Agent waiting on Subagents: a `wait_subagents` call is open in that
    /// Turn, the Turn still works, and nothing of its own — no Activity and
    /// no streaming Message — is in progress beside the Subagents it waits
    /// on. The Subagent rows are left out because they are what it waits on,
    /// whether spawned through the Broker or by its own Provider, and so is
    /// the Tool Call the Agent's Provider records the `wait_subagents` call
    /// as, because that Tool Call is the wait, not work beside it. Any other
    /// Tool Call still running is the Agent's own work, as a Command is.
    pub fn only_waiting_on_subagents(&self) -> bool {
        let Some(turn_id) = self.waiting_on_subagents else {
            return false;
        };
        let turn_works = self
            .turns
            .iter()
            .any(|turn| turn.id == turn_id && turn.status == TurnStatus::Active);
        let own_work = self.activities.iter().any(|activity| {
            activity.turn_id() == turn_id
                && !matches!(activity, Activity::Subagent { .. })
                && !activity.is_wait_on_subagents()
                && activity.status() == Some(ActivityStatus::Active)
        }) || self.messages.iter().any(|message| {
            message.turn_id == turn_id && message.status == MessageStatus::Streaming
        });
        turn_works && !own_work
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
    /// Descriptors of Attachments a Prompt about to be added binds, carried
    /// ahead of the Prompt so no client ever holds a binding it cannot
    /// describe. A descriptor the Session already carries may arrive again,
    /// and changes nothing when it does.
    AttachmentsDescribed {
        attachments: Vec<AttachmentDescriptor>,
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
    /// The Session withdrew a Pending Prompt of its own accord, for the
    /// reason `withdrawal` gives: the Prompt is Cancelled and carries it.
    PromptWithdrawn {
        prompt_id: PromptId,
        withdrawal: PromptWithdrawal,
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
    /// The Session's tree Cost or its own Cost moved; both are carried
    /// whole, since one Turn's Cost can move either.
    TotalCostChanged {
        total_cost: Option<CostTotal>,
        own_cost: Option<CostTotal>,
    },
    /// The whole Working reading derived by the server across this Session's
    /// Subagent subtree. It is carried as one value so a client joining an
    /// update stream never has to reconstruct work it did not observe begin.
    SessionWorkingChanged {
        working_since: Option<SessionTimestamp>,
    },
    /// The whole Monitoring reading derived by the server. Like Working it is
    /// never a Provider's or a reader's to set, and a Watch starting or
    /// settling can move it without any Turn moving.
    SessionMonitoringChanged {
        monitoring_since: Option<SessionTimestamp>,
    },
    /// The whole set of Watches live across this Session's subtree, derived
    /// by the server whenever a Watch starts or settles anywhere below it.
    /// Carried entire, like the Monitoring reading it explains, so a client
    /// joining the stream late never has to reconstruct a Watch it did not
    /// see start.
    SessionWatchesChanged {
        watches: Vec<WatchSummary>,
    },
    /// The Turn in which the Session's Agent now waits on its Subagents, or
    /// `None` once no such wait is open: set by the server as a
    /// `wait_subagents` call begins waiting and cleared however it ends.
    /// Like the Watches, it moves without any Turn moving.
    SessionWaitingOnSubagentsChanged {
        waiting_on_subagents: Option<TurnId>,
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
        /// Who answered or declined the Questionnaire on the user's behalf,
        /// where the user did not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<Author>,
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
    /// A Tool Call's input, rendered once the Provider sent it, in place of
    /// whatever the row held before.
    ToolCallInputChanged {
        activity_id: ActivityId,
        input: String,
        input_truncated: bool,
    },
    ToolCallOutputAppended {
        activity_id: ActivityId,
        content: String,
    },
    ToolCallOutputTruncated {
        activity_id: ActivityId,
    },
    ToolCallStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
        omitted_parts: u32,
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
    /// An Active Compaction Settles, carrying its record as it stands once
    /// settled: the Context Fill before and after where known, why it failed
    /// where the Provider said, and the summary it left where the Provider
    /// gave one.
    CompactionSettled {
        activity_id: ActivityId,
        status: ActivityStatus,
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
        error: Option<String>,
        summary: Option<String>,
        summary_truncated: bool,
    },
    /// A completed Compaction whose Provider reported no Context Fill after
    /// it takes the Session's first reading since it Settled instead, however
    /// long after that reading comes. It never replaces a count already known.
    CompactionAfterMeasured {
        activity_id: ActivityId,
        after_tokens: u64,
    },
    /// A Subsession's row naming it by the Title its Session has since
    /// taken.
    SubsessionTitleChanged {
        activity_id: ActivityId,
        title: String,
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
    /// Each Attachment, already uploaded, bound to its label in `text`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentBinding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub preparation_id: Option<PreparationId>,
    pub agent_selection: Option<AgentSelection>,
    pub execution_directory: ExecutionDirectory,
    pub prompt: InitialPrompt,
    /// The identity the Session is to have, where whoever begins it chose
    /// one before asking, so the beginning can be read after — and asked
    /// again — before its answer arrives. A Session the Prompt already began
    /// answers as any retry of the same creation does; an identity another
    /// Session holds, or not the one a Worktree preparation intends, is
    /// refused. Absent, the Server chooses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
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

/// The user's — or a Sidekick's — own Description for a Workspace, by the
/// Workspace's identity, carried in the body for the reason
/// [`SetWorkspaceIconRequest`] carries it there. It is kept as
/// [`one_line_description`] keeps it; text left blank by that clears the
/// Description, so the next Session created in the Workspace may derive one
/// again. Refused where this server cannot tell the Workspace is one of its
/// own, or where the text runs longer than a Description may.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetWorkspaceDescriptionRequest {
    pub workspace_id: WorkspaceId,
    /// Where the Workspace is presented — its [`Workspace::path`] — for one
    /// this server has begun no Session in and holds nothing of yet, such as
    /// a fresh Landing's. The server resolves it as it resolves any
    /// Workspace, and takes the Workspace as its own only where that
    /// resolution names `workspace_id`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub description: String,
}

/// Every Workspace a Server knows, as `GET /v1/workspaces` answers — a
/// Peer's request included — with the paths they are spelled in, so a reader
/// on another machine names each in the owning Server's own syntax, as the
/// Session catalog's snapshot lets it name the Workspaces Sessions work in.
/// The Workspaces are those its Sessions work in, most recently worked in
/// first, then those it holds a Description or Icon for though no Session
/// works there, by path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceListing {
    pub workspace_paths: WorkspacePaths,
    pub workspaces: Vec<Workspace>,
}

/// One Session as its Server holds it, as
/// `GET /v1/sessions/{session_id}/with-summary` answers — a Peer's request
/// included: its snapshot, and the summary its listing reads it by, taken
/// together in one moment. A reader on another machine reads how the Session
/// stands from the same instant as what it says, as this Server's own reads
/// do, so no Turn settling between two requests can set one against the
/// other. A Subagent's Session, which no listing carries, has its summary
/// too.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotWithSummary {
    pub snapshot: SessionSnapshot,
    pub summary: SessionSummary,
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
    /// A set Description runs longer than a Description may.
    InvalidDescription,
    SessionNotFound,
    SubagentSession,
    /// The request needs the Session idle — deleting it, or compacting its
    /// context — and it or a Subagent below it is still Working: running a
    /// Turn, owing one to a Prompt it admitted, or waiting on Subagents.
    WorkingSession,
    /// A Compaction was requested of a Session that owes its reader an
    /// Intervention, its own or one of a Subagent below it.
    PendingIntervention,
    /// A Compaction was requested of a Session whose Provider compacts only
    /// when it chooses to.
    CompactionUnsupported,
    /// A Compaction request carried instructions for its summary, and the
    /// Session's Provider takes none.
    CompactionInstructionsUnsupported,
    /// A Context Breakdown was requested of a Session whose Provider
    /// attributes none.
    ContextBreakdownUnsupported,
    /// A Context Breakdown was requested of a Session whose Provider is not
    /// running for it — none has started since Suru did — or whose
    /// conversation rides another Session's Provider, which cannot single
    /// it out.
    ContextBreakdownUnavailable,
    /// The Provider was asked for a Context Breakdown and gave none.
    ContextBreakdownFailed,
    PromptConflict,
    PromptNotFound,
    PromptNotPending,
    /// A queued Prompt was to be promoted to steer while a requested
    /// Compaction runs, whose Turn takes no steer; it stays queued.
    CompactionInProgress,
    /// An interrupt found nothing running: no active Turn, and no working
    /// Subagent anywhere below the Session.
    NothingToInterrupt,
    InterruptionFailed,
    QuestionnaireSubmissionFailed,
    DecisionSubmissionFailed,
    /// A per-Subagent stop named a Subagent whose Provider offers none.
    SubagentStopUnsupported,
    /// An act a Sidekick sent named a Session of the Sidekick Workspace, on
    /// which no Sidekick acts.
    SidekickWorkspaceSession,
    AgentSelectionOperationConflict,
    AgentSelectionProviderConflict,
    InvalidSkillInvocation,
    /// An upload's bytes are not a PNG, JPEG, GIF, or WebP image whose
    /// header can be read.
    UnsupportedAttachment,
    /// An upload's bytes exceed the per-image cap.
    AttachmentTooLarge,
    /// No stored Attachment has the named id.
    AttachmentNotFound,
    /// A Prompt's Attachment binding names a span outside its text, or a
    /// label the text does not carry at that span.
    InvalidAttachmentBinding,
    /// A Prompt binds more Attachments than one Prompt may carry.
    TooManyAttachments,
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
    /// The Server holds the Session but could not read what it stored of it.
    SessionUnreadable,
    /// A request carried through a Pairing reached the Server it was for,
    /// and its answer was lost on the way back, so whether what it asked was
    /// done is not known — and it is not asked again anywhere else.
    PairingOutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

impl SessionErrorCode {
    /// The name this code travels under, in a Session error's body and in its
    /// [`SESSION_ERROR_CODE_HEADER`].
    pub fn wire_name(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(name)) => name,
            _ => unreachable!("a unit variant serializes as its name"),
        }
    }
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

/// A Session began or stopped Monitoring, carried to a client that may be
/// listing that Session without having it open.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitoringChanged {
    pub session_id: SessionId,
    pub monitoring_since: Option<SessionTimestamp>,
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
    pub own_cost: Option<CostTotal>,
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

/// A Workspace's Description as a derivation, a setting, or a clearing left
/// it, carried to every client the way [`WorkspaceIconChanged`] is.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDescriptionChanged {
    pub workspace_id: WorkspaceId,
    pub description: Option<WorkspaceDescription>,
}

/// The Sessions a Sidekick's Session began on Remotes, as they now stand,
/// carried to a client that may be listing that Session without having it
/// open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRemoteSubsessionsChanged {
    pub session_id: SessionId,
    pub remote_subsessions: Vec<RemoteSession>,
}
