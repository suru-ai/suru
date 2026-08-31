//! View state for pairing with and browsing Remotes.

use std::collections::HashMap;

use crate::protocol::{
    InvitePreview, Outlook, RedeemInviteRequest, Remote, RemoteHealth, RemoteStatus,
};

#[derive(Clone, Debug, Default)]
pub(super) struct ConnectOverlay {
    state: ConnectOverlayState,
    known_names: Vec<String>,
    known_remotes: Vec<Remote>,
    remote_statuses: HashMap<String, RemoteProbeStatus>,
}

#[derive(Clone, Debug, Default)]
enum ConnectOverlayState {
    #[default]
    Closed,
    Loading,
    InviteEntry {
        invite: String,
        error: Option<String>,
    },
    Inspecting,
    Confirming {
        invite: String,
        preview: InvitePreview,
    },
    Details(ConnectDraft),
    Redeeming(ConnectDraft),
    RemotePicker {
        selected: usize,
    },
}

#[derive(Clone, Debug)]
enum RemoteProbeStatus {
    Checking,
    Available,
    ProtocolMismatch(Option<u32>),
    Unavailable(String),
    Revoked,
}

impl RemoteProbeStatus {
    fn remembered(status: RemoteStatus) -> Self {
        match status {
            RemoteStatus::Available => Self::Available,
            RemoteStatus::Unavailable => Self::Unavailable("could not reach Remote".to_owned()),
            RemoteStatus::Revoked => Self::Revoked,
            RemoteStatus::ProtocolMismatch => Self::ProtocolMismatch(None),
        }
    }

