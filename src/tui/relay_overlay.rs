//! View state for the list of the Client's own Server's Relays, where its
//! user adds one by address, logs in there, and removes one.
//!
//! A login is the Server's to carry out (ADR-0048): the Client shows where its
//! user goes and the code they enter there, and follows the login until the
//! Server reports it ended. The Server answers the latest login at each Relay
//! in its listing, so a Client opened after the one that began a login finds
//! it standing where it does and follows it in turn.

use std::collections::HashSet;

use crate::protocol::{Relay, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelayState};

use super::list_window::ListWindow;

#[derive(Clone, Debug, Default)]
pub(super) struct RelayOverlay {
    state: RelayOverlayState,
    relays: Vec<Relay>,
    /// Why the Server's Relays could not be listed, said in their place
    /// until a listing lands.
    listing_error: Option<String>,
    /// The Relay the keys are on, held while the reader steps away from the
    /// list to add one or to watch a login.
    selected: usize,
    /// The Relays whose latest login this Client follows, so a login is
    /// followed once however often the list is opened.
    following: HashSet<String>,
    window: ListWindow,
}

#[derive(Clone, Debug, Default)]
enum RelayOverlayState {
    #[default]
    Closed,
    Loading,
    Listing {
        /// Whether the selected Relay's removal has been asked for once and
        /// awaits the second press that performs it.
        armed: bool,
        note: Option<ListNote>,
    },
    AddressEntry {
        address: String,
        error: Option<String>,
    },
    Adding {
        address: String,
    },
    BeginningLogin {
        address: String,
    },
    /// Where to go and what to enter there for the login under way at the
    /// Relay at `address`.
    LoginDisplay {
        address: String,
    },
    Removing,
}

/// The one line something finished leaves beneath the list, standing until
/// the next key the reader presses.
#[derive(Clone, Debug)]
struct ListNote {
    text: String,
    failed: bool,
}

impl ListNote {
    fn said(text: String) -> Self {
        Self {
            text,
            failed: false,
        }
    }

    fn failed(text: String) -> Self {
        Self { text, failed: true }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayInputMode {
    Waiting,
    List,
    Address,
    Login,
}

/// What logging in at the selected Relay asks of the Client's own Server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RelayLoginAct {
    /// Begin a login at the Relay at this address.
    Begin(String),
    /// Follow the login already under way there, which nothing here follows.
    Follow(String),
}

impl RelayOverlay {
    pub(super) fn open(&mut self) {
        self.state = RelayOverlayState::Loading;
    }

    /// Steps back to the list from adding a Relay or watching a login, which
    /// goes on at the Server all the same, and closes the list itself.
    pub(super) fn back(&mut self) {
        self.state = match self.state {
            RelayOverlayState::AddressEntry { .. } | RelayOverlayState::LoginDisplay { .. } => {
                Self::listing(None)
            }
            _ => RelayOverlayState::Closed,
        };
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self.state, RelayOverlayState::Closed)
    }

