//! View state for the list of the Client's own Server's Relays, where its
//! user adds one by address, logs in there, and removes one.
//!
//! A login is the Server's to carry out (ADR-0048): the Client shows where its
//! user goes and the code they enter there, and follows the login until the
//! Server reports it ended. The Server answers the latest login at each Relay
//! in its listing, so a Client opened after the one that began a login finds
//! it standing where it does and follows it in turn.
//!
//! Every request the list sends names itself with a [`RelayRequest`], and an
//! answer resolves only the request it answers. One to a request the reader
//! has since moved past — a list closed and opened again, a login begun
//! afresh — moves nothing on screen; where it reports something the Server
//! did, a Relay added or removed, the list still holds to it. A listing
//! asked for before something it could not show — a login begun or ended,
//! a Relay added or removed — is asked for again rather than taken, so an
//! older picture never undoes what came after it.

use std::collections::HashMap;

use crate::protocol::{Relay, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelayState};

use super::list_window::ListWindow;

/// One request the Relay list sent the Client's own Server, told apart from
/// every other so its answer reaches only what asked for it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RelayRequest(u64);

/// Following the latest login at the Relay at `address`, under the request
/// whose answer says how it ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayLoginFollow {
    pub request: RelayRequest,
    pub address: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RelayOverlay {
    state: RelayOverlayState,
    relays: Vec<Relay>,
    /// Why the Server's Relays could not be listed, said above whatever the
    /// list holds until a listing lands.
    listing_error: Option<String>,
    /// The listing awaited — the one opening the list, or one asked for
    /// again — so a listing asked for before it lands nowhere.
    listing: Option<RelayRequest>,
    /// Whether something the listing awaited could not show has happened
    /// since it was asked for, so it is to be asked for again when it lands.
    listing_superseded: bool,
    /// The Relay the keys are on, held while the reader steps away from the
    /// list to add one or to watch a login.
    selected: usize,
    /// What follows the login at each Relay, so a login is followed once
    /// however often the list is opened, and a follower a later login has
    /// left behind is told apart from the one following it.
    followers: HashMap<String, Follower>,
    last_request: u64,
    window: ListWindow,
}

