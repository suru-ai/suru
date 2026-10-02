//! A Context Breakdown read from Copilot's `session.metadata.getContextAttribution`.
//!
//! The attribution is told nothing of the Model's limits and answers the runtime default whatever
//! the Model (see `docs/validation/0299-copilot-context-fill.md`), so its own limits, free space
//! and buffer are never read. The window is instead the `tokenLimit` Copilot last reported for
//! the Session, which Context Fill measures against too, and the reserve stays unknown. Its
//! per-source entries are not read: how their kinds nest within the categories is undocumented.

use github_copilot_sdk::rpc::MetadataContextAttributionResultContextAttribution as Attribution;

use super::copilot_error;
use crate::{
    protocol::{ContextBreakdown, ContextFill, ContextPart, ContextSource},
    provider::ProviderError,
};

/// `attribution` measured against `window`, the `tokenLimit` the Session's Model is served at
/// where Copilot has reported one under the Selection in force.
pub(super) fn context_breakdown(
    attribution: &Attribution,
    window: Option<u64>,
) -> Result<ContextBreakdown, ProviderError> {
    let occupied_tokens = u64::try_from(attribution.total_tokens).map_err(|_| {
        copilot_error(format!(
            "Copilot reported a negative context size: {}",
            attribution.total_tokens
        ))
    })?;
    let categories = &attribution.categories;
    let parts = [
        (ContextSource::SystemPrompt, categories.system_prompt),
        (ContextSource::Instructions, categories.custom_instructions),
        (ContextSource::SystemTools, categories.system_tools),
        (ContextSource::McpTools, categories.mcp_tools),
        (ContextSource::Messages, categories.messages),
    ]
    .into_iter()
    .filter_map(|(source, tokens)| {
        Some(ContextPart {
            source,
            tokens: u64::try_from(tokens).ok()?,
            items: Vec::new(),
        })
    })
    .collect();
    Ok(ContextBreakdown {
        fill: ContextFill {
            occupied_tokens,
            capacity_tokens: window,
        },
        reserved_tokens: None,
        parts,
    })
}