    pub(super) fn loading_label(&self) -> Option<&'static str> {
        match self.state {
            RelayOverlayState::Loading => Some("Loading Relays…"),
            RelayOverlayState::Adding { .. } => Some("Adding Relay…"),
            RelayOverlayState::BeginningLogin { .. } => Some("Beginning login…"),
            RelayOverlayState::Removing => Some("Removing Relay…"),
            _ => None,
        }
    }

    pub(super) fn input_mode(&self) -> RelayInputMode {
        match self.state {
            RelayOverlayState::Listing { .. } => RelayInputMode::List,
            RelayOverlayState::AddressEntry { .. } => RelayInputMode::Address,
            RelayOverlayState::LoginDisplay { .. } => RelayInputMode::Login,
            RelayOverlayState::Closed
            | RelayOverlayState::Loading
            | RelayOverlayState::Adding { .. }
            | RelayOverlayState::BeginningLogin { .. }
            | RelayOverlayState::Removing => RelayInputMode::Waiting,
        }
    }

    /// Takes the Server's Relays, and answers the logins under way among them
    /// that nothing here follows yet.
    pub(super) fn load(&mut self, relays: Vec<Relay>) -> Vec<String> {
        self.relays = relays;
        self.listing_error = None;
        if matches!(self.state, RelayOverlayState::Loading) {
            self.selected = 0;
            self.window.open();
            self.state = Self::listing(None);
        }
        self.clamp_selection();
        self.relays
            .iter()
            .filter(|relay| is_pending(relay.login.as_ref()))
            .map(|relay| relay.address.clone())
            .filter(|address| self.following.insert(address.clone()))
            .collect()
    }

    pub(super) fn fail_listing(&mut self, error: String) {
        if matches!(self.state, RelayOverlayState::Loading) {
            self.relays.clear();
            self.selected = 0;
            self.listing_error = Some(error);
            self.state = Self::listing(None);
        }
    }

    pub(super) fn select_previous(&mut self) {
        if matches!(self.state, RelayOverlayState::Listing { .. }) && !self.relays.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.relays.len() - 1);
            self.window.reveal();
        }
    }

    pub(super) fn select_next(&mut self) {
        if matches!(self.state, RelayOverlayState::Listing { .. }) && !self.relays.is_empty() {
            self.selected = (self.selected + 1) % self.relays.len();
            self.window.reveal();
        }
    }

    /// Adding is one act begun from the list and finished from the address
    /// entry: from the list it opens the entry, and from the entry it sends
    /// the address typed there, answering it.
    pub(super) fn add(&mut self) -> Option<String> {
        match &mut self.state {
            RelayOverlayState::Listing { .. } => {
                self.state = RelayOverlayState::AddressEntry {
                    address: String::new(),
                    error: None,
                };
                None
            }
            RelayOverlayState::AddressEntry { address, error } => {
                let address = address.trim().to_owned();
                if address.is_empty() {
                    *error = Some("Type a Relay's address first".to_owned());
                    return None;
                }
                self.state = RelayOverlayState::Adding {
                    address: address.clone(),
                };
                Some(address)
            }
            _ => None,
        }
    }

    pub(super) fn insert(&mut self, text: &str) {
        if let RelayOverlayState::AddressEntry { address, error } = &mut self.state {
            address.extend(text.chars().filter(|character| !character.is_control()));
            *error = None;
        }
    }

    pub(super) fn delete_backward(&mut self) {
        if let RelayOverlayState::AddressEntry { address, error } = &mut self.state {
            address.pop();
            *error = None;
        }
    }

    pub(super) fn relay_added(&mut self, relay: Relay) {
        let address = relay.address.clone();
        let index = match self.relays.iter().position(|held| held.address == address) {
            Some(index) => {
                self.relays[index] = relay;
                index
            }
            None => {
                self.relays.push(relay);
                self.relays.len() - 1
            }
        };
        if matches!(self.state, RelayOverlayState::Adding { .. }) {
            self.selected = index;
            self.window.reveal();
            self.state = Self::listing(Some(ListNote::said(format!("Added {address}"))));
        }
    }

    /// The address stays as the reader typed it, with why it was refused
    /// beneath, so they can mend it rather than type it again.
    pub(super) fn addition_failed(&mut self, error: String) {
        if let RelayOverlayState::Adding { address } = &mut self.state {
            self.state = RelayOverlayState::AddressEntry {
                address: std::mem::take(address),
                error: Some(error),
            };
        }
    }

    /// Logs in at the selected Relay. A login already under way there is
    /// shown again rather than begun anew, since its code still stands.
    pub(super) fn log_in(&mut self) -> Option<RelayLoginAct> {
        if !matches!(self.state, RelayOverlayState::Listing { .. }) {
            return None;
        }
        let relay = self.relays.get(self.selected)?;
        let address = relay.address.clone();
        if is_pending(relay.login.as_ref()) {
            self.state = RelayOverlayState::LoginDisplay {
                address: address.clone(),
            };
            return self
                .following
                .insert(address.clone())
                .then_some(RelayLoginAct::Follow(address));
        }
        self.state = RelayOverlayState::BeginningLogin {
            address: address.clone(),
        };
        Some(RelayLoginAct::Begin(address))
    }

    /// Takes a login the Server began at the Relay at `address`, answering
    /// whether the Client is to follow it.
    pub(super) fn login_begun(&mut self, address: &str, login: RelayLogin) -> Option<String> {
        let pending = is_pending(Some(&login));
        if let Some(relay) = self.relay_mut(address) {
            relay.login = Some(login);
        }
        if matches!(&self.state, RelayOverlayState::BeginningLogin { address: beginning } if beginning == address)
        {
            self.state = RelayOverlayState::LoginDisplay {
                address: address.to_owned(),
            };
        }
        (pending && self.following.insert(address.to_owned())).then(|| address.to_owned())
    }

    pub(super) fn login_not_begun(&mut self, address: &str, error: &str) {
        if matches!(&self.state, RelayOverlayState::BeginningLogin { address: beginning } if beginning == address)
        {
            self.state = Self::listing(Some(ListNote::failed(format!(
                "Could not begin a login at {address}: {error}"
            ))));
        }
    }

    /// Takes how the login at the Relay at `address` ended. A display showing
    /// it resolves to the list, saying how.
    pub(super) fn login_settled(&mut self, address: &str, login: RelayLogin) {
        self.following.remove(address);
        let note = match &login.outcome {
            RelayLoginOutcome::Pending => return,
            RelayLoginOutcome::Done { account } => {
                ListNote::said(format!("Logged in at {address} as {}", account.username))
            }
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::NotAdmitted,
                ..
            } => ListNote::failed(format!(
                "You are not admitted to {address}; ask the Relay's operator to admit you"
            )),
            RelayLoginOutcome::Refused { message, .. } => {
                ListNote::failed(format!("The login at {address} ended: {message}"))
            }
        };
        if let Some(relay) = self.relay_mut(address) {
            if let RelayLoginOutcome::Done { account } = &login.outcome {
                relay.state = RelayState::LoggedIn;
                relay.unreachable = None;
                relay.account = Some(account.clone());
            }
            relay.login = Some(login);
        }
        if self.displays_login_at(address) {
            self.state = Self::listing(Some(note));
        }
    }

    /// Following the login at the Relay at `address` ended before the login
    /// did. It may go on at the Server, so showing it again follows it afresh.
    pub(super) fn login_lost(&mut self, address: &str, error: &str) {
        self.following.remove(address);
        if self.displays_login_at(address) {
            self.state = Self::listing(Some(ListNote::failed(format!(
                "Stopped following the login at {address}: {error}"
            ))));
        }
    }

    fn displays_login_at(&self, address: &str) -> bool {
        matches!(&self.state, RelayOverlayState::LoginDisplay { address: shown } if shown == address)
    }

    /// The address the login on display has its user visit.
    pub(super) fn copy_address(&self) -> Option<String> {
        self.login_display()
            .map(|(_, login)| login.verification_uri.clone())
    }

    /// The code the login on display has its user enter.
    pub(super) fn copy_code(&self) -> Option<String> {
        self.login_display()
            .map(|(_, login)| login.user_code.clone())
    }

    /// Removing a Relay is asked for twice, as ending a Pairing is: the first
    /// press arms the selected Relay's removal and only the second performs
    /// it.
    pub(super) fn remove_selected(&mut self) -> Option<String> {
        let RelayOverlayState::Listing { armed, note } = &mut self.state else {
            return None;
        };
        let address = self.relays.get(self.selected)?.address.clone();
        *note = None;
        if !*armed {
            *armed = true;
            return None;
        }
        self.state = RelayOverlayState::Removing;
        Some(address)
    }

    /// Puts down an armed removal and clears the note something finished
    /// left, which every key but the one that arms removal does.
    pub(super) fn disarm_removal(&mut self) {
        if let RelayOverlayState::Listing { armed, note } = &mut self.state {
            *armed = false;
            *note = None;
        }
    }

    pub(super) fn removal_armed(&self) -> bool {
        matches!(self.state, RelayOverlayState::Listing { armed: true, .. })
    }

    pub(super) fn relay_removed(&mut self, address: &str, acknowledged: bool) {
        self.relays.retain(|relay| relay.address != address);
        self.following.remove(address);
        self.clamp_selection();
        if matches!(self.state, RelayOverlayState::Removing) {
            self.state = Self::listing(Some(ListNote::said(if acknowledged {
                format!("Removed {address}")
            } else {
                format!("Removed {address} here; the Relay did not answer")
            })));
        }
    }

    pub(super) fn removal_failed(&mut self, error: String) {
        if matches!(self.state, RelayOverlayState::Removing) {
            self.state = Self::listing(Some(ListNote::failed(error)));
        }
    }

    pub(super) fn relays(&self) -> &[Relay] {
        match self.state {
            RelayOverlayState::Listing { .. } => &self.relays,
            _ => &[],
        }
    }

    pub(super) fn listing_error(&self) -> Option<&str> {
        self.listing_error.as_deref()
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn window(&self) -> &ListWindow {
        &self.window
    }

    /// The list's note, and whether it says something failed.
    pub(super) fn note(&self) -> Option<(&str, bool)> {
        match &self.state {
            RelayOverlayState::Listing {
                note: Some(note), ..
            } => Some((note.text.as_str(), note.failed)),
            _ => None,
        }
    }

    pub(super) fn address_entry(&self) -> Option<(&str, Option<&str>)> {
        match &self.state {
            RelayOverlayState::AddressEntry { address, error } => Some((address, error.as_deref())),
            _ => None,
        }
    }

    /// The Relay whose login is on display, and that login.
    pub(super) fn login_display(&self) -> Option<(&str, &RelayLogin)> {
        let RelayOverlayState::LoginDisplay { address } = &self.state else {
            return None;
        };
        let relay = self.relays.iter().find(|relay| &relay.address == address)?;
        Some((&relay.address, relay.login.as_ref()?))
    }

    fn listing(note: Option<ListNote>) -> RelayOverlayState {
        RelayOverlayState::Listing { armed: false, note }
    }

    fn relay_mut(&mut self, address: &str) -> Option<&mut Relay> {
        self.relays
            .iter_mut()
            .find(|relay| relay.address == address)
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.relays.len().saturating_sub(1));
    }
}