    fn probed(health: RemoteHealth) -> Self {
        match health.status {
            RemoteStatus::ProtocolMismatch => Self::ProtocolMismatch(health.protocol_version),
            status => Self::remembered(status),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectFocus {
    Name,
    Addresses,
}

#[derive(Clone, Debug)]
struct ConnectDraft {
    invite: String,
    name: String,
    addresses: Vec<std::net::SocketAddr>,
    selected: usize,
    focus: ConnectFocus,
    error: Option<String>,
}

pub(super) struct ConnectDetails<'a> {
    pub(super) name: &'a str,
    pub(super) addresses: &'a [std::net::SocketAddr],
    pub(super) selected: usize,
    pub(super) name_focused: bool,
    pub(super) error: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConnectInputMode {
    Waiting,
    Invite,
    Confirm,
    Name,
    Addresses,
    Picker,
}

impl ConnectOverlay {
    pub(super) fn open(&mut self) {
        self.state = ConnectOverlayState::Loading;
    }

    pub(super) fn close(&mut self) {
        self.state = ConnectOverlayState::Closed;
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self.state, ConnectOverlayState::Closed)
    }

    pub(super) fn loading_label(&self) -> Option<&'static str> {
        match self.state {
            ConnectOverlayState::Loading => Some("Loading Remotes…"),
            ConnectOverlayState::Inspecting => Some("Inspecting Invite…"),
            ConnectOverlayState::Redeeming(_) => Some("Pairing Remote…"),
            _ => None,
        }
    }

    pub(super) fn load_remotes(&mut self, remotes: Vec<Remote>, outlook: &Outlook) {
        self.known_names = remotes.iter().map(|remote| remote.name.clone()).collect();
        self.remote_statuses = remotes
            .iter()
            .map(|remote| {
                let status = match remote.status {
                    RemoteStatus::Available => RemoteProbeStatus::Checking,
                    status => RemoteProbeStatus::remembered(status),
                };
                (remote.name.clone(), status)
            })
            .collect();
        self.known_remotes = remotes;
        self.state = if self.known_remotes.is_empty() && *outlook == Outlook::Local {
            ConnectOverlayState::InviteEntry {
                invite: String::new(),
                error: None,
            }
        } else {
            ConnectOverlayState::RemotePicker {
                selected: outlook
                    .remote_name()
                    .and_then(|name| {
                        self.known_remotes
                            .iter()
                            .position(|remote| remote.name == name)
                    })
                    .map_or(0, |index| index + 1),
            }
        };
    }

    pub(super) fn input_mode(&self) -> ConnectInputMode {
        match &self.state {
            ConnectOverlayState::InviteEntry { .. } => ConnectInputMode::Invite,
            ConnectOverlayState::Confirming { .. } => ConnectInputMode::Confirm,
            ConnectOverlayState::Details(draft) if draft.focus == ConnectFocus::Name => {
                ConnectInputMode::Name
            }
            ConnectOverlayState::Details(_) => ConnectInputMode::Addresses,
            ConnectOverlayState::RemotePicker { .. } => ConnectInputMode::Picker,
            ConnectOverlayState::Closed
            | ConnectOverlayState::Loading
            | ConnectOverlayState::Inspecting
            | ConnectOverlayState::Redeeming(_) => ConnectInputMode::Waiting,
        }
    }

    pub(super) fn pair_another(&mut self) {
        if matches!(self.state, ConnectOverlayState::RemotePicker { .. }) {
            self.state = ConnectOverlayState::InviteEntry {
                invite: String::new(),
                error: None,
            };
        }
    }

    pub(super) fn fail_invite_entry(&mut self, message: String) {
        self.state = ConnectOverlayState::InviteEntry {
            invite: String::new(),
            error: Some(message),
        };
    }

    pub(super) fn insert(&mut self, text: &str) {
        match &mut self.state {
            ConnectOverlayState::InviteEntry { invite, error } => {
                invite.push_str(text.trim());
                *error = None;
            }
            ConnectOverlayState::Details(draft) if draft.focus == ConnectFocus::Name => {
                draft
                    .name
                    .extend(text.chars().filter(|character| !character.is_control()));
                draft.error = None;
            }
            _ => {}
        }
    }

    pub(super) fn delete_backward(&mut self) {
        match &mut self.state {
            ConnectOverlayState::InviteEntry { invite, error } => {
                invite.pop();
                *error = None;
            }
            ConnectOverlayState::Details(draft) if draft.focus == ConnectFocus::Name => {
                draft.name.pop();
                draft.error = None;
            }
            _ => {}
        }
    }

    pub(super) fn begin_preview(&mut self) -> Option<String> {
        let ConnectOverlayState::InviteEntry { invite, error } = &mut self.state else {
            return None;
        };
        let invite = invite.trim().to_owned();
        if invite.is_empty() {
            *error = Some("Paste an Invite first".to_owned());
            return None;
        }
        self.state = ConnectOverlayState::Inspecting;
        Some(invite)
    }

    pub(super) fn show_preview(&mut self, invite: String, preview: InvitePreview) {
        self.state = ConnectOverlayState::Confirming { invite, preview };
    }

    pub(super) fn fail_preview(&mut self, invite: String, error: String) {
        self.state = ConnectOverlayState::InviteEntry {
            invite,
            error: Some(error),
        };
    }

    pub(super) fn confirm(&mut self) -> bool {
        let ConnectOverlayState::Confirming { invite, preview } = &self.state else {
            return false;
        };
        self.state = ConnectOverlayState::Details(ConnectDraft {
            invite: invite.clone(),
            name: preview.hostname.clone(),
            addresses: preview.addresses.clone(),
            selected: 0,
            focus: ConnectFocus::Name,
            error: None,
        });
        true
    }

    pub(super) fn focus_next(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state {
            draft.focus = match draft.focus {
                ConnectFocus::Name => ConnectFocus::Addresses,
                ConnectFocus::Addresses => ConnectFocus::Name,
            };
        }
    }

    pub(super) fn select_previous(&mut self) {
        match &mut self.state {
            ConnectOverlayState::Details(draft)
                if draft.focus == ConnectFocus::Addresses && !draft.addresses.is_empty() =>
            {
                draft.selected = draft
                    .selected
                    .checked_sub(1)
                    .unwrap_or(draft.addresses.len() - 1);
            }
            ConnectOverlayState::RemotePicker { selected } => {
                *selected = selected.checked_sub(1).unwrap_or(self.known_remotes.len());
            }
            _ => {}
        }
    }

    pub(super) fn select_next(&mut self) {
        match &mut self.state {
            ConnectOverlayState::Details(draft)
                if draft.focus == ConnectFocus::Addresses && !draft.addresses.is_empty() =>
            {
                draft.selected = (draft.selected + 1) % draft.addresses.len();
            }
            ConnectOverlayState::RemotePicker { selected } => {
                *selected = (*selected + 1) % (self.known_remotes.len() + 1);
            }
            _ => {}
        }
    }

    pub(super) fn move_address_up(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
            && draft.focus == ConnectFocus::Addresses
            && draft.selected > 0
        {
            draft.addresses.swap(draft.selected, draft.selected - 1);
            draft.selected -= 1;
        }
    }

    pub(super) fn move_address_down(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
            && draft.focus == ConnectFocus::Addresses
            && draft.selected + 1 < draft.addresses.len()
        {
            draft.addresses.swap(draft.selected, draft.selected + 1);
            draft.selected += 1;
        }
    }

    pub(super) fn begin_redemption(&mut self) -> Option<RedeemInviteRequest> {
        let ConnectOverlayState::Details(draft) = &mut self.state else {
            return None;
        };
        let name = draft.name.trim().to_owned();
        if name.is_empty() {
            draft.error = Some("Remote name cannot be empty".to_owned());
            return None;
        }
        if self.known_names.iter().any(|known| known == &name) {
            draft.error = Some(format!("A Remote named `{name}` already exists"));
            return None;
        }
        let request = RedeemInviteRequest {
            invite: draft.invite.clone(),
            name: Some(name),
            addresses: draft.addresses.clone(),
        };
        self.state = ConnectOverlayState::Redeeming(draft.clone());
        Some(request)
    }

    pub(super) fn redemption_failed(&mut self, error: String) {
        let ConnectOverlayState::Redeeming(mut draft) = std::mem::take(&mut self.state) else {
            return;
        };
        draft.error = Some(error);
        self.state = ConnectOverlayState::Details(draft);
    }

    pub(super) fn remote_redeemed(&mut self, remote: Remote) {
        self.known_names.push(remote.name.clone());
        self.remote_statuses
            .insert(remote.name.clone(), RemoteProbeStatus::Available);
        self.known_remotes.push(remote);
        self.state = ConnectOverlayState::RemotePicker {
            selected: self.known_remotes.len(),
        };
    }

    pub(super) fn remote_probed(&mut self, name: &str, result: Result<RemoteHealth, String>) {
        let Some(status) = self.remote_statuses.get_mut(name) else {
            return;
        };
        *status = result.map_or_else(RemoteProbeStatus::Unavailable, RemoteProbeStatus::probed);
    }

    pub(super) fn remote_failed(&mut self, name: &str, status: RemoteStatus) {
        self.remote_statuses
            .insert(name.to_owned(), RemoteProbeStatus::remembered(status));
    }

    pub(super) fn invite_entry(&self) -> Option<(&str, Option<&str>)> {
        match &self.state {
            ConnectOverlayState::InviteEntry { invite, error } => Some((invite, error.as_deref())),
            _ => None,
        }
    }

    pub(super) fn confirmation(&self) -> Option<&InvitePreview> {
        match &self.state {
            ConnectOverlayState::Confirming { preview, .. } => Some(preview),
            _ => None,
        }
    }

    pub(super) fn details(&self) -> Option<ConnectDetails<'_>> {
        match &self.state {
            ConnectOverlayState::Details(draft) => Some(ConnectDetails {
                name: &draft.name,
                addresses: &draft.addresses,
                selected: draft.selected,
                name_focused: draft.focus == ConnectFocus::Name,
                error: draft.error.as_deref(),
            }),
            _ => None,
        }
    }

