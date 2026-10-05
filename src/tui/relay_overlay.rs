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
//! afresh — moves nothing on screen.
//!
//! What the list holds of the Relays is only ever a picture the Server gave
//! whole: a listing it answered, or the Relays it pushed as they changed —
//! which keeps the list live whether or not it is open, and the Client
//! knowing how each Relay stands wherever else it needs to. Each picture
//! carries the run of the Server it came from and the revision its Relays
//! stood at there, and the list takes only a picture from the run it is
//! attached to, no older than the one it holds, so no two undo each other
//! however they cross. What the Server answers a Relay added, removed or
//! logged in at moves the list on — saying what was done — but changes
//! nothing it holds: the list asks for a picture afresh instead, which shows
//! that, and whatever else has happened since, at a revision of its own.
//! The login on display ends only with that login, however the Relay is
//! pictured meanwhile.
//!
//! Whether the Server Serves through each Relay is that Relay's entry's, and
//! chosen here: the choice waits on the Server's answer, and the row says it
//! only as the Server pictures it.
//!
//! A redemption refused for want of a login at a Relay logs in there through
//! the same followers, so a login is begun and followed once whichever
//! surface asks for it, and the list shows it as under way.

use std::collections::HashMap;

use uuid::Uuid;

use crate::protocol::{
    Relay, RelayListing, RelayLogin, RelayLoginOutcome, RelayLoginRefusal, RelayState,
};

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
    /// The run of the Server, and the revision there, that the Relays held
    /// were pictured at.
    held: Option<Pictured>,
    /// The run of the Server the Client is attached to, as its pushes say:
    /// each comes from the Server attached now, so a picture from any other
    /// run is an earlier one's.
    attached: Option<Uuid>,
    /// The Relay to log in at once the listing awaited lands, where the list
    /// was opened to log in there.
    log_in_at: Option<String>,
    /// The Relay just added, for the keys to be on once a picture shows it.
    select_on_listing: Option<String>,
    /// Why the Server's Relays could not be listed, said above whatever the
    /// list holds until a listing lands.
    listing_error: Option<String>,
    /// The listing awaited — the one opening the list, or one asked for
    /// afresh — so a listing asked for before it lands nowhere.
    listing: Option<RelayRequest>,
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

/// The request following a Relay's login, and that login as it was when it
/// was asked to follow it.
#[derive(Clone, Debug)]
struct Follower {
    request: RelayRequest,
    login: RelayLogin,
}

/// Where a picture of the Relays was taken: in which run of the Server, and
/// at which revision there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pictured {
    instance: Uuid,
    revision: u64,
}

impl Pictured {
    fn of(listing: &RelayListing) -> Self {
        Self {
            instance: listing.instance,
            revision: listing.revision,
        }
    }
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
    /// Where to go and what to enter there for `login`, under way at the
    /// Relay at `address`.
    LoginDisplay {
        address: String,
        login: RelayLogin,
    },
    Removing {
        request: RelayRequest,
    },
    /// Waiting on the Server to store whether it Serves through the Relay at
    /// `address`, as `serve_through` asks.
    SettingServeThrough {
        request: RelayRequest,
        address: String,
        serve_through: bool,
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
}

/// What is said of a login at `address` that ended in `outcome`, and whether
/// it says the login formed no Login; nothing of one still under way.
fn login_ended(address: &str, outcome: &RelayLoginOutcome) -> Option<ListNote> {
    Some(match outcome {
        RelayLoginOutcome::Pending => return None,
        RelayLoginOutcome::Done { account } => {
            ListNote::said(format!("Logged in at {address} as {}", account.username))
        }
        RelayLoginOutcome::Refused {
            reason: RelayLoginRefusal::NotAdmitted,
            ..
        } => ListNote::failed(format!(
            "You are not admitted to {address}; ask the Relay's operator to admit you"
        )),
        RelayLoginOutcome::Refused {
            reason: RelayLoginRefusal::LoginsCapReached { limit },
            ..
        } => ListNote::failed(format!(
            "Your Account already has {} logged in at {address}, as many as the Relay's \
             operator allows; remove the Relay from a Server that no longer needs it, or ask \
             the operator to raise the cap",
            servers(*limit)
        )),
        RelayLoginOutcome::Refused { message, .. } => {
            ListNote::failed(format!("The login at {address} ended: {message}"))
        }
    })
}

/// Why a login at `address` that ended in `outcome` formed no Login, as the
/// step that waited on it says it; nothing of one done or still under way.
pub(super) fn login_refused(address: &str, outcome: &RelayLoginOutcome) -> Option<String> {
    login_ended(address, outcome)
        .filter(|note| note.failed)
        .map(|note| note.text)
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
    /// Log in at the Relay the list was opened to log in at, now it is
    /// listed. Logins under way elsewhere are followed as the list next
    /// lands.
    LogIn(RelayLoginAct),
}

/// What logging in at a Relay named by its address asks of the Client's own
/// Server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RelayLoginLead {
    /// The list is open on that Relay already: log in there now, if there is
    /// anything to ask.
    Now(Option<RelayLoginAct>),
    /// List the Relays under this request, logging in there once it lands.
    Listing(RelayRequest),
}

