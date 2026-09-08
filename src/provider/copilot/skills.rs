//! Copilot-native Skill discovery and opaque command identity resolution.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use github_copilot_sdk::{
    SessionConfig, SessionId,
    rpc::{
        CommandsInvokeRequest, CommandsListRequest, Skill, SlashCommandInvocationResult,
        SlashCommandKind,
    },
    session::Session as NativeSession,
    session_events::SkillSource,
};
use tokio::sync::watch;

use super::{
    COPILOT_CLIENT_NAME, copilot_error, copilot_error_context, session::until_crash,
    transport::CopilotConnection,
};
use crate::{
    protocol::{
        ProviderId, SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillPromptDelivery,
    },
    provider::{ProviderError, ProviderPrompt, harness::SharedHarnessHandle},
};

const METHOD_NOT_FOUND: i32 = -32601;
const DISCOVERY_CONTEXT: &str = "Copilot Skill discovery failed";
const INCOMPATIBLE_MESSAGE: &str = "Copilot Skills require the experimental Session command interface supported by this Suru build; update the Copilot CLI to a compatible version";

#[derive(Clone, Debug)]
pub(super) struct CopilotSkills {
    store: Arc<Mutex<NativeStore>>,
    invalidations: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct NativeStore {
    generation: u64,
    workspaces: HashMap<PathBuf, NativeCatalog>,
}

#[derive(Clone, Debug)]
struct NativeCatalog {
    skills: HashMap<SkillId, NativeSkill>,
    steer_supported: bool,
}

#[derive(Clone, Debug)]
struct NativeSkill {
    command_name: String,
}

impl Default for CopilotSkills {
    fn default() -> Self {
        let (invalidations, _) = watch::channel(0);
        Self {
            store: Arc::new(Mutex::new(NativeStore::default())),
            invalidations,
        }
    }
}

impl CopilotSkills {
    pub(super) fn subscribe_invalidations(&self) -> watch::Receiver<u64> {
        self.invalidations.subscribe()
    }

    pub(super) fn native_catalog_changed(&self) {
        let generation = {
            let mut store = self
                .store
                .lock()
                .expect("Copilot Skill store lock is not poisoned");
            store.generation = store.generation.saturating_add(1);
            store.workspaces.clear();
            store.generation
        };
        self.invalidations.send_replace(generation);
    }

    fn begin_discovery(&self, execution_directory: &Path) -> u64 {
        let mut store = self
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned");
        store.workspaces.remove(execution_directory);
        store.generation
    }

    fn commit_discovery(
        &self,
        execution_directory: &Path,
        generation: u64,
        catalog: NativeCatalog,
    ) -> Result<(), ProviderError> {
        let mut store = self
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned");
        if store.generation != generation {
            return Err(copilot_error(
                "Copilot Skills changed while their Catalog was being discovered",
            ));
        }
        store
            .workspaces
            .insert(execution_directory.to_owned(), catalog);
        Ok(())
    }

