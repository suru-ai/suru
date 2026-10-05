//! View state for pairing with and browsing Remotes.
//!
//! A redemption names itself with a [`RedemptionRequest`], and only the
//! answer to the one awaited is taken. One the Server refuses for want of a
//! login at a Relay waits on that login instead — begun there, or shown again
//! where one is under way — and once it is done carries on with the same
//! redemption, asked afresh; one that ends without a Login stops on that
//! step, saying why, and can be tried again.

use std::collections::HashMap;

use crate::protocol::{
    InvitePreview, Outlook, RedeemInviteRequest, RelayLogin, RelayLoginOutcome, Remote,
    RemoteHealth, RemoteStatus, UnreachableReason, Way,
};

use super::{list_window::ListWindow, relay_overlay::RelayRequest, unreachable_reason};

/// One redemption the Connect overlay asked the Client's own Server for,
/// told apart from every other so its answer reaches only what asked for it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RedemptionRequest(u64);

#[derive(Clone, Debug, Default)]
pub(super) struct ConnectOverlay {
    state: ConnectOverlayState,
    known_names: Vec<String>,
    known_remotes: Vec<Remote>,
    remote_statuses: HashMap<String, RemoteProbeStatus>,
    /// The window over the paired Remotes the picker offers.
    remotes_window: ListWindow,
    /// The window over the ways of reaching the Remote being configured.
    ways_window: ListWindow,
    last_request: u64,
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
    Redeeming {
        draft: ConnectDraft,
        request: RedemptionRequest,
    },
    /// The redemption of `draft` waits on a login at the Relay at `relay`,
    /// which the Server refused it for want of, saying so as `refusal`.
    LoggingIn {
        draft: ConnectDraft,
        relay: String,
        refusal: String,
        step: LoginStep,
    },
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
    /// Unavailable for a reason its user can do something about.
    Unreachable(UnreachableReason),
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
        match (health.status, health.unreachable) {
            (RemoteStatus::ProtocolMismatch, _) => Self::ProtocolMismatch(health.protocol_version),
            (RemoteStatus::Unavailable, Some(reason)) => Self::Unreachable(reason),
            (status, _) => Self::remembered(status),
        }
    }
}

