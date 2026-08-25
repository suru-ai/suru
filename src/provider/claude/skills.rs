//! Claude-native Skill discovery, opaque identity resolution, and Prompt lowering.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::time::Duration;

use super::{
    CLAUDE_PROVIDER_ID, claude_error, claude_error_context,
    transport::{ClaudeConnection, ClaudeSettingSources, StreamJsonTransport},
    wire::{ControlRequest, NativeInitialize, NativeSkill, NativeSkillList},
};
use crate::{
    protocol::{
        ProviderId, SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillPromptDelivery, Workspace,
    },
    provider::{ProviderError, ProviderPrompt, harness::ProcessRegistry},
};

const DISCOVERY_CONTEXT: &str = "Claude Skill discovery failed";
const MAX_DISTINCT_SKILLS: usize = 6;

#[derive(Clone, Debug, Default)]
pub(super) struct ClaudeSkills {
    catalogs: Arc<Mutex<HashMap<PathBuf, NativeCatalog>>>,
}

#[derive(Clone, Debug)]
struct NativeCatalog {
    skills: HashMap<SkillId, NativeEntry>,
}

#[derive(Clone, Debug)]
struct NativeEntry {
    command_name: String,
}

impl ClaudeSkills {
    pub(super) async fn discover(
        &self,
        executable: OsString,
        processes: ProcessRegistry,
        workspace: PathBuf,
        request_timeout: Duration,
    ) -> Result<SkillCatalog, ProviderError> {
        let args = [
            OsString::from("--dangerously-skip-permissions"),
            OsString::from("--no-session-persistence"),
        ];
        let ClaudeConnection { transport, process } = StreamJsonTransport::launch(
            &executable,
            args,
            Some(workspace.clone()),
            None,
            processes,
            ClaudeSettingSources::PersonalAndProject,
        )
        .await
        .map_err(|error| claude_error_context(DISCOVERY_CONTEXT, error))?;

        // Claude's dedicated Skill projection contains what the model can invoke. Initialization's
        // broader command projection also carries Skills reserved for explicit user invocation,
        // mixed with built-in slash commands. Both are needed for Suru's user-facing catalog.
        let listed = async {
            let initialized = transport
                .control_request(&ControlRequest::Initialize, request_timeout)
                .await
                .map_err(|failure| claude_error_context(DISCOVERY_CONTEXT, failure.into_error()))?;
            let initialized: NativeInitialize =
                serde_json::from_value(initialized).map_err(|error| {
                    claude_error(format!(
                        "Claude Code CLI returned an invalid initialize response: {error}"
                    ))
                })?;
            let listed = transport
                .control_request(&ControlRequest::ReloadSkills, request_timeout)
                .await
                .map_err(|failure| claude_error_context(DISCOVERY_CONTEXT, failure.into_error()))?;
            let listed: NativeSkillList = serde_json::from_value(listed).map_err(|error| {
                claude_error(format!(
                    "Claude Code CLI returned an invalid reload_skills response: {error}"
                ))
            })?;
            Ok::<_, ProviderError>((initialized, listed))
        }
        .await;
        transport.close().await;
        let stopped = process
            .wait_until_stopped()
            .await
            .map_err(|error| claude_error_context(DISCOVERY_CONTEXT, error));

        // Prefer the discovery error when there is one: stopping the child is cleanup, not the
        // operation the user asked for. Either way, the short-lived process is gone first.
        let (initialized, listed) = listed?;
        stopped?;
        let mut by_name = HashMap::<String, (NativeSkill, SafeMetadata)>::new();
        let mut invalid_entries = 0usize;
        for skill in listed.skills {
            if skill.name.is_empty()
                || skill.name.chars().any(char::is_whitespace)
                || skill.description.trim().is_empty()
            {
                invalid_entries += 1;
                continue;
            }
            let metadata = safe_metadata(&skill.description);
            // Claude returns native sources in precedence order. A later entry with the same
            // canonical name is the effective override the slash invocation resolves.
            by_name.insert(skill.name.to_ascii_lowercase(), (skill, metadata));
        }
        for skill in initialized.commands {
            let key = skill.name.to_ascii_lowercase();
            if by_name.contains_key(&key) {
                continue;
            }
            let Some(metadata) = scoped_metadata(&skill.description) else {
                // Initialization also contains built-in and other non-Skill slash commands. Native
                // Skills carry source metadata in their descriptions; unscoped commands do not.
                continue;
            };
            if skill.name.is_empty()
                || skill.name.chars().any(char::is_whitespace)
                || metadata.description.trim().is_empty()
            {
                invalid_entries += 1;
                continue;
            }
            by_name.insert(key, (skill, metadata));
        }

        let mut native = HashMap::new();
        let mut descriptors = by_name
            .into_values()
            .map(|(skill, metadata)| {
                let id = opaque_skill_id(&workspace, &skill, &metadata.scope);
                native.insert(
                    id.clone(),
                    NativeEntry {
                        command_name: skill.name.clone(),
                    },
                );
                SkillDescriptor {
                    id,
                    name: skill.name,
                    description: metadata.description,
                    scope: Some(metadata.scope),
                }
            })
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        self.catalogs
            .lock()
            .expect("Claude Skill store lock is not poisoned")
            .insert(workspace.clone(), NativeCatalog { skills: native });

        Ok(SkillCatalog {
            provider: ProviderId::new(CLAUDE_PROVIDER_ID),
            workspace: Workspace { path: workspace },
            skills: descriptors,
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: Some(MAX_DISTINCT_SKILLS as u32),
                supported_deliveries: vec![
                    SkillPromptDelivery::Initial,
                    SkillPromptDelivery::Queue,
                ],
            },
            status: SkillCatalogStatus::Fresh {
                warning: (invalid_entries > 0).then(|| {
                    format!(
                        "Claude skipped {invalid_entries} invalid Skill entr{}",
                        if invalid_entries == 1 { "y" } else { "ies" }
                    )
                }),
            },
        })
    }

    pub(super) fn lower(
        &self,
        workspace: &Path,
        prompt: ProviderPrompt,
    ) -> Result<String, ProviderError> {
        if prompt.skill_invocations.is_empty() {
            return Ok(prompt.text);
        }
        if prompt.skill_invocations.len() > MAX_DISTINCT_SKILLS {
            return Err(claude_error(
                "Claude supports at most six distinct Skills per Prompt",
            ));
        }
        let catalogs = self
            .catalogs
            .lock()
            .expect("Claude Skill store lock is not poisoned");
        let catalog = catalogs.get(workspace).ok_or_else(|| {
            claude_error("Claude Skill identities are no longer valid in this Workspace")
        })?;
        let commands = prompt
            .skill_invocations
            .iter()
            .map(|invocation| {
                catalog
                    .skills
                    .get(&invocation.skill_id)
                    .map(|entry| format!("/{}", entry.command_name))
                    .ok_or_else(|| {
                        claude_error(format!(
                            "Claude Skill identity `{}` is no longer valid in this Workspace",
                            invocation.skill_id
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        drop(catalogs);

        let remainder = prompt.without_skill_markers("Claude")?;
        let prefix = commands.join("\n");
        if remainder.is_empty() {
            Ok(prefix)
        } else {
            Ok(format!("{prefix}\n{remainder}"))
        }
    }
}

struct SafeMetadata {
    description: String,
    scope: String,
}

fn safe_metadata(description: &str) -> SafeMetadata {
    scoped_metadata(description).unwrap_or_else(|| SafeMetadata {
        description: description.to_owned(),
        scope: "Built-in".to_owned(),
    })
}

fn scoped_metadata(description: &str) -> Option<SafeMetadata> {
    for (suffix, scope) in [
        (" (project)", "Workspace"),
        (" (project, gitignored)", "Workspace"),
        (" (user)", "User"),
        (" (plugin)", "Plugin"),
    ] {
        if let Some(description) = description.strip_suffix(suffix) {
            return Some(SafeMetadata {
                description: description.to_owned(),
                scope: scope.to_owned(),
            });
        }
    }
    if let Some((plugin, description)) = description
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(") "))
    {
        return Some(SafeMetadata {
            description: description.to_owned(),
            scope: format!("Plugin · {plugin}"),
        });
    }
    None
}

fn opaque_skill_id(workspace: &Path, skill: &NativeSkill, scope: &str) -> SkillId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"suru:claude-skill:v1\0");
    hash.update(workspace.to_string_lossy().as_bytes());
    for component in [&skill.name, &skill.description, &skill.argument_hint, scope] {
        hash.update(b"\0");
        hash.update(component.as_bytes());
    }
    SkillId::new(format!("claude-{}", hash.finalize().to_hex()))
}
