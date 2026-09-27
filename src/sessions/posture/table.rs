//! The fixed table between the Providers' Approval Postures (ADR 0036): how a
//! brokered Subagent on a Provider other than its spawner's reads the rough
//! equivalent of its spawner's posture.
//!
//! Every native value sits at one of four levels, and every level is one
//! canonical value of each Provider. Where a Provider has no twin for a value
//! the table errs toward asking, because an unwanted Intervention is
//! recoverable and an unwanted permission is not. The table is a constant,
//! not a Setting: the user-facing Settings stay native (ADR 0026), and
//! changing a cell is a code change recorded in ADR 0036.

use crate::protocol::{
    ApprovalPosture, ClaudePermissionMode, CodexApprovalPolicy, CodexSandboxMode,
    CopilotPermissions, ProviderId,
};

/// How much an Agent may do unasked, the one scale ADR 0036 reads every
/// Provider's native values on.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum PostureLevel {
    /// Level 1: every edit or command that needs consent asks.
    AsksForConsent,
    /// Level 2: routine work in the workspace runs; going beyond it asks.
    RunsRoutineWork,
    /// Level 3: nothing asks; work outside the sandbox or allowlist is
    /// refused.
    RunsContained,
    /// Level 4: nothing asks; everything is allowed.
    RunsUnrestricted,
}

impl PostureLevel {
    /// The level `posture` sits at.
    pub(super) const fn of(posture: ApprovalPosture) -> Self {
        use ClaudePermissionMode as Claude;
        use CodexApprovalPolicy as Policy;
        use CodexSandboxMode as Sandbox;
        use CopilotPermissions as Copilot;
        match posture {
            ApprovalPosture::Claude { permission_mode } => match permission_mode {
                Claude::Default => Self::AsksForConsent,
                // Claude's auto classifier still asks where it is unsure.
                Claude::AcceptEdits | Claude::Auto => Self::RunsRoutineWork,
                Claude::DontAsk => Self::RunsContained,
                Claude::BypassPermissions => Self::RunsUnrestricted,
            },
            ApprovalPosture::Codex {
                approval_policy,
                sandbox_mode,
            } => match (approval_policy, sandbox_mode) {
                // Neither other Provider can say read-only, so a read-only
                // sandbox reads as the level below the policy it sits under.
                (Policy::Untrusted, _) | (Policy::OnRequest, Sandbox::ReadOnly) => {
                    Self::AsksForConsent
                }
                (Policy::OnRequest, Sandbox::WorkspaceWrite | Sandbox::DangerFullAccess) => {
                    Self::RunsRoutineWork
                }
                (Policy::Never, Sandbox::ReadOnly | Sandbox::WorkspaceWrite) => Self::RunsContained,
                (Policy::Never, Sandbox::DangerFullAccess) => Self::RunsUnrestricted,
            },
            ApprovalPosture::Copilot { permissions } => match permissions {
                Copilot::Ask => Self::AsksForConsent,
                Copilot::AllowAll => Self::RunsUnrestricted,
            },
        }
    }