/// How a Relay stands, as its row in the list says it: a login under way
/// there first, with what its user does to finish it.
pub(super) fn relay_status(relay: &Relay) -> String {
    if let Some(login) = relay.login.as_ref().filter(|login| is_pending(Some(login))) {
        return format!(
            "Logging in · enter {} at {}",
            login.user_code, login.verification_uri
        );
    }
    match relay.state {
        RelayState::LoggedIn => relay.account.as_ref().map_or_else(
            || "Logged in".to_owned(),
            |account| format!("Logged in as {} ({})", account.username, account.provider),
        ),
        RelayState::LoginNeeded => match relay.login.as_ref().map(|login| &login.outcome) {
            Some(RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::NotAdmitted,
                ..
            }) => "Login needed · not admitted; ask the Relay's operator".to_owned(),
            Some(RelayLoginOutcome::Refused { message, .. }) => {
                format!("Login needed · {message}")
            }
            _ => "Login needed".to_owned(),
        },
        RelayState::Unreachable => relay.unreachable.as_ref().map_or_else(
            || "Unreachable".to_owned(),
            |unreachable| format!("Unreachable · {}", unreachable.message),
        ),
    }
}

fn is_pending(login: Option<&RelayLogin>) -> bool {
    login.is_some_and(|login| !login.outcome.is_settled())
}