    pub(super) fn remotes(&self) -> &[Remote] {
        match &self.state {
            ConnectOverlayState::RemotePicker { .. } => &self.known_remotes,
            _ => &[],
        }
    }

    pub(super) fn status_label(&self, name: &str) -> String {
        let ConnectOverlayState::RemotePicker { .. } = &self.state else {
            return String::new();
        };
        match self.remote_statuses.get(name) {
            Some(RemoteProbeStatus::Checking) | None => "Checking…".to_owned(),
            Some(RemoteProbeStatus::Available) => "Available".to_owned(),
            Some(RemoteProbeStatus::ProtocolMismatch(Some(version))) => {
                format!("Protocol v{version} mismatch")
            }
            Some(RemoteProbeStatus::ProtocolMismatch(None)) => "Protocol mismatch".to_owned(),
            Some(RemoteProbeStatus::Unavailable(error)) => format!("Unavailable · {error}"),
            Some(RemoteProbeStatus::Revoked) => "Revoked".to_owned(),
        }
    }

    pub(super) fn selected(&self) -> usize {
        match self.state {
            ConnectOverlayState::RemotePicker { selected, .. } => selected,
            _ => 0,
        }
    }

    pub(super) fn selected_outlook(&self) -> Option<Outlook> {
        let ConnectOverlayState::RemotePicker { selected } = self.state else {
            return None;
        };
        if selected == 0 {
            return Some(Outlook::Local);
        }
        let remote = self.known_remotes.get(selected - 1)?;
        matches!(
            self.remote_statuses.get(&remote.name),
            Some(RemoteProbeStatus::Available)
        )
        .then(|| Outlook::Remote(remote.name.clone()))
    }
}
