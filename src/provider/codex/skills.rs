//! Codex-native Skill discovery and opaque identity resolution.

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::{
    codex_error, codex_error_context,
    transport::JsonRpcTransport,
    wire::{NativeSkillMetadata, NativeSkillScope, NativeSkillsList, SkillsListParams, UserInput},
};
use crate::{
    protocol::{
        ProviderId, SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillPromptDelivery, Workspace,
    },
    provider::{ProviderError, ProviderPrompt, harness::ProcessRegistry},
};

#[derive(Clone, Debug, Default)]
pub(super) struct CodexSkills {
    workspaces: Arc<Mutex<HashMap<PathBuf, HashMap<SkillId, NativeSkill>>>>,
}

#[derive(Clone, Debug)]
struct NativeSkill {
    name: String,
    path: PathBuf,
}

impl CodexSkills {
    pub(super) async fn discover(
        &self,
        executable: &OsStr,
        processes: ProcessRegistry,
        workspace: &Path,
        force_reload: bool,
    ) -> Result<SkillCatalog, ProviderError> {
        let connection = JsonRpcTransport::launch(executable, processes).await?;
        let result = connection
            .transport
            .request(
                "skills/list",
                &SkillsListParams {
                    cwds: [workspace],
                    force_reload,
                },
            )
            .await;
        connection.transport.close().await;
        let stopped = connection.process.wait_until_stopped().await;
        let result =
            result.map_err(|error| codex_error_context("Codex Skill discovery failed", error))?;
        stopped?;
        let listed: NativeSkillsList = serde_json::from_value(result).map_err(|error| {
            codex_error(format!(
                "Codex returned an invalid skills/list response: {error}"
            ))
        })?;
        let mut entries = listed.data.into_iter();
        let entry = entries.next().ok_or_else(|| {
            codex_error("Codex returned no Workspace entry in its skills/list response")
        })?;
        if entries.next().is_some() || entry.cwd != workspace {
            return Err(codex_error(
                "Codex returned a skills/list response for another Workspace",
            ));
        }

        let warning = (!entry.errors.is_empty()).then(|| {
            format!(
                "Codex skipped {} invalid Skill entr{}",
                entry.errors.len(),
                if entry.errors.len() == 1 { "y" } else { "ies" }
            )
        });
        let mut native = HashMap::new();
        let mut skills = Vec::new();
        for skill in entry.skills.into_iter().filter(|skill| skill.enabled) {
            validate_native_skill(&skill)?;
            let id = opaque_skill_id(workspace, &skill.path);
            native.insert(
                id.clone(),
                NativeSkill {
                    name: skill.name.clone(),
                    path: skill.path,
                },
            );
            skills.push(SkillDescriptor {
                id,
                name: skill.name,
                description: skill.description,
                scope: Some(safe_scope(skill.scope).to_owned()),
            });
        }
        skills.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.scope.cmp(&right.scope))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        self.workspaces
            .lock()
            .expect("Codex Skill store lock is not poisoned")
            .insert(workspace.to_owned(), native);
        Ok(SkillCatalog {
            provider: ProviderId::new("codex"),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            skills,
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: vec![
                    SkillPromptDelivery::Initial,
                    SkillPromptDelivery::Queue,
                    SkillPromptDelivery::Steer,
                ],
            },
            status: SkillCatalogStatus::Fresh { warning },
        })
    }

    pub(super) fn lower(
        &self,
        workspace: &Path,
        prompt: ProviderPrompt,
    ) -> Result<Vec<UserInput>, ProviderError> {
        let workspaces = self
            .workspaces
            .lock()
            .expect("Codex Skill store lock is not poisoned");
        let native = workspaces.get(workspace);
        let mut input = vec![UserInput::Text { text: prompt.text }];
        for invocation in prompt.skill_invocations {
            let skill = native
                .and_then(|skills| skills.get(&invocation.skill_id))
                .ok_or_else(|| {
                    codex_error(format!(
                        "Codex Skill identity `{}` is no longer valid in this Workspace",
                        invocation.skill_id
                    ))
                })?;
            input.push(UserInput::Skill {
                name: skill.name.clone(),
                path: skill.path.clone(),
            });
        }
        Ok(input)
    }
}

fn validate_native_skill(skill: &NativeSkillMetadata) -> Result<(), ProviderError> {
    if skill.name.is_empty() || skill.name.chars().any(char::is_whitespace) {
        return Err(codex_error(
            "Codex returned a Skill with an invalid canonical name",
        ));
    }
    if !skill.path.is_absolute() {
        return Err(codex_error(
            "Codex returned a Skill whose native path was not absolute",
        ));
    }
    Ok(())
}

fn safe_scope(scope: NativeSkillScope) -> &'static str {
    match scope {
        NativeSkillScope::User => "User",
        NativeSkillScope::Repo => "Workspace",
        NativeSkillScope::System => "System",
        NativeSkillScope::Admin => "Admin",
    }
}

fn opaque_skill_id(workspace: &Path, native_path: &Path) -> SkillId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"suru:codex-skill:v1\0");
    hash.update(workspace.to_string_lossy().as_bytes());
    hash.update(b"\0");
    hash.update(native_path.to_string_lossy().as_bytes());
    SkillId::new(format!("codex-{}", hash.finalize().to_hex()))
}
