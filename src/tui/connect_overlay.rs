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
        /// Whether the selected Remote's removal has been asked for once and
        /// awaits the second press that performs it.
        armed: bool,
        note: Option<PickerNote>,
    },
    Removing {
        selected: usize,
    },
}

/// The one line a finished removal leaves on the picker, standing until the
/// next key the reader presses.
#[derive(Clone, Debug)]
struct PickerNote {
    text: String,
    failed: bool,
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

/// The name field and the address rows are one column the arrows walk, so a
/// reader who never reaches for Tab still arrives at the addresses, and leaves
/// them by walking off either end. An Invite carrying no address to order
/// leaves the keys in the name field, where the only thing left to say is the
/// Remote's name.
impl ConnectDraft {
    fn select_previous(&mut self) {
        let Some(last) = self.addresses.len().checked_sub(1) else {
            return;
        };
        match self.focus {
            ConnectFocus::Name => {
                self.focus = ConnectFocus::Addresses;
                self.selected = last;
            }
            ConnectFocus::Addresses if self.selected == 0 => self.focus = ConnectFocus::Name,
            ConnectFocus::Addresses => self.selected -= 1,
        }
    }

    fn select_next(&mut self) {
        if self.addresses.is_empty() {
            return;
        }
        match self.focus {
            ConnectFocus::Name => {
                self.focus = ConnectFocus::Addresses;
                self.selected = 0;
            }
            ConnectFocus::Addresses if self.selected + 1 >= self.addresses.len() => {
                self.focus = ConnectFocus::Name;
            }
            ConnectFocus::Addresses => self.selected += 1,
        }
    }
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

    /// Opens straight on Invite entry, as `/pair` asks. The Remote listing is
    /// still asked for behind it, so a duplicate name is refused and a
    /// successful redemption has a picker to land on.
    pub(super) fn open_invite_entry(&mut self) {
        self.state = ConnectOverlayState::InviteEntry {
            invite: String::new(),
            error: None,
        };
    }

    pub(super) fn close(&mut self) {
        self.state = ConnectOverlayState::Closed;
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self.state, ConnectOverlayState::Closed)
    }

    pub(super) fn loading_label(&self) -> Option<&'static str> {
        match &self.state {
            ConnectOverlayState::Loading => Some("Loading Remotes…"),
            ConnectOverlayState::Inspecting => Some("Inspecting Invite…"),
            ConnectOverlayState::Redeeming(_) => Some("Pairing Remote…"),
            ConnectOverlayState::Removing { .. } => Some("Removing Remote…"),
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
        // A listing that lands behind Invite entry is only what that screen
        // needs to refuse a duplicate name; the reader stays where they are.
        if !matches!(self.state, ConnectOverlayState::Loading) {
            return;
        }
        self.state = ConnectOverlayState::RemotePicker {
            selected: outlook
                .remote_name()
                .and_then(|name| {
                    self.known_remotes
                        .iter()
                        .position(|remote| remote.name == name)
                })
                .map_or(0, |index| index + 1),
            armed: false,
            note: None,
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
            | ConnectOverlayState::Redeeming(_)
            | ConnectOverlayState::Removing { .. } => ConnectInputMode::Waiting,
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
            ConnectOverlayState::Details(draft) => draft.select_previous(),
            ConnectOverlayState::RemotePicker { selected, .. } => {
                *selected = selected.checked_sub(1).unwrap_or(self.known_remotes.len());
            }
            _ => {}
        }
    }

    pub(super) fn select_next(&mut self) {
        match &mut self.state {
            ConnectOverlayState::Details(draft) => draft.select_next(),
            ConnectOverlayState::RemotePicker { selected, .. } => {
                *selected = (*selected + 1) % (self.known_remotes.len() + 1);
            }
            _ => {}
        }
    }

    /// Reordering acts on the marked address from either field. The cursor is
    /// drawn whether or not the addresses hold the keys, so a reader who reads
    /// the priority off the screen and reaches straight for Shift+↑↓ moves the
    /// row they are looking at rather than pressing a dead key.
    pub(super) fn move_address_up(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
            && draft.selected > 0
        {
            draft.addresses.swap(draft.selected, draft.selected - 1);
            draft.selected -= 1;
        }
    }

    pub(super) fn move_address_down(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
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
            armed: false,
            note: None,
        };
    }

    /// Ending a Pairing is asked for twice: the first press arms the selected
    /// Remote's removal and only the second performs it. Local is the one row
    /// no press removes.
    pub(super) fn remove_remote(&mut self) -> Option<String> {
        let ConnectOverlayState::RemotePicker {
            selected, armed, ..
        } = &self.state
        else {
            return None;
        };
        let (selected, armed) = (*selected, *armed);
        let name = selected
            .checked_sub(1)
            .and_then(|index| self.known_remotes.get(index))
            .map(|remote| remote.name.clone());
        let Some(name) = name else {
            self.disarm_removal();
            return None;
        };
        if !armed {
            self.state = ConnectOverlayState::RemotePicker {
                selected,
                armed: true,
                note: None,
            };
            return None;
        }
        self.state = ConnectOverlayState::Removing { selected };
        Some(name)
    }

    /// Puts down an armed removal and clears the note a finished one left,
    /// which every key but the one that arms removal does.
    pub(super) fn disarm_removal(&mut self) {
        if let ConnectOverlayState::RemotePicker { armed, note, .. } = &mut self.state {
            *armed = false;
            *note = None;
        }
    }

    pub(super) fn remote_removed(&mut self, name: &str, acknowledged: bool) {
        self.known_names.retain(|known| known != name);
        self.known_remotes.retain(|remote| remote.name != name);
        self.remote_statuses.remove(name);
        self.settle_removal(PickerNote {
            text: if acknowledged {
                format!("Removed {name}")
            } else {
                format!("Removed {name} here; it did not answer")
            },
            failed: false,
        });
    }

    pub(super) fn removal_failed(&mut self, error: String) {
        self.settle_removal(PickerNote {
            text: error,
            failed: true,
        });
    }

    fn settle_removal(&mut self, note: PickerNote) {
        let selected = match &self.state {
            ConnectOverlayState::Removing { selected, .. } => *selected,
            _ => return,
        };
        self.state = ConnectOverlayState::RemotePicker {
            selected: selected.min(self.known_remotes.len()),
            armed: false,
            note: Some(note),
        };
    }

    pub(super) fn removal_armed(&self) -> bool {
        matches!(
            self.state,
            ConnectOverlayState::RemotePicker { armed: true, .. }
        )
    }

    /// The finished removal's one line, and whether it failed.
    pub(super) fn picker_note(&self) -> Option<(&str, bool)> {
        match &self.state {
            ConnectOverlayState::RemotePicker {
                note: Some(note), ..
            } => Some((note.text.as_str(), note.failed)),
            _ => None,
        }
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
        let ConnectOverlayState::RemotePicker { selected, .. } = self.state else {
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
