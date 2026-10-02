//! A Context Breakdown read from Copilot's `session.metadata.getContextAttribution`.
//!
//! The attribution is told nothing of the Model's limits and answers the runtime default whatever
//! the Model (see `docs/validation/0299-copilot-context-fill.md`), so its capacity, free space and
//! buffer are left unknown rather than measured against a window the Session does not have. Its
//! per-source entries are not read: how their kinds nest within the categories is undocumented.

use github_copilot_sdk::rpc::MetadataContextAttributionResultContextAttribution as Attribution;

use super::copilot_error;
use crate::{
    protocol::{ContextBreakdown, ContextFill, ContextPart, ContextSource},
    provider::ProviderError,
};

pub(super) fn context_breakdown(
    attribution: &Attribution,
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
            capacity_tokens: None,
        },
        reserved_tokens: None,
        parts,
    })
}
