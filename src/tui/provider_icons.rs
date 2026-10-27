//! How a client names a Provider where it has room for a glyph: the Nerd Font
//! icon the Provider's runtime declares, ahead of its name where the reader
//! has icons on. Every surface presenting a Provider — the frame's own rows,
//! the Aside, the settings panel — names it the same way from here.

use crate::{protocol::ProviderId, provider::built_in_providers};

/// A Provider's Nerd Font glyph where Suru knows one. Unknown Provider IDs
/// keep their text-only presentation, which lets future Providers participate
/// in the shared surfaces before choosing an icon of their own.
pub(super) fn provider_icon(provider: &ProviderId) -> Option<char> {
    built_in_providers()
        .iter()
        .find(|candidate| &candidate.id == provider)
        .and_then(|provider| provider.nerd_font_icon)
}

/// `label` naming `provider`, led by the Provider's icon where `show_icons`
/// is on and the Provider has one.
pub(super) fn provider_label(show_icons: bool, provider: &ProviderId, label: &str) -> String {
    if show_icons && let Some(icon) = provider_icon(provider) {
        format!("{icon} {label}")
    } else {
        label.to_owned()
    }
}