    pub(super) async fn discover(
        &self,
        handle: &SharedHarnessHandle<CopilotConnection>,
        execution_directory: &Path,
    ) -> Result<SkillCatalog, ProviderError> {
        let generation = self.begin_discovery(execution_directory);
        let session_id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let config = SessionConfig::default()
            .with_session_id(session_id.clone())
            .with_client_name(COPILOT_CLIENT_NAME)
            .with_working_directory(execution_directory)
            .with_streaming(false)
            .with_enable_config_discovery(true)
            .with_enable_skills(true)
            .approve_all_permissions();
        let native = until_crash(
            handle,
            DISCOVERY_CONTEXT,
            handle.connection().client().create_session(config),
        )
        .await?;
        let rpc = native.rpc();
        let commands = rpc.commands();
        let commands = tokio::select! {
            biased;
            crashed = handle.crashed() => Err(copilot_error_context(DISCOVERY_CONTEXT, crashed)),
            listed = commands.list_with_params(CommandsListRequest {
                include_builtins: Some(false),
                include_client_commands: Some(false),
                include_skills: Some(true),
            }) => match listed {
                Ok(listed) => Ok(Some(listed)),
                Err(error) if error.rpc_code() == Some(METHOD_NOT_FOUND) => Ok(None),
                Err(error) => Err(handle.connection().failure(DISCOVERY_CONTEXT, error)),
            },
        };
        let skills = if matches!(commands, Ok(Some(_))) {
            let rpc = native.rpc();
            let skills = rpc.skills();
            tokio::select! {
                biased;
                crashed = handle.crashed() => Err(copilot_error_context(DISCOVERY_CONTEXT, crashed)),
                listed = skills.list() => match listed {
                    Ok(listed) => Ok(Some(listed)),
                    Err(error) if error.rpc_code() == Some(METHOD_NOT_FOUND) => Ok(None),
                    Err(error) => Err(handle.connection().failure(DISCOVERY_CONTEXT, error)),
                },
            }
        } else {
            Ok(None)
        };

        if let Err(error) = native.disconnect().await {
            tracing::warn!("failed to disconnect Copilot Skill discovery Session: {error}");
        }
        if let Err(error) = handle
            .connection()
            .client()
            .delete_session(&session_id)
            .await
        {
            tracing::warn!("failed to delete Copilot Skill discovery Session: {error}");
        }

        let (Some(commands), Some(skills)) = (commands?, skills?) else {
            return Ok(Self::incompatible_catalog(execution_directory));
        };
        let mut commands = commands
            .commands
            .into_iter()
            .filter(|command| command.kind == SlashCommandKind::Skill)
            .map(|command| (command.name.clone(), command))
            .collect::<HashMap<_, _>>();
        let mut native = HashMap::new();
        let mut descriptors = Vec::new();
        let mut steer_supported = true;
        let mut invalid_entries = 0usize;
        for skill in skills
            .skills
            .into_iter()
            .filter(|skill| skill.enabled && skill.user_invocable)
        {
            let Some(command_name) = skill.command_name.as_deref() else {
                invalid_entries += 1;
                continue;
            };
            let Some(command) = commands.remove(command_name) else {
                invalid_entries += 1;
                continue;
            };
            if skill.name.is_empty() || skill.name.chars().any(char::is_whitespace) {
                invalid_entries += 1;
                continue;
            }
            let id = opaque_skill_id(execution_directory, &skill);
            if native.contains_key(&id) {
                invalid_entries += 1;
                continue;
            }
            steer_supported &= command.allow_during_agent_execution;
            native.insert(
                id.clone(),
                NativeSkill {
                    command_name: command.name,
                },
            );
            descriptors.push(SkillDescriptor {
                id,
                name: skill.name,
                description: skill.description,
                scope: Some(safe_scope(skill.source).to_owned()),
            });
        }
        descriptors.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        self.commit_discovery(
            execution_directory,
            generation,
            NativeCatalog {
                skills: native,
                steer_supported,
            },
        )?;
        Ok(SkillCatalog {
            provider: ProviderId::new("copilot"),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: execution_directory.to_owned(),
            },
            skills: descriptors,
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: Some(1),
                supported_deliveries: if steer_supported {
                    vec![
                        SkillPromptDelivery::Initial,
                        SkillPromptDelivery::Queue,
                        SkillPromptDelivery::Steer,
                    ]
                } else {
                    vec![SkillPromptDelivery::Initial, SkillPromptDelivery::Queue]
                },
            },
            status: SkillCatalogStatus::Fresh {
                warning: (invalid_entries > 0).then(|| {
                    format!(
                        "Copilot skipped {invalid_entries} invalid Skill entr{}",
                        if invalid_entries == 1 { "y" } else { "ies" }
                    )
                }),
            },
        })
    }

    pub(super) fn incompatible_catalog(execution_directory: &Path) -> SkillCatalog {
        SkillCatalog {
            provider: ProviderId::new("copilot"),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: execution_directory.to_owned(),
            },
            skills: Vec::new(),
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: Some(0),
                supported_deliveries: Vec::new(),
            },
            status: SkillCatalogStatus::Unavailable {
                message: INCOMPATIBLE_MESSAGE.to_owned(),
            },
        }
    }

    pub(super) async fn expand(
        &self,
        handle: &SharedHarnessHandle<CopilotConnection>,
        execution_directory: &Path,
        session: &NativeSession,
        delivery: SkillPromptDelivery,
        prompt: ProviderPrompt,
    ) -> Result<String, ProviderError> {
        let [] = prompt.skill_invocations.as_slice() else {
            if prompt.skill_invocations.len() != 1 {
                return Err(copilot_error(
                    "Copilot supports at most one distinct Skill per Prompt",
                ));
            }
            let invocation = &prompt.skill_invocations[0];
            let command_name =
                self.resolve_command(execution_directory, &invocation.skill_id, delivery)?;
            let input = prompt.without_skill_markers("Copilot")?;
            let result = until_crash(
                handle,
                "Copilot Skill Invocation failed",
                session.rpc().commands().invoke(CommandsInvokeRequest {
                    input: Some(input),
                    name: command_name,
                    origin: None,
                }),
            )
            .await?;
            return match result {
                SlashCommandInvocationResult::AgentPrompt(expanded) => {
                    Ok(prevent_redundant_skill_invocation(expanded.prompt))
                }
                _ => Err(copilot_error(
                    "Copilot Skill Invocation did not return an agent Prompt",
                )),
            };
        };
        Ok(prompt.text)
    }

    fn resolve_command(
        &self,
        execution_directory: &Path,
        skill_id: &SkillId,
        delivery: SkillPromptDelivery,
    ) -> Result<String, ProviderError> {
        let native = self
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned")
            .workspaces
            .get(execution_directory)
            .map(|catalog| {
                (
                    catalog.skills.get(skill_id).cloned(),
                    catalog.steer_supported,
                )
            })
            .ok_or_else(|| {
                copilot_error(format!(
                    "Copilot Skill identity `{skill_id}` is no longer valid in this Workspace"
                ))
            })?;
        let (native, steer_supported) = native;
        let native = native.ok_or_else(|| {
            copilot_error(format!(
                "Copilot Skill identity `{skill_id}` is no longer valid in this Workspace"
            ))
        })?;
        if delivery == SkillPromptDelivery::Steer && !steer_supported {
            return Err(copilot_error(format!(
                "Copilot Skill identity `{skill_id}` is no longer available for Steer Prompts"
            )));
        }
        Ok(native.command_name)
    }
}