/// Where the login a redemption waits on stands.
#[derive(Clone, Debug)]
pub(super) enum LoginStep {
    /// Adding the Relay, which the Server holds no entry for, under this
    /// request, to log in there once it is added.
    Adding(RelayRequest),
    /// Beginning a login there under this request.
    Beginning(RelayRequest),
    /// Waiting for the reader to finish `login` where it says.
    Waiting(RelayLogin),
    /// The login ended, or could not be had, without a Login: why.
    Stopped(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectFocus {
    Name,
    Ways,
}

#[derive(Clone, Debug)]
struct ConnectDraft {
    invite: String,
    name: String,
    ways: Vec<Way>,
    selected: usize,
    focus: ConnectFocus,
    error: Option<String>,
}

/// The name field and the rows of ways are one column the arrows walk, so a
/// reader who never reaches for Tab still arrives at the ways, and leaves them
/// by walking off either end. An Invite carrying no way to order leaves the
/// keys in the name field, where the only thing left to say is the Remote's
/// name.
impl ConnectDraft {
    fn select_previous(&mut self) {
        let Some(last) = self.ways.len().checked_sub(1) else {
            return;
        };
        match self.focus {
            ConnectFocus::Name => {
                self.focus = ConnectFocus::Ways;
                self.selected = last;
            }
            ConnectFocus::Ways if self.selected == 0 => self.focus = ConnectFocus::Name,
            ConnectFocus::Ways => self.selected -= 1,
        }
    }

    fn select_next(&mut self) {
        if self.ways.is_empty() {
            return;
        }
        match self.focus {
            ConnectFocus::Name => {
                self.focus = ConnectFocus::Ways;
                self.selected = 0;
            }
            ConnectFocus::Ways if self.selected + 1 >= self.ways.len() => {
                self.focus = ConnectFocus::Name;
            }
            ConnectFocus::Ways => self.selected += 1,
        }
    }
}

pub(super) struct ConnectDetails<'a> {
    pub(super) name: &'a str,
    pub(super) ways: &'a [Way],
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
    Ways,
    Picker,
    /// The login a redemption waits on, shown or stopped.
    Login,
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
            ConnectOverlayState::Redeeming { .. } => Some("Pairing Remote…"),
            ConnectOverlayState::Removing { .. } => Some("Removing Remote…"),
            ConnectOverlayState::LoggingIn {
                step: LoginStep::Adding(_),
                ..
            } => Some("Adding Relay…"),
            ConnectOverlayState::LoggingIn {
                step: LoginStep::Beginning(_),
                ..
            } => Some("Beginning login…"),
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
        self.remotes_window.open();
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
            ConnectOverlayState::Details(_) => ConnectInputMode::Ways,
            ConnectOverlayState::RemotePicker { .. } => ConnectInputMode::Picker,
            ConnectOverlayState::LoggingIn {
                step: LoginStep::Waiting(_) | LoginStep::Stopped(_),
                ..
            } => ConnectInputMode::Login,
            ConnectOverlayState::Closed
            | ConnectOverlayState::Loading
            | ConnectOverlayState::Inspecting
            | ConnectOverlayState::Redeeming { .. }
            | ConnectOverlayState::LoggingIn { .. }
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
            ways: preview.ways.clone(),
            selected: 0,
            focus: ConnectFocus::Name,
            error: None,
        });
        self.ways_window.open();
        true
    }

    pub(super) fn focus_next(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state {
            draft.focus = match draft.focus {
                ConnectFocus::Name => ConnectFocus::Ways,
                ConnectFocus::Ways => ConnectFocus::Name,
            };
        }
    }

    pub(super) fn select_previous(&mut self) {
        match &mut self.state {
            ConnectOverlayState::Details(draft) => {
                draft.select_previous();
                self.ways_window.reveal();
            }
            ConnectOverlayState::RemotePicker { selected, .. } => {
                *selected = selected.checked_sub(1).unwrap_or(self.known_remotes.len());
                self.remotes_window.reveal();
            }
            _ => {}
        }
    }

    pub(super) fn select_next(&mut self) {
        match &mut self.state {
            ConnectOverlayState::Details(draft) => {
                draft.select_next();
                self.ways_window.reveal();
            }
            ConnectOverlayState::RemotePicker { selected, .. } => {
                *selected = (*selected + 1) % (self.known_remotes.len() + 1);
                self.remotes_window.reveal();
            }
            _ => {}
        }
    }

    /// Reordering acts on the marked way from either field. The cursor is
    /// drawn whether or not the ways hold the keys, so a reader who reads the
    /// priority off the screen and reaches straight for Shift+↑↓ moves the row
    /// they are looking at rather than pressing a dead key.
    pub(super) fn move_way_up(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
            && draft.selected > 0
        {
            draft.ways.swap(draft.selected, draft.selected - 1);
            draft.selected -= 1;
            self.ways_window.reveal();
        }
    }

    pub(super) fn move_way_down(&mut self) {
        if let ConnectOverlayState::Details(draft) = &mut self.state
            && draft.selected + 1 < draft.ways.len()
        {
            draft.ways.swap(draft.selected, draft.selected + 1);
            draft.selected += 1;
            self.ways_window.reveal();
        }
    }

    pub(super) fn begin_redemption(&mut self) -> Option<(RedemptionRequest, RedeemInviteRequest)> {
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
        let draft = draft.clone();
        Some(self.redeem(draft))
    }

    /// Asks afresh for the redemption a login stopped, answering the request
    /// that asks: the Server says again what it needs, if anything.
    pub(super) fn redeem_again(&mut self) -> Option<(RedemptionRequest, RedeemInviteRequest)> {
        if !matches!(
            self.state,
            ConnectOverlayState::LoggingIn {
                step: LoginStep::Stopped(_),
                ..
            }
        ) {
            return None;
        }
        self.carry_on()
    }

    /// Redeems the Invite as `draft` says, under a request of its own.
    fn redeem(&mut self, draft: ConnectDraft) -> (RedemptionRequest, RedeemInviteRequest) {
        self.last_request += 1;
        let request = RedemptionRequest(self.last_request);
        let redemption = RedeemInviteRequest {
            invite: draft.invite.clone(),
            name: Some(draft.name.trim().to_owned()),
            ways: draft.ways.clone(),
        };
        self.state = ConnectOverlayState::Redeeming { draft, request };
        (request, redemption)
    }

    /// Whether the redemption `request` asked for is the one awaited.
    pub(super) fn awaits_redemption(&self, request: RedemptionRequest) -> bool {
        matches!(self.state, ConnectOverlayState::Redeeming { request: awaited, .. } if awaited == request)
    }

    pub(super) fn redemption_failed(&mut self, request: RedemptionRequest, error: String) {
        if !self.awaits_redemption(request) {
            return;
        }
        let ConnectOverlayState::Redeeming { mut draft, .. } = std::mem::take(&mut self.state)
        else {
            return;
        };
        draft.error = Some(error);
        self.state = ConnectOverlayState::Details(draft);
    }

    /// Has the redemption awaited, refused as `refusal` for want of a login at
    /// the Relay at `relay`, wait on that login, which stands as `step`.
    pub(super) fn await_login(&mut self, relay: String, refusal: String, step: LoginStep) {
        let ConnectOverlayState::Redeeming { draft, .. } = std::mem::take(&mut self.state) else {
            return;
        };
        self.state = ConnectOverlayState::LoggingIn {
            draft,
            relay,
            refusal,
            step,
        };
    }

    /// The Relay whose addition `request` asked for, where a redemption waits
    /// on it.
    pub(super) fn awaited_addition(&self, request: RelayRequest) -> Option<&str> {
        match &self.state {
            ConnectOverlayState::LoggingIn {
                relay,
                step: LoginStep::Adding(awaited),
                ..
            } if *awaited == request => Some(relay),
            _ => None,
        }
    }

    /// The Relay a redemption waits on a login at was added: the login
    /// begins there under `request`.
    pub(super) fn begin_login(&mut self, request: RelayRequest) {
        if let ConnectOverlayState::LoggingIn { step, .. } = &mut self.state {
            *step = LoginStep::Beginning(request);
        }
    }

    pub(super) fn addition_failed(&mut self, request: RelayRequest, error: &str) {
        self.stop_where(
            |step| matches!(step, LoginStep::Adding(awaited) if *awaited == request),
            |relay| format!("Could not add the Relay at {relay}: {error}"),
        );
    }

    /// Takes the login the Server began for the beginning awaited, answering
    /// the Relay it is at, for it to be followed.
    pub(super) fn login_begun(
        &mut self,
        request: RelayRequest,
        login: &RelayLogin,
    ) -> Option<String> {
        let ConnectOverlayState::LoggingIn { relay, step, .. } = &mut self.state else {
            return None;
        };
        if !matches!(step, LoginStep::Beginning(awaited) if *awaited == request) {
            return None;
        }
        *step = LoginStep::Waiting(login.clone());
        Some(relay.clone())
    }

    pub(super) fn login_not_begun(&mut self, request: RelayRequest, error: &str) {
        self.stop_where(
            |step| matches!(step, LoginStep::Beginning(awaited) if *awaited == request),
            |relay| format!("Could not begin a login at {relay}: {error}"),
        );
    }

    /// Takes how a login at the Relay at `address` ended. Where it is the one
    /// the redemption waits on, a Login formed carries the redemption on —
    /// answering it, asked afresh — and any other end stops it there, saying
    /// why.
    pub(super) fn login_ended(
        &mut self,
        address: &str,
        login: &RelayLogin,
    ) -> Option<(RedemptionRequest, RedeemInviteRequest)> {
        if !self
            .awaited_login()
            .is_some_and(|(relay, shown)| relay == address && shown.user_code == login.user_code)
        {
            return None;
        }
        match &login.outcome {
            RelayLoginOutcome::Pending => None,
            RelayLoginOutcome::Done { .. } => self.carry_on(),
            outcome => {
                let why = super::relay_overlay::login_refused(address, outcome)?;
                self.stop_where(|_| true, |_| why);
                None
            }
        }
    }

    /// The Server has come to hold a Login that stands at the Relay at
    /// `address`, however it came to: where the redemption waits on a login
    /// there, it carries on, answering the redemption asked afresh.
    pub(super) fn relay_logged_in(
        &mut self,
        address: &str,
    ) -> Option<(RedemptionRequest, RedeemInviteRequest)> {
        if !self
            .awaited_login()
            .is_some_and(|(relay, _)| relay == address)
        {
            return None;
        }
        self.carry_on()
    }

    /// Following the login the redemption waits on at the Relay at `address`
    /// ended before the login did: the redemption stops there, and trying
    /// again follows it afresh.
    pub(super) fn login_lost(&mut self, address: &str, error: &str) {
        if self
            .awaited_login()
            .is_some_and(|(relay, _)| relay == address)
        {
            self.stop_where(
                |_| true,
                |relay| format!("Stopped following the login at {relay}: {error}"),
            );
        }
    }

    /// The Relay a redemption waits on its reader to log in at, and the login
    /// on display there.
    pub(super) fn awaited_login(&self) -> Option<(&str, &RelayLogin)> {
        match &self.state {
            ConnectOverlayState::LoggingIn {
                relay,
                step: LoginStep::Waiting(login),
                ..
            } => Some((relay, login)),
            _ => None,
        }
    }

    /// The address the login a redemption waits on has its reader visit.
    pub(super) fn copy_login_address(&self) -> Option<String> {
        self.awaited_login()
            .map(|(_, login)| login.verification_uri.clone())
    }

    /// The code the login a redemption waits on has its reader enter.
    pub(super) fn copy_login_code(&self) -> Option<String> {
        self.awaited_login()
            .map(|(_, login)| login.user_code.clone())
    }

    /// The Relay a redemption waits on a login at, and where that login
    /// stands.
    pub(super) fn login_step(&self) -> Option<(&str, &LoginStep)> {
        match &self.state {
            ConnectOverlayState::LoggingIn { relay, step, .. } => Some((relay, step)),
            _ => None,
        }
    }

    /// Redeems afresh the Invite whose redemption waited on a login.
    fn carry_on(&mut self) -> Option<(RedemptionRequest, RedeemInviteRequest)> {
        let ConnectOverlayState::LoggingIn { draft, .. } = std::mem::take(&mut self.state) else {
            return None;
        };
        Some(self.redeem(draft))
    }

    /// Stops the login a redemption waits on where its step is as `stopping`
    /// says, saying why as `why` has it of the Relay.
    fn stop_where(
        &mut self,
        stopping: impl FnOnce(&LoginStep) -> bool,
        why: impl FnOnce(&str) -> String,
    ) {
        if let ConnectOverlayState::LoggingIn { relay, step, .. } = &mut self.state
            && stopping(step)
        {
            *step = LoginStep::Stopped(why(relay));
        }
    }

    /// Steps back from the login a redemption waits on to the Remote's
    /// details, which say why it was refused — the login goes on at the
    /// Server all the same — and closes the overlay from anywhere else.
    pub(super) fn back(&mut self) {
        self.state = match std::mem::take(&mut self.state) {
            ConnectOverlayState::LoggingIn {
                mut draft, refusal, ..
            } => {
                draft.error = Some(refusal);
                ConnectOverlayState::Details(draft)
            }
            _ => ConnectOverlayState::Closed,
        };
    }

    pub(super) fn remote_redeemed(&mut self, request: RedemptionRequest, remote: Remote) {
        if !self.awaits_redemption(request) {
            return;
        }
        self.known_names.push(remote.name.clone());
        self.remote_statuses
            .insert(remote.name.clone(), RemoteProbeStatus::Available);
        self.known_remotes.push(remote);
        self.remotes_window.open();
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
    pub(super) fn disarm_removal(&mut self) -> bool {
        match &mut self.state {
            ConnectOverlayState::RemotePicker { armed, note, .. } => {
                std::mem::take(armed) | note.take().is_some()
            }
            _ => false,
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
                ways: &draft.ways,
                selected: draft.selected,
                name_focused: draft.focus == ConnectFocus::Name,
                error: draft.error.as_deref(),
            }),
            _ => None,
        }
    }

    pub(super) fn remotes_window(&self) -> &ListWindow {
        &self.remotes_window
    }

    pub(super) fn ways_window(&self) -> &ListWindow {
        &self.ways_window
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
            Some(RemoteProbeStatus::Unreachable(reason)) => {
                format!("Unavailable · {}", unreachable_reason::brief(reason))
            }
            Some(RemoteProbeStatus::Revoked) => "Revoked".to_owned(),
        }
    }

    /// Why the selected Remote cannot be reached, in full, where its user can
    /// do something about it: more than its row has room to say.
    pub(super) fn selected_detail(&self) -> Option<String> {
        let ConnectOverlayState::RemotePicker { selected, .. } = self.state else {
            return None;
        };
        let remote = self.known_remotes.get(selected.checked_sub(1)?)?;
        match self.remote_statuses.get(&remote.name)? {
            RemoteProbeStatus::Unreachable(reason) => Some(unreachable_reason::in_full(reason)),
            _ => None,
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