/// What logging in at a Relay a redemption waits on asks of the Client's own
/// Server, leaving the list as it stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RedemptionLoginLead {
    /// A login is under way there already, whose code still stands: show it,
    /// following it where nothing follows it yet.
    UnderWay {
        login: RelayLogin,
        follow: Option<RelayLoginFollow>,
    },
    /// The Server holds no entry for the Relay: add it under this request,
    /// then begin a login there.
    Add(RelayRequest),
    /// Begin a login there under this request.
    Begin(RelayRequest),
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
        self.log_in_at = None;
        self.ask_for_listing()
    }

    /// Logs in at the Relay at `address`: at once, where the list is open
    /// and holds it, and otherwise by opening the list and logging in there
    /// once it lands — or, where the Server holds no entry for it, by
    /// offering to add it, its address typed in already.
    pub(super) fn log_in_at(&mut self, address: String) -> RelayLoginLead {
        if matches!(self.state, RelayOverlayState::Listing { .. })
            && let Some(index) = self.position(&address)
        {
            self.selected = index;
            self.window.reveal();
            return RelayLoginLead::Now(self.log_in());
        }
        let request = self.open();
        self.log_in_at = Some(address);
        RelayLoginLead::Listing(request)
    }

    /// Steps back to the list from adding a Relay or watching a login, which
    /// goes on at the Server all the same, and closes the list itself.
    pub(super) fn back(&mut self) {
        self.state = match self.state {
            RelayOverlayState::AddressEntry { .. } | RelayOverlayState::LoginDisplay { .. } => {
                Self::listing(None)
            }
            _ => {
                self.log_in_at = None;
                RelayOverlayState::Closed
            }
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
            RelayOverlayState::SettingServeThrough {
                serve_through: true,
                ..
            } => Some("Turning Serve through on…"),
            RelayOverlayState::SettingServeThrough {
                serve_through: false,
                ..
            } => Some("Turning Serve through off…"),
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
            | RelayOverlayState::Removing { .. }
            | RelayOverlayState::SettingServeThrough { .. } => RelayInputMode::Waiting,
        }
    }

    /// Takes the Server's Relays where they answer the listing awaited, and
    /// answers the logins under way among them that nothing here follows yet.
    /// A listing older than what the list holds, or from an earlier run of
    /// the Server, is not taken, though it lands: the list holds something
    /// newer.
    pub(super) fn load(&mut self, request: RelayRequest, listing: RelayListing) -> ListingLanded {
        if self.listing != Some(request) {
            return ListingLanded::Follow(Vec::new());
        }
        self.listing = None;
        self.listing_error = None;
        if matches!(self.state, RelayOverlayState::Loading) {
            self.selected = 0;
            self.window.open();
            self.state = Self::listing(None);
        }
        if self.takes(&listing) {
            self.take(listing);
        }
        if let Some(address) = self.log_in_at.take() {
            match self.position(&address) {
                Some(index) => {
                    self.selected = index;
                    self.window.reveal();
                    if let Some(act) = self.log_in() {
                        return ListingLanded::LogIn(act);
                    }
                }
                None => {
                    self.state = RelayOverlayState::AddressEntry {
                        address,
                        error: None,
                    };
                }
            }
        }
        let follows = self
            .relays
            .clone()
            .iter()
            .filter_map(|relay| self.follow(&relay.address, relay.login.as_ref()?))
            .collect();
        ListingLanded::Follow(follows)
    }

    /// Takes the Relays as the Server pushed them, whether or not the list is
    /// open: always from a run of the Server it was not attached to before,
    /// which is the one attached now, and otherwise where they are newer
    /// than what it holds. Nothing is followed for it: each change to a
    /// login is pushed in turn.
    pub(super) fn receive_pushed(&mut self, listing: RelayListing) {
        if self.attached != Some(listing.instance) {
            self.attached = Some(listing.instance);
            self.take(listing);
        } else if self.takes(&listing) {
            self.take(listing);
        }
    }

    /// Asks for the Server's Relays afresh, for the list to show what was
    /// just done there and whatever else has happened since.
    pub(super) fn refresh(&mut self) -> RelayRequest {
        self.ask_for_listing()
    }

    /// How the Relay at `address` stands, as the list last heard of it.
    pub(super) fn held_state(&self, address: &str) -> Option<RelayState> {
        self.held(address).map(|relay| relay.state)
    }

    /// The Relay at `address`, as the list last heard of it.
    pub(super) fn held(&self, address: &str) -> Option<&Relay> {
        self.relays.iter().find(|relay| relay.address == address)
    }

    /// The Relays as the list last heard of them, whether or not it is open.
    pub(super) fn held_relays(&self) -> &[Relay] {
        &self.relays
    }

    /// Whether `listing` is to be taken over what the list holds: it comes
    /// from the run of the Server the Client is attached to, where that is
    /// known, and from the same run as what is held at a revision no earlier.
    fn takes(&self, listing: &RelayListing) -> bool {
        if self
            .attached
            .is_some_and(|attached| attached != listing.instance)
        {
            return false;
        }
        self.held.is_none_or(|held| {
            held.instance == listing.instance && listing.revision >= held.revision
        })
    }

    /// Holds `listing` as the Relays, keeping the reader on the Relay they
    /// were on — or one just added, once it shows — and lets go of followers
    /// whose very login it shows ended.
    fn take(&mut self, listing: RelayListing) {
        let selected = self
            .relays
            .get(self.selected)
            .map(|relay| relay.address.clone());
        self.held = Some(Pictured::of(&listing));
        self.relays = listing.relays;
        if let Some(index) = self
            .select_on_listing
            .as_deref()
            .and_then(|added| self.position(added))
        {
            self.selected = index;
            self.select_on_listing = None;
            self.window.reveal();
        } else if let Some(index) = selected.and_then(|selected| self.position(&selected)) {
            self.selected = index;
        }
        self.clamp_selection();
        let relays = &self.relays;
        self.followers.retain(|address, follower| {
            !relays.iter().any(|relay| {
                &relay.address == address
                    && relay.login.as_ref().is_some_and(|login| {
                        login.user_code == follower.login.user_code && login.outcome.is_settled()
                    })
            })
        });
        self.resolve_display();
    }

    fn position(&self, address: &str) -> Option<usize> {
        self.relays
            .iter()
            .position(|relay| relay.address == address)
    }

    /// Takes a listing that failed, saying why above whatever the list holds.
    pub(super) fn fail_listing(&mut self, request: RelayRequest, error: String) {
        if self.listing != Some(request) {
            return;
        }
        self.listing = None;
        self.listing_error = Some(error);
        self.log_in_at = None;
        if matches!(self.state, RelayOverlayState::Loading) {
            self.relays.clear();
            self.selected = 0;
            self.state = Self::listing(None);
        }
    }

    /// Asks for the Server's Relays, in place of any listing awaited.
    fn ask_for_listing(&mut self) -> RelayRequest {
        let request = self.issue();
        self.listing = Some(request);
        request
    }

    pub(super) fn select_previous(&mut self) {
        if matches!(self.state, RelayOverlayState::Listing { .. }) && !self.relays.is_empty() {
            // The reader moving is where the keys are to be, over any Relay
            // just added and yet to show.
            self.select_on_listing = None;
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.relays.len() - 1);
            self.window.reveal();
        }
    }

    pub(super) fn select_next(&mut self) {
        if matches!(self.state, RelayOverlayState::Listing { .. }) && !self.relays.is_empty() {
            self.select_on_listing = None;
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

    /// Takes a Relay the Server added: the addition awaited returns to the
    /// list, saying so, with the keys to be on the Relay once it shows. The
    /// list holds nothing more of it until a picture shows it, answered to
    /// the listing this asks for — the Relay may be gone again by then.
    pub(super) fn relay_added(&mut self, request: RelayRequest, relay: &Relay) -> RelayRequest {
        if self.awaits_addition(request) {
            self.select_on_listing = Some(relay.address.clone());
            self.state = Self::listing(Some(ListNote::said(format!("Added {}", relay.address))));
        }
        self.ask_for_listing()
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
        if let Some(login) = self.under_way(&relay.address) {
            self.state = RelayOverlayState::LoginDisplay {
                address: relay.address.clone(),
                login: login.clone(),
            };
            return self
                .follow(&relay.address, &login)
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
        let follow = self.follow(&address, &login);
        self.state = RelayOverlayState::LoginDisplay { address, login };
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

    /// Takes how a login ended, from the follower that asked, answering the
    /// Relay it was at — or nothing, from a follower left behind. A display
    /// showing it resolves to the list, saying how; how the Relay then
    /// stands is for a picture asked for afresh to show.
    pub(super) fn login_settled(
        &mut self,
        request: RelayRequest,
        login: &RelayLogin,
    ) -> Option<String> {
        let address = self.release_follower(request)?;
        self.resolve_display_with(&address, login);
        Some(address)
    }

    /// Following a login ended before the login did, answering the Relay it
    /// was at — or nothing, from a follower left behind. It may go on at the
    /// Server, so showing it again follows it afresh.
    pub(super) fn login_lost(&mut self, request: RelayRequest, error: &str) -> Option<String> {
        let address = self.release_follower(request)?;
        if self.displays_login_at(&address) {
            self.state = Self::listing(Some(ListNote::failed(format!(
                "Stopped following the login at {address}: {error}"
            ))));
        }
        Some(address)
    }

    /// Logs in at the Relay at `address` for a redemption refused for want of
    /// a Login there, leaving the list as it stands: a login already under
    /// way there is shown again rather than begun anew, since its code still
    /// stands, and one is begun otherwise — once the Relay is added, where
    /// the Server holds no entry for it.
    pub(super) fn log_in_for_redemption(&mut self, address: &str) -> RedemptionLoginLead {
        if let Some(login) = self.under_way(address) {
            let follow = self.follow(address, &login);
            return RedemptionLoginLead::UnderWay { login, follow };
        }
        let request = self.issue();
        if self.held(address).is_some() {
            RedemptionLoginLead::Begin(request)
        } else {
            RedemptionLoginLead::Add(request)
        }
    }

    /// Follows `login`, begun at the Relay at `address` for some other
    /// surface than the list, unless it is followed already.
    pub(super) fn follow_login(
        &mut self,
        address: &str,
        login: &RelayLogin,
    ) -> Option<RelayLoginFollow> {
        self.follow(address, login)
    }

    /// Names a request some other surface sends about a Relay, told apart
    /// from every request the list sends.
    pub(super) fn issue_request(&mut self) -> RelayRequest {
        self.issue()
    }

    /// Follows `login`, at the Relay at `address`, unless it has ended or is
    /// already followed. A follower of an earlier login there is replaced.
    fn follow(&mut self, address: &str, login: &RelayLogin) -> Option<RelayLoginFollow> {
        if !is_pending(Some(login))
            || self
                .followers
                .get(address)
                .is_some_and(|follower| follower.login.user_code == login.user_code)
        {
            return None;
        }
        let request = self.issue();
        self.followers.insert(
            address.to_owned(),
            Follower {
                request,
                login: login.clone(),
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

    /// Returns a display whose login the Relays held show ended to the list,
    /// saying how. Only that login ending resolves it: a picture of the Relay
    /// from before it began, or one showing a later login begun elsewhere —
    /// whose follower reports the one on display given up — leaves it be.
    fn resolve_display(&mut self) {
        let RelayOverlayState::LoginDisplay { address, login } = &self.state else {
            return;
        };
        let Some(ended) = self
            .relays
            .iter()
            .find(|relay| &relay.address == address)
            .and_then(|relay| relay.login.clone())
            .filter(|held| held.user_code == login.user_code)
        else {
            return;
        };
        let address = address.clone();
        self.resolve_display_with(&address, &ended);
    }

    /// Returns a display of `login`, at the Relay at `address`, to the list
    /// once it has ended, saying how.
    fn resolve_display_with(&mut self, address: &str, login: &RelayLogin) {
        let RelayOverlayState::LoginDisplay {
            address: shown_at,
            login: shown,
        } = &self.state
        else {
            return;
        };
        if shown_at != address || shown.user_code != login.user_code || is_pending(Some(login)) {
            return;
        }
        self.state = Self::listing(login_ended(address, &login.outcome));
    }

    fn displays_login_at(&self, address: &str) -> bool {
        matches!(&self.state, RelayOverlayState::LoginDisplay { address: shown, .. } if shown == address)
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

    /// Takes a Relay the Server removed: the removal awaited returns to the
    /// list, saying so. The list lets go of it as a picture no longer shows
    /// it, answered to the listing this asks for — it may be back by then.
    pub(super) fn relay_removed(
        &mut self,
        request: RelayRequest,
        address: &str,
        acknowledged: bool,
    ) -> RelayRequest {
        if self.awaits_removal(request) {
            self.state = Self::listing(Some(ListNote::said(if acknowledged {
                format!("Removed {address}")
            } else {
                format!("Removed {address} here; the Relay did not answer")
            })));
        }
        self.ask_for_listing()
    }

    pub(super) fn removal_failed(&mut self, request: RelayRequest, error: String) {
        if self.awaits_removal(request) {
            self.state = Self::listing(Some(ListNote::failed(error)));
        }
    }

    fn awaits_removal(&self, request: RelayRequest) -> bool {
        matches!(self.state, RelayOverlayState::Removing { request: awaited } if awaited == request)
    }

    /// Asks for the selected Relay to be Served through where it is not, and
    /// not where it is — as the Server last pictured it — answering the
    /// request that asks. The choice waits on the Server's answer, and the
    /// row says it only once a picture shows it.
    pub(super) fn toggle_serve_through(&mut self) -> Option<(RelayRequest, String, bool)> {
        if !matches!(self.state, RelayOverlayState::Listing { .. }) {
            return None;
        }
        let relay = self.relays.get(self.selected)?;
        let (address, serve_through) = (relay.address.clone(), !relay.serve_through);
        let request = self.issue();
        self.state = RelayOverlayState::SettingServeThrough {
            request,
            address: address.clone(),
            serve_through,
        };
        Some((request, address, serve_through))
    }

    /// Takes the Relay as the Server stood it once it stored the choice
    /// awaited: the choice returns to the list, saying what it does —
    /// `serving` says whether the Server is Serving — and the list asks for
    /// a picture afresh, which shows it.
    pub(super) fn serve_through_set(
        &mut self,
        request: RelayRequest,
        relay: &Relay,
        serving: bool,
    ) -> RelayRequest {
        if self.awaits_serve_through(request) {
            self.state = Self::listing(Some(ListNote::said(serve_through_said(relay, serving))));
        }
        self.ask_for_listing()
    }

    pub(super) fn serve_through_failed(&mut self, request: RelayRequest, error: &str) {
        if let RelayOverlayState::SettingServeThrough {
            request: awaited,
            address,
            ..
        } = &self.state
            && *awaited == request
        {
            let note = ListNote::failed(format!(
                "Could not change whether this Server Serves through {address}: {error}"
            ));
            self.state = Self::listing(Some(note));
        }
    }

    fn awaits_serve_through(&self, request: RelayRequest) -> bool {
        matches!(self.state, RelayOverlayState::SettingServeThrough { request: awaited, .. } if awaited == request)
    }

    /// Whether the Server Serves through the Relay the keys are on, as it
    /// last pictured it.
    pub(super) fn selected_serve_through(&self) -> Option<bool> {
        self.relays()
            .get(self.selected)
            .map(|relay| relay.serve_through)
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
        let RelayOverlayState::LoginDisplay { address, login } = &self.state else {
            return None;
        };
        Some((address, login))
    }

    /// What Enter does on the Relay the keys are on, where it does anything
    /// worth offering: shows again a login under way there, or logs in at a
    /// Relay that needs a login. One merely Unreachable, or logged in, is
    /// offered no login.
    pub(super) fn selected_offer(&self) -> Option<&'static str> {
        let relay = self.relays().get(self.selected)?;
        if self.under_way(&relay.address).is_some() {
            Some("Enter show login")
        } else if relay.state == RelayState::LoginNeeded {
            Some("Enter log in")
        } else {
            None
        }
    }

    /// How `relay` stands, as its row says it: as [`relay_status`] says, save
    /// that a login this Client follows there, which no picture yet shows,
    /// is said to be under way.
    pub(super) fn status(&self, relay: &Relay) -> String {
        match self.under_way(&relay.address) {
            Some(login) if !is_pending(relay.login.as_ref()) => logging_in(&login),
            _ => relay_status(relay),
        }
    }

    /// The login under way at the Relay at `address`: the one the list shows
    /// pending there, or else the one this Client began or follows there,
    /// which a picture taken before it began does not show.
    fn under_way(&self, address: &str) -> Option<RelayLogin> {
        self.held(address)
            .and_then(|relay| relay.login.clone())
            .filter(|login| is_pending(Some(login)))
            .or_else(|| {
                self.followers
                    .get(address)
                    .map(|follower| follower.login.clone())
            })
    }

    fn issue(&mut self) -> RelayRequest {
        self.last_request += 1;
        RelayRequest(self.last_request)
    }

    fn listing(note: Option<ListNote>) -> RelayOverlayState {
        RelayOverlayState::Listing { armed: false, note }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.relays.len().saturating_sub(1));
    }
}

/// How a Relay stands, as its row in the list says it: a login under way
/// there first, with what its user does to finish it.
fn relay_status(relay: &Relay) -> String {
    if let Some(login) = relay.login.as_ref().filter(|login| is_pending(Some(login))) {
        return logging_in(login);
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
            Some(RelayLoginOutcome::Refused {
                reason: RelayLoginRefusal::LoginsCapReached { limit },
                ..
            }) => format!(
                "Login needed · at the cap of {}; ask the Relay's operator",
                servers(*limit)
            ),
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

/// What a row says of the Server Serving through `relay`, where it does —
/// and of what it waits on, where that is anything: a login there, or
/// Serving itself, which `serving` says is on.
pub(super) fn serve_through_status(relay: &Relay, serving: bool) -> Option<String> {
    if !relay.serve_through {
        return None;
    }
    Some(match waits_on(relay, serving) {
        Some(waits) => format!("Serves through once {waits}"),
        None => "Serving through".to_owned(),
    })
}

/// What the Server Serving through `relay` waits on, where it waits.
fn waits_on(relay: &Relay, serving: bool) -> Option<&'static str> {
    match (relay.state == RelayState::LoginNeeded, serving) {
        (true, true) => Some("logged in"),
        (true, false) => Some("logged in and Serving is on"),
        (false, false) => Some("Serving is on"),
        (false, true) => None,
    }
}

/// What the list says of the Server choosing whether it Serves through
/// `relay`, as the Server answered it.
fn serve_through_said(relay: &Relay, serving: bool) -> String {
    let address = &relay.address;
    if !relay.serve_through {
        return format!("This Server no longer Serves through {address}");
    }
    let once = match (relay.state == RelayState::LoginNeeded, serving) {
        (true, true) => " once it is logged in there",
        (true, false) => " once it is logged in there and Serving is on; /serve turns Serving on",
        (false, false) => " once Serving is on; /serve turns Serving on",
        (false, true) => "",
    };
    format!("This Server Serves through {address}{once}")
}

/// A login under way, with what its user does to finish it.
fn logging_in(login: &RelayLogin) -> String {
    format!(
        "Logging in · enter {} at {}",
        login.user_code, login.verification_uri
    )
}

/// `count` Servers, said as a reader says it.
fn servers(count: u32) -> String {
    if count == 1 {
        "1 Server".to_owned()
    } else {
        format!("{count} Servers")
    }
}

fn is_pending(login: Option<&RelayLogin>) -> bool {
    login.is_some_and(|login| !login.outcome.is_settled())
}