/// The request following a Relay's login, and the code of the login it was
/// asked to follow.
#[derive(Clone, Debug)]
struct Follower {
    request: RelayRequest,
    user_code: String,
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
        request: RelayRequest,
        address: String,
    },
    BeginningLogin {
        request: RelayRequest,
        address: String,
    },
    /// Where to go and what to enter there for the login under way at the
    /// Relay at `address`.
    LoginDisplay {
        address: String,
    },
    Removing {
        request: RelayRequest,
    },
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

    /// What the list says of a login at `address` that ended in `outcome`.
    fn login_ended(address: &str, outcome: &RelayLoginOutcome) -> Option<Self> {
        Some(match outcome {
            RelayLoginOutcome::Pending => return None,
            RelayLoginOutcome::Done { account } => {
                Self::said(format!("Logged in at {address} as {}", account.username))
            }
            RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::NotAdmitted,
                ..
            } => Self::failed(format!(
                "You are not admitted to {address}; ask the Relay's operator to admit you"
            )),
            RelayLoginOutcome::Refused { message, .. } => {
                Self::failed(format!("The login at {address} ended: {message}"))
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RelayInputMode {
    Waiting,
    List,
    Address,
    Login,
}

/// What a listing landing asks of the Client's own Server next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ListingLanded {
    /// Follow these logins under way, which nothing here follows yet.
    Follow(Vec<RelayLoginFollow>),
    /// Ask for the listing again under this request, the one landed having
    /// been asked for before something it could not show.
    AskAgain(RelayRequest),
}

/// What logging in at the selected Relay asks of the Client's own Server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RelayLoginAct {
    /// Begin a login at the Relay at `address`.
    Begin {
        request: RelayRequest,
        address: String,
    },
    /// Follow the login already under way there, which nothing here follows.
    Follow(RelayLoginFollow),
}

impl RelayOverlay {
    /// Opens the list, answering the listing it asks for.
    pub(super) fn open(&mut self) -> RelayRequest {
        self.state = RelayOverlayState::Loading;
        self.ask_for_listing()
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
            RelayOverlayState::Removing { .. } => Some("Removing Relay…"),
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
            | RelayOverlayState::Removing { .. } => RelayInputMode::Waiting,
        }
    }

    /// Takes the Server's Relays where they answer the listing awaited, and
    /// answers the logins under way among them that nothing here follows yet.
    /// A follower whose login the listing shows ended, or superseded by a
    /// later one, is left behind, and its answer will land nowhere. A listing
    /// asked for before something it could not show is asked for again.
    pub(super) fn load(&mut self, request: RelayRequest, relays: Vec<Relay>) -> ListingLanded {
        if self.listing != Some(request) {
            return ListingLanded::Follow(Vec::new());
        }
        if let Some(again) = self.ask_again_if_superseded() {
            return ListingLanded::AskAgain(again);
        }
        self.listing = None;
        self.listing_error = None;
        let selected = self
            .relays
            .get(self.selected)
            .map(|relay| relay.address.clone());
        self.relays = relays;
        if matches!(self.state, RelayOverlayState::Loading) {
            self.selected = 0;
            self.window.open();
            self.state = Self::listing(None);
        } else if let Some(index) = selected.and_then(|selected| {
            self.relays
                .iter()
                .position(|relay| relay.address == selected)
        }) {
            self.selected = index;
        }
        self.clamp_selection();
        let relays = &self.relays;
        self.followers.retain(|address, _| {
            relays
                .iter()
                .any(|relay| &relay.address == address && is_pending(relay.login.as_ref()))
        });
        let follows = self
            .relays
            .clone()
            .iter()
            .filter_map(|relay| self.follow(&relay.address, relay.login.as_ref()?))
            .collect();
        self.resolve_display();
        ListingLanded::Follow(follows)
    }

    /// Takes a listing that failed, answering the listing to ask for again
    /// where it was asked for before something it could not show.
    pub(super) fn fail_listing(
        &mut self,
        request: RelayRequest,
        error: String,
    ) -> Option<RelayRequest> {
        if self.listing != Some(request) {
            return None;
        }
        if let Some(again) = self.ask_again_if_superseded() {
            return Some(again);
        }
        self.listing = None;
        self.listing_error = Some(error);
        if matches!(self.state, RelayOverlayState::Loading) {
            self.relays.clear();
            self.selected = 0;
            self.state = Self::listing(None);
        }
        None
    }

    /// Asks for the Server's Relays, in place of any listing awaited.
    fn ask_for_listing(&mut self) -> RelayRequest {
        let request = self.issue();
        self.listing = Some(request);
        self.listing_superseded = false;
        request
    }

    /// Asks for the listing awaited again where something it could not show
    /// has happened since it was asked for.
    fn ask_again_if_superseded(&mut self) -> Option<RelayRequest> {
        self.listing_superseded.then(|| self.ask_for_listing())
    }

    /// Marks the listing awaited, if any, as one asked for before something
    /// it cannot show: a login begun or ended, a Relay added or removed.
    fn supersede_listing(&mut self) {
        self.listing_superseded |= self.listing.is_some();
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
    /// the address typed there, answering the request that adds it.
    pub(super) fn add(&mut self) -> Option<(RelayRequest, String)> {
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
                let request = self.issue();
                self.state = RelayOverlayState::Adding {
                    request,
                    address: address.clone(),
                };
                Some((request, address))
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

    /// Takes a Relay the Server added. The list holds it whichever addition
    /// asked, but only the addition awaited returns to the list, and there,
    /// where the Server's Relays could not be listed before, it answers the
    /// listing to ask for again now the Server answers.
    pub(super) fn relay_added(
        &mut self,
        request: RelayRequest,
        relay: Relay,
    ) -> Option<RelayRequest> {
        self.supersede_listing();
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
        if !self.awaits_addition(request) {
            return None;
        }
        self.selected = index;
        self.window.reveal();
        self.state = Self::listing(Some(ListNote::said(format!("Added {address}"))));
        if self.listing_error.is_none() || self.listing.is_some() {
            return None;
        }
        Some(self.ask_for_listing())
    }

    /// The address stays as the reader typed it, with why it was refused
    /// beneath, so they can mend it rather than type it again.
    pub(super) fn addition_failed(&mut self, request: RelayRequest, error: String) {
        if let RelayOverlayState::Adding {
            request: awaited,
            address,
        } = &mut self.state
            && *awaited == request
        {
            self.state = RelayOverlayState::AddressEntry {
                address: std::mem::take(address),
                error: Some(error),
            };
        }
    }

    fn awaits_addition(&self, request: RelayRequest) -> bool {
        matches!(self.state, RelayOverlayState::Adding { request: awaited, .. } if awaited == request)
    }

    /// Logs in at the selected Relay. A login already under way there is
    /// shown again rather than begun anew, since its code still stands.
    pub(super) fn log_in(&mut self) -> Option<RelayLoginAct> {
        if !matches!(self.state, RelayOverlayState::Listing { .. }) {
            return None;
        }
        let relay = self.relays.get(self.selected)?.clone();
        if let Some(login) = relay.login.as_ref().filter(|login| is_pending(Some(login))) {
            self.state = RelayOverlayState::LoginDisplay {
                address: relay.address.clone(),
            };
            return self
                .follow(&relay.address, login)
                .map(RelayLoginAct::Follow);
        }
        let request = self.issue();
        self.state = RelayOverlayState::BeginningLogin {
            request,
            address: relay.address.clone(),
        };
        Some(RelayLoginAct::Begin {
            request,
            address: relay.address,
        })
    }

    /// Takes the login the Server began for the beginning awaited, answering
    /// what is to follow it.
    pub(super) fn login_begun(
        &mut self,
        request: RelayRequest,
        login: RelayLogin,
    ) -> Option<RelayLoginFollow> {
        let RelayOverlayState::BeginningLogin {
            request: awaited,
            address,
        } = &self.state
        else {
            return None;
        };
        if *awaited != request {
            return None;
        }
        let address = address.clone();
        self.supersede_listing();
        let follow = self.follow(&address, &login);
        if let Some(relay) = self.relay_mut(&address) {
            relay.login = Some(login);
        }
        self.state = RelayOverlayState::LoginDisplay { address };
        follow
    }

    pub(super) fn login_not_begun(&mut self, request: RelayRequest, error: &str) {
        if let RelayOverlayState::BeginningLogin {
            request: awaited,
            address,
        } = &self.state
            && *awaited == request
        {
            self.state = Self::listing(Some(ListNote::failed(format!(
                "Could not begin a login at {address}: {error}"
            ))));
        }
    }

    /// Takes how a login ended, from the follower that asked. A display
    /// showing it resolves to the list, saying how.
    pub(super) fn login_settled(&mut self, request: RelayRequest, login: RelayLogin) {
        let Some(address) = self.release_follower(request) else {
            return;
        };
        self.supersede_listing();
        if let Some(relay) = self.relay_mut(&address) {
            if let RelayLoginOutcome::Done { account } = &login.outcome {
                relay.state = RelayState::LoggedIn;
                relay.unreachable = None;
                relay.account = Some(account.clone());
            }
            relay.login = Some(login);
        }
        self.resolve_display();
    }

    /// Following a login ended before the login did. It may go on at the
    /// Server, so showing it again follows it afresh.
    pub(super) fn login_lost(&mut self, request: RelayRequest, error: &str) {
        let Some(address) = self.release_follower(request) else {
            return;
        };
        if self.displays_login_at(&address) {
            self.state = Self::listing(Some(ListNote::failed(format!(
                "Stopped following the login at {address}: {error}"
            ))));
        }
    }

    /// Follows `login`, at the Relay at `address`, unless it has ended or is
    /// already followed. A follower of an earlier login there is replaced.
    fn follow(&mut self, address: &str, login: &RelayLogin) -> Option<RelayLoginFollow> {
        if !is_pending(Some(login))
            || self
                .followers
                .get(address)
                .is_some_and(|follower| follower.user_code == login.user_code)
        {
            return None;
        }
        let request = self.issue();
        self.followers.insert(
            address.to_owned(),
            Follower {
                request,
                user_code: login.user_code.clone(),
            },
        );
        Some(RelayLoginFollow {
            request,
            address: address.to_owned(),
        })
    }

    /// Lets go of the follower that sent `request`, answering the Relay whose
    /// login it followed — or nothing, where it is one left behind.
    fn release_follower(&mut self, request: RelayRequest) -> Option<String> {
        let address = self.followers.iter().find_map(|(address, follower)| {
            (follower.request == request).then(|| address.clone())
        })?;
        self.followers.remove(&address);
        Some(address)
    }

    /// Returns a display whose login is no longer under way to the list,
    /// saying how it ended.
    fn resolve_display(&mut self) {
        let RelayOverlayState::LoginDisplay { address } = &self.state else {
            return;
        };
        let login = self
            .relays
            .iter()
            .find(|relay| &relay.address == address)
            .and_then(|relay| relay.login.as_ref());
        if is_pending(login) {
            return;
        }
        let note = login.and_then(|login| ListNote::login_ended(address, &login.outcome));
        self.state = Self::listing(note);
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
    /// it, answering the request that removes it.
    pub(super) fn remove_selected(&mut self) -> Option<(RelayRequest, String)> {
        let RelayOverlayState::Listing { armed, note } = &mut self.state else {
            return None;
        };
        let address = self.relays.get(self.selected)?.address.clone();
        *note = None;
        if !*armed {
            *armed = true;
            return None;
        }
        let request = self.issue();
        self.state = RelayOverlayState::Removing { request };
        Some((request, address))
    }

    /// Puts down an armed removal and clears the note something finished
    /// left, which every key but the one that arms removal does, answering
    /// whether there was either to put down.
    pub(super) fn disarm_removal(&mut self) -> bool {
        match &mut self.state {
            RelayOverlayState::Listing { armed, note } => {
                std::mem::take(armed) | note.take().is_some()
            }
            _ => false,
        }
    }

    pub(super) fn removal_armed(&self) -> bool {
        matches!(self.state, RelayOverlayState::Listing { armed: true, .. })
    }

    /// Takes a Relay the Server removed. The list lets go of it whichever
    /// removal asked, but only the removal awaited returns to the list.
    pub(super) fn relay_removed(
        &mut self,
        request: RelayRequest,
        address: &str,
        acknowledged: bool,
    ) {
        self.supersede_listing();
        self.relays.retain(|relay| relay.address != address);
        self.followers.remove(address);
        self.clamp_selection();
        if self.awaits_removal(request) {
            self.state = Self::listing(Some(ListNote::said(if acknowledged {
                format!("Removed {address}")
            } else {
                format!("Removed {address} here; the Relay did not answer")
            })));
        }
    }

    pub(super) fn removal_failed(&mut self, request: RelayRequest, error: String) {
        if self.awaits_removal(request) {
            self.state = Self::listing(Some(ListNote::failed(error)));
        }
    }

    fn awaits_removal(&self, request: RelayRequest) -> bool {
        matches!(self.state, RelayOverlayState::Removing { request: awaited } if awaited == request)
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

    fn issue(&mut self) -> RelayRequest {
        self.last_request += 1;
        RelayRequest(self.last_request)
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