    /// The one value `provider` acts under at this level, or `None` for a
    /// Provider whose native values the table has no column for — one with no
    /// native posture Suru can name, as [`ApprovalPosture::for_provider`]
    /// knows none for it either.
    pub(super) fn canonical(self, provider: &ProviderId) -> Option<ApprovalPosture> {
        use ClaudePermissionMode as Claude;
        use CodexApprovalPolicy as Policy;
        use CodexSandboxMode as Sandbox;
        use CopilotPermissions as Copilot;
        Some(match provider.as_str() {
            "claude" => ApprovalPosture::Claude {
                permission_mode: match self {
                    Self::AsksForConsent => Claude::Default,
                    Self::RunsRoutineWork => Claude::AcceptEdits,
                    Self::RunsContained => Claude::DontAsk,
                    Self::RunsUnrestricted => Claude::BypassPermissions,
                },
            },
            "codex" => {
                let (approval_policy, sandbox_mode) = match self {
                    Self::AsksForConsent => (Policy::Untrusted, Sandbox::WorkspaceWrite),
                    Self::RunsRoutineWork => (Policy::OnRequest, Sandbox::WorkspaceWrite),
                    Self::RunsContained => (Policy::Never, Sandbox::WorkspaceWrite),
                    Self::RunsUnrestricted => (Policy::Never, Sandbox::DangerFullAccess),
                };
                ApprovalPosture::Codex {
                    approval_policy,
                    sandbox_mode,
                }
            }
            // Copilot has no value that runs unasked yet stays contained, so
            // every level short of the last asks.
            "copilot" => ApprovalPosture::Copilot {
                permissions: match self {
                    Self::AsksForConsent | Self::RunsRoutineWork | Self::RunsContained => {
                        Copilot::Ask
                    }
                    Self::RunsUnrestricted => Copilot::AllowAll,
                },
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PostureLevel::{self, *};
    use crate::protocol::{
        ApprovalPosture, ClaudePermissionMode, CodexApprovalPolicy, CodexSandboxMode,
        CopilotPermissions, ProviderId,
    };

    const LEVELS: [PostureLevel; 4] = [
        AsksForConsent,
        RunsRoutineWork,
        RunsContained,
        RunsUnrestricted,
    ];

    /// The Providers the table has a column for.
    const PROVIDERS: [&str; 3] = ["claude", "codex", "copilot"];

    fn claude(permission_mode: ClaudePermissionMode) -> ApprovalPosture {
        ApprovalPosture::Claude { permission_mode }
    }

    fn codex(
        approval_policy: CodexApprovalPolicy,
        sandbox_mode: CodexSandboxMode,
    ) -> ApprovalPosture {
        ApprovalPosture::Codex {
            approval_policy,
            sandbox_mode,
        }
    }

    fn copilot(permissions: CopilotPermissions) -> ApprovalPosture {
        ApprovalPosture::Copilot { permissions }
    }

    /// Every native value each Provider offers. Each list sits beside a match
    /// over the type it lists, so a value added to any native type fails to
    /// compile here until it is listed, and then fails the tests below until
    /// ADR 0036 places it.
    fn every_native_value() -> Vec<ApprovalPosture> {
        use ClaudePermissionMode as Claude;
        use CodexApprovalPolicy as Policy;
        use CodexSandboxMode as Sandbox;
        use CopilotPermissions as Copilot;
        let _: fn(ApprovalPosture) = |posture| match posture {
            ApprovalPosture::Claude { .. }
            | ApprovalPosture::Codex { .. }
            | ApprovalPosture::Copilot { .. } => {}
        };
        let _: fn(Claude) = |mode| match mode {
            Claude::Default
            | Claude::AcceptEdits
            | Claude::DontAsk
            | Claude::BypassPermissions
            | Claude::Auto => {}
        };
        let modes = [
            Claude::Default,
            Claude::AcceptEdits,
            Claude::DontAsk,
            Claude::BypassPermissions,
            Claude::Auto,
        ];
        let _: fn(Policy) = |policy| match policy {
            Policy::Untrusted | Policy::OnRequest | Policy::Never => {}
        };
        let policies = [Policy::Untrusted, Policy::OnRequest, Policy::Never];
        let _: fn(Sandbox) = |sandbox| match sandbox {
            Sandbox::ReadOnly | Sandbox::WorkspaceWrite | Sandbox::DangerFullAccess => {}
        };
        let sandboxes = [
            Sandbox::ReadOnly,
            Sandbox::WorkspaceWrite,
            Sandbox::DangerFullAccess,
        ];
        let _: fn(Copilot) = |permissions| match permissions {
            Copilot::Ask | Copilot::AllowAll => {}
        };
        let permissions = [Copilot::Ask, Copilot::AllowAll];

        modes
            .into_iter()
            .map(claude)
            .chain(policies.into_iter().flat_map(|policy| {
                sandboxes
                    .into_iter()
                    .map(move |sandbox| codex(policy, sandbox))
            }))
            .chain(permissions.into_iter().map(copilot))
            .collect()
    }

    /// ADR 0036's second table, as written there: each native value and the
    /// level it sits at.
    fn adr_levels() -> Vec<(ApprovalPosture, PostureLevel)> {
        use ClaudePermissionMode as Claude;
        use CodexApprovalPolicy as Policy;
        use CodexSandboxMode as Sandbox;
        use CopilotPermissions as Copilot;
        vec![
            (claude(Claude::Default), AsksForConsent),
            (claude(Claude::AcceptEdits), RunsRoutineWork),
            (claude(Claude::Auto), RunsRoutineWork),
            (claude(Claude::DontAsk), RunsContained),
            (claude(Claude::BypassPermissions), RunsUnrestricted),
            (codex(Policy::Untrusted, Sandbox::ReadOnly), AsksForConsent),
            (
                codex(Policy::Untrusted, Sandbox::WorkspaceWrite),
                AsksForConsent,
            ),
            (
                codex(Policy::Untrusted, Sandbox::DangerFullAccess),
                AsksForConsent,
            ),
            (codex(Policy::OnRequest, Sandbox::ReadOnly), AsksForConsent),
            (
                codex(Policy::OnRequest, Sandbox::WorkspaceWrite),
                RunsRoutineWork,
            ),
            (
                codex(Policy::OnRequest, Sandbox::DangerFullAccess),
                RunsRoutineWork,
            ),
            (codex(Policy::Never, Sandbox::ReadOnly), RunsContained),
            (codex(Policy::Never, Sandbox::WorkspaceWrite), RunsContained),
            (
                codex(Policy::Never, Sandbox::DangerFullAccess),
                RunsUnrestricted,
            ),
            (copilot(Copilot::Ask), AsksForConsent),
            (copilot(Copilot::AllowAll), RunsUnrestricted),
        ]
    }

    /// ADR 0036's third table, as written there: each level's one value on
    /// each Provider.
    fn adr_canonical(level: PostureLevel, provider: &str) -> ApprovalPosture {
        use ClaudePermissionMode as Claude;
        use CodexApprovalPolicy as Policy;
        use CodexSandboxMode as Sandbox;
        use CopilotPermissions as Copilot;
        match (level, provider) {
            (AsksForConsent, "claude") => claude(Claude::Default),
            (RunsRoutineWork, "claude") => claude(Claude::AcceptEdits),
            (RunsContained, "claude") => claude(Claude::DontAsk),
            (RunsUnrestricted, "claude") => claude(Claude::BypassPermissions),
            (AsksForConsent, "codex") => codex(Policy::Untrusted, Sandbox::WorkspaceWrite),
            (RunsRoutineWork, "codex") => codex(Policy::OnRequest, Sandbox::WorkspaceWrite),
            (RunsContained, "codex") => codex(Policy::Never, Sandbox::WorkspaceWrite),
            (RunsUnrestricted, "codex") => codex(Policy::Never, Sandbox::DangerFullAccess),
            (AsksForConsent | RunsRoutineWork | RunsContained, "copilot") => copilot(Copilot::Ask),
            (RunsUnrestricted, "copilot") => copilot(Copilot::AllowAll),
            _ => unreachable!("ADR 0036 has no column for {provider}"),
        }
    }

    #[test]
    fn every_native_value_sits_at_the_level_adr_0036_gives_it() {
        let adr = adr_levels();
        for value in every_native_value() {
            let placed = adr
                .iter()
                .filter(|(listed, _)| *listed == value)
                .map(|(_, level)| *level)
                .collect::<Vec<_>>();
            assert_eq!(
                placed.len(),
                1,
                "ADR 0036 places {value:?} exactly once: {placed:?}"
            );
            assert_eq!(PostureLevel::of(value), placed[0], "{value:?}");
        }
        assert_eq!(
            adr.len(),
            every_native_value().len(),
            "and places nothing that is not a native value"
        );
    }

    #[test]
    fn every_level_is_one_value_of_each_provider() {
        for level in LEVELS {
            for provider in PROVIDERS {
                let value = level
                    .canonical(&ProviderId::new(provider))
                    .unwrap_or_else(|| panic!("{level:?} has a value on {provider}"));
                assert_eq!(
                    value,
                    adr_canonical(level, provider),
                    "{level:?} on {provider}"
                );
                assert_eq!(value.provider(), ProviderId::new(provider));
            }
        }
    }

    #[test]
    fn every_native_value_has_an_equivalent_on_every_provider() {
        for value in every_native_value() {
            for provider in PROVIDERS {
                assert!(
                    PostureLevel::of(value)
                        .canonical(&ProviderId::new(provider))
                        .is_some(),
                    "{value:?} reads as some value of {provider}"
                );
            }
        }
    }

    #[test]
    fn canonical_values_are_fixed_points() {
        for level in LEVELS {
            for provider in PROVIDERS {
                let provider = ProviderId::new(provider);
                let value = level.canonical(&provider).unwrap();
                assert_eq!(
                    PostureLevel::of(value).canonical(&provider),
                    Some(value),
                    "{level:?} on {provider} reads back as itself"
                );
            }
        }
    }

    /// Where a Provider has no twin for a value, the one it gets asks more,
    /// never less: an equivalent never sits above the value it was read from.
    #[test]
    fn an_equivalent_never_allows_more_than_the_value_it_was_read_from() {
        for value in every_native_value() {
            for provider in PROVIDERS {
                let equivalent = PostureLevel::of(value)
                    .canonical(&ProviderId::new(provider))
                    .unwrap();
                assert!(
                    PostureLevel::of(equivalent) <= PostureLevel::of(value),
                    "{value:?} reads on {provider} as {equivalent:?}"
                );
            }
        }
    }

    #[test]
    fn a_provider_the_table_has_no_column_for_has_no_value_at_any_level() {
        for level in LEVELS {
            assert_eq!(level.canonical(&ProviderId::new("gemini")), None);
        }
    }
}