/// Rewords Copilot's explicit-Skill preamble so the model follows the Skill content already
/// embedded below it instead of trying to load the same Skill again through the model-facing
/// `skill` Tool. That second route deliberately excludes Skills marked `disable-model-invocation`,
/// making an otherwise successful user invocation look like it failed.
///
/// The native command response is an experimental surface, so an unfamiliar shape passes through
/// untouched rather than risking damage to a future expansion format.
fn prevent_redundant_skill_invocation(expanded: String) -> String {
    const PREFIX: &str = "The user explicitly invoked the \"";
    const SUFFIX: &str = "\" skill. Follow its instructions now.";
    const REPLACEMENT: &str = "The following skill has already been loaded. Follow its supplied instructions directly; do not invoke the `skill` tool again.";

    let separator = if expanded.contains("\r\n\r\n") {
        "\r\n\r\n"
    } else {
        "\n\n"
    };
    let Some(paragraph_end) = expanded.find(separator) else {
        return expanded;
    };
    let introduction = &expanded[..paragraph_end];
    let context = &expanded[paragraph_end + separator.len()..];
    let Some(skill_name) = introduction
        .strip_prefix(PREFIX)
        .and_then(|introduction| introduction.strip_suffix(SUFFIX))
    else {
        return expanded;
    };
    if !skill_name.starts_with('/')
        || skill_name.len() == 1
        || skill_name.chars().any(char::is_whitespace)
        || !context.starts_with("<skill-context")
    {
        return expanded;
    }

    format!("{REPLACEMENT}{separator}{context}")
}

fn safe_scope(source: SkillSource) -> &'static str {
    match source {
        SkillSource::Project => "Workspace",
        SkillSource::Inherited => "Inherited",
        SkillSource::PersonalCopilot | SkillSource::PersonalAgents => "User",
        SkillSource::Plugin => "Plugin",
        SkillSource::Custom => "Custom",
        SkillSource::Builtin => "Built-in",
        SkillSource::Unknown => "Other",
    }
}

fn opaque_skill_id(execution_directory: &Path, skill: &Skill) -> SkillId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"suru:copilot-skill:v1\0");
    hash.update(execution_directory.to_string_lossy().as_bytes());
    for component in [
        serde_json::to_string(&skill.source).expect("Copilot Skill source serializes"),
        skill.path.clone().unwrap_or_default(),
        skill.plugin_name.clone().unwrap_or_default(),
        skill.name.clone(),
    ] {
        hash.update(b"\0");
        hash.update(component.as_bytes());
    }
    SkillId::new(format!("copilot-{}", hash.finalize().to_hex()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_skill(path: &str) -> Skill {
        Skill {
            name: "review".to_owned(),
            description: "Review the current change".to_owned(),
            source: SkillSource::Project,
            enabled: true,
            user_invocable: true,
            command_name: Some("native-review".to_owned()),
            path: Some(path.to_owned()),
            ..Skill::default()
        }
    }

    #[test]
    fn native_change_revokes_every_binding_before_broadcasting_invalidation() {
        let skills = CopilotSkills::default();
        let execution_directory = PathBuf::from("execution_directory");
        let id = SkillId::new("copilot-fixture");
        skills
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned")
            .workspaces
            .insert(
                execution_directory.clone(),
                NativeCatalog {
                    skills: HashMap::from([(
                        id.clone(),
                        NativeSkill {
                            command_name: "native-review".to_owned(),
                        },
                    )]),
                    steer_supported: true,
                },
            );
        let invalidations = skills.subscribe_invalidations();

        skills.native_catalog_changed();

        assert!(
            skills
                .store
                .lock()
                .expect("Copilot Skill store lock is not poisoned")
                .workspaces
                .is_empty(),
            "native changes revoke bindings before an asynchronous refresh"
        );
        assert!(invalidations.has_changed().expect("watch remains open"));
    }

    #[test]
    fn opaque_identity_is_stateless_workspace_scoped_and_source_sensitive() {
        let execution_directory = Path::new("execution_directory-a");
        let skill = native_skill("skill-location-a");

        assert_eq!(
            opaque_skill_id(execution_directory, &skill),
            opaque_skill_id(execution_directory, &skill),
            "reconstructed runtime state derives the same identity"
        );
        assert_ne!(
            opaque_skill_id(execution_directory, &skill),
            opaque_skill_id(Path::new("execution_directory-b"), &skill),
            "another Workspace has another identity namespace"
        );
        assert_ne!(
            opaque_skill_id(execution_directory, &skill),
            opaque_skill_id(execution_directory, &native_skill("skill-location-b")),
            "moving or replacing the native source changes identity"
        );
    }

    #[test]
    fn refreshed_binding_rechecks_native_delivery_support() {
        let skills = CopilotSkills::default();
        let execution_directory = PathBuf::from("execution_directory");
        let id = SkillId::new("copilot-fixture");
        skills
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned")
            .workspaces
            .insert(
                execution_directory.clone(),
                NativeCatalog {
                    skills: HashMap::from([(
                        id.clone(),
                        NativeSkill {
                            command_name: "native-review".to_owned(),
                        },
                    )]),
                    steer_supported: false,
                },
            );

        assert!(
            skills
                .resolve_command(&execution_directory, &id, SkillPromptDelivery::Initial)
                .is_ok()
        );
        let error = skills
            .resolve_command(&execution_directory, &id, SkillPromptDelivery::Steer)
            .expect_err("refreshed non-steer binding rejects stale Steer admission");
        assert!(error.to_string().contains("Steer"));
    }

    #[test]
    fn discovery_revokes_only_the_workspace_being_refreshed() {
        let skills = CopilotSkills::default();
        let refreshed = PathBuf::from("execution_directory-a");
        let untouched = PathBuf::from("execution_directory-b");
        let catalog = || NativeCatalog {
            skills: HashMap::new(),
            steer_supported: true,
        };
        skills
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned")
            .workspaces
            .extend([
                (refreshed.clone(), catalog()),
                (untouched.clone(), catalog()),
            ]);

        skills.begin_discovery(&refreshed);

        let workspaces = skills
            .store
            .lock()
            .expect("Copilot Skill store lock is not poisoned");
        assert!(!workspaces.workspaces.contains_key(&refreshed));
        assert!(workspaces.workspaces.contains_key(&untouched));
    }

    #[test]
    fn discovery_started_before_native_invalidation_cannot_restore_authority() {
        let skills = CopilotSkills::default();
        let execution_directory = PathBuf::from("execution_directory");
        let generation = skills.begin_discovery(&execution_directory);
        skills.native_catalog_changed();

        let committed = skills.commit_discovery(
            &execution_directory,
            generation,
            NativeCatalog {
                skills: HashMap::new(),
                steer_supported: true,
            },
        );

        assert!(committed.is_err());
        assert!(
            skills
                .store
                .lock()
                .expect("Copilot Skill store lock is not poisoned")
                .workspaces
                .is_empty()
        );
    }

    #[test]
    fn explicit_skill_expansion_tells_the_model_not_to_reload_the_skill() {
        let context = concat!(
            "<skill-context name=\"ask-matt\">\n",
            "# Ask Matt\n\n",
            "Use the right flow.\n",
            "</skill-context>",
        );
        for separator in ["\n\n", "\r\n\r\n"] {
            let expanded = format!(
                "The user explicitly invoked the \"/ask-matt\" skill. Follow its instructions now.{separator}{context}"
            );

            assert_eq!(
                prevent_redundant_skill_invocation(expanded),
                format!(
                    "The following skill has already been loaded. Follow its supplied instructions directly; do not invoke the `skill` tool again.{separator}{context}"
                )
            );
        }
    }

    #[test]
    fn unfamiliar_skill_expansion_is_not_rewritten() {
        let expanded = concat!(
            "Follow the selected skill.\n\n",
            "<skill-context name=\"review\">\n",
            "Review carefully.\n",
            "</skill-context>",
        )
        .to_owned();

        assert_eq!(
            prevent_redundant_skill_invocation(expanded.clone()),
            expanded
        );
    }
}
