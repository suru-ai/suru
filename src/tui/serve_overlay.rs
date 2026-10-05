//! View state for the Serving user's picker of the ways an Invite offers, and
//! Invite manager.
//!
//! The ways are the machine's own addresses, found as `/serve` opens and
//! listed while the Server says its Serving listener listens, at the port it
//! listens on, and the Client's own Server's Relays as the Server last
//! pictured them, each followed while the picker stands. A Relay is offered
//! where the Server Serves through it and holds a Login there that stands —
//! what the Server asks of a Relay an Invite offers — and shown otherwise
//! with why, so no Invite is asked for that the Server would refuse without
//! the reader knowing why. With the listener off, or not listening where it
//! was asked to, the picker says why it lists no address, and with nothing at
//! all to offer, what would give it something.
//!
//! Preparing the picker and the Invite asked for each name themselves with a
//! [`ServeRequest`], and only the answer to the one awaited is taken: one the
//! reader has since moved past — the picker closed and opened again — lands
//! nowhere. The keys stay on the way they are on however the Relays move
//! around it, moving to its neighbour only where it goes itself.

use std::{cell::Cell, collections::HashSet};

use crate::protocol::{
    IssueInviteRequest, IssuedInvite, ListenerState, Peer, Relay, RelayState, Way,
};

use super::list_window::ListWindow;

/// What the picker says in place of the machine's addresses while the
/// Serving listener is off.
pub(super) const LISTENER_OFF: &str =
    "The Serving listener is off, so an Invite offers none of this machine's addresses";

/// What the picker says in place of the machine's addresses while the
/// Serving listener is asked to listen and does not, before why.
pub(super) const LISTENER_FAILED: &str = "The Serving listener is not listening, so an Invite offers none of this machine's \
     addresses: ";

/// What the picker says where the Serving listener is off and no Relay can be
/// offered either, so an Invite has no way at all to offer.
pub(super) const NO_WAY: &str = "An Invite has no way to offer: turn the Serving listener on in \
                                 the settings panel, or Serve through a Relay from /relay";

/// What the picker says where the Serving listener is not listening and no
/// Relay can be offered either.
pub(super) const NO_WAY_UNTIL_LISTENING: &str = "An Invite has no way to offer until the Serving listener listens, or this Server Serves \
     through a Relay from /relay";

/// What the ways an Invite may offer stand on, followed while the picker
/// stands: how the Serving listener stands and the Client's own Server's
/// Relays, each as the Server last pictured them.
#[derive(Clone, Copy, Debug)]
pub(super) struct ServeWays<'a> {
    pub(super) listener: &'a ListenerState,
    pub(super) relays: &'a [Relay],
}

impl<'a> ServeWays<'a> {
    pub(super) fn of(listener: &'a ListenerState, relays: &'a [Relay]) -> Self {
        Self { listener, relays }
    }
}

/// One request the picker sent the Client's own Server — to prepare Serving,
/// or to issue an Invite — told apart from every other so its answer reaches
/// only what asked for it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServeRequest(u64);

#[derive(Clone, Debug, Default)]
pub(super) struct ServeOverlay {
    state: ServeOverlayState,
    /// The window over whichever list the overlay stands on: the ways an
    /// Invite may offer, or the Peers it has enrolled.
    window: ListWindow,
    last_request: u64,
}

#[derive(Clone, Debug, Default)]
enum ServeOverlayState {
    #[default]
    Closed,
    Preparing {
        request: ServeRequest,
    },
    Issuing {
        request: ServeRequest,
        choice: Choice,
    },
    Choosing(Choice),
    Managing {
        invite: IssuedInvite,
        peers: Vec<Peer>,
        selected: usize,
        /// Whether the selected Peer's removal has been asked for once and
        /// awaits the second press that performs it.
        armed: bool,
        removing: Option<String>,
        error: Option<String>,
    },
}

/// What the reader has chosen of the ways an Invite may offer. Every way is
/// offered unless the reader has left it out, so a Relay that comes to be
/// offered while the picker stands is offered with the rest.
#[derive(Clone, Debug, Default)]
struct Choice {
    /// The machine's own addresses, as found when the picker opened, listed
    /// while the Serving listener listens.
    addresses: Vec<Way>,
    left_out: HashSet<Way>,
    /// The way the keys are on, once they have moved.
    focus: Option<Way>,
    /// Where among the ways the keys last stood, for them to stand by its
    /// neighbour where the way they were on goes.
    at: Cell<usize>,
    error: Option<String>,
}

impl Choice {
    /// Where among `candidates` the keys stand: on the way they are on,
    /// wherever that has moved, or — where it has gone — where it stood.
    fn focused(&self, candidates: &[CandidateWay]) -> usize {
        let index = self
            .focus
            .as_ref()
            .and_then(|focus| {
                candidates
                    .iter()
                    .position(|candidate| &candidate.way == focus)
            })
            .unwrap_or_else(|| self.at.get().min(candidates.len().saturating_sub(1)));
        self.at.set(index);
        index
    }

    /// Puts the keys on the way at `index` among `candidates`.
    fn focus_on(&mut self, candidates: &[CandidateWay], index: usize) {
        self.focus = candidates.get(index).map(|candidate| candidate.way.clone());
        self.at.set(index);
    }

    /// The ways the picker lists, the machine's addresses first — while the
    /// Serving listener listens, and at the port it listens on — and then the
    /// Server's Relays, each with whether the reader has it offered.
    fn candidates(&self, ways: ServeWays<'_>) -> Vec<CandidateWay> {
        let listening = match ways.listener {
            ListenerState::Open { address } => Some(address.port()),
            ListenerState::Off | ListenerState::Failed { .. } => None,
        };
        let addresses = listening
            .into_iter()
            .flat_map(|port| {
                self.addresses.iter().filter_map(move |way| match way {
                    Way::Direct(address) => {
                        let mut address = *address;
                        address.set_port(port);
                        Some(Way::Direct(address))
                    }
                    Way::Relay(_) => None,
                })
            })
            .map(|way| CandidateWay {
                chosen: !self.left_out.contains(&way),
                way,
                withheld: None,
            });
        let relays = ways.relays.iter().map(|relay| {
            let way = Way::Relay(relay.address.clone());
            let withheld = Withheld::of(relay);
            CandidateWay {
                chosen: withheld.is_none() && !self.left_out.contains(&way),
                way,
                withheld,
            }
        });
        addresses.chain(relays).collect()
    }
}

/// A way an Invite may offer, and whether the reader has it offered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CandidateWay {
    pub(super) way: Way,
    pub(super) chosen: bool,
    /// Why an Invite cannot offer it, where it cannot.
    pub(super) withheld: Option<Withheld>,
}

/// Why an Invite cannot offer a Relay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Withheld {
    /// The Server holds no Login there that stands.
    LoginNeeded,
    /// The Server does not Serve through it.
    NotServedThrough,
}

impl Withheld {
    fn of(relay: &Relay) -> Option<Self> {
        if relay.state == RelayState::LoginNeeded {
            Some(Self::LoginNeeded)
        } else if !relay.serve_through {
            Some(Self::NotServedThrough)
        } else {
            None
        }
    }

    /// What its row says of it.
    pub(super) fn brief(self) -> &'static str {
        match self {
            Self::LoginNeeded => "login needed",
            Self::NotServedThrough => "not Served through",
        }
    }

    /// What would have an Invite offer the Relay at `address`.
    fn in_full(self, address: &str) -> String {
        match self {
            Self::LoginNeeded => format!(
                "An Invite offers the Relay at {address} only once this Server is logged in \
                 there; log in from /relay"
            ),
            Self::NotServedThrough => format!(
                "An Invite offers the Relay at {address} only once this Server Serves through \
                 it; choose that from /relay"
            ),
        }
    }
}

impl ServeOverlay {
    /// Opens the picker, answering the preparation it asks for.
    pub(super) fn open(&mut self) -> ServeRequest {
        let request = self.issue();
        self.state = ServeOverlayState::Preparing { request };
        request
    }

    fn issue(&mut self) -> ServeRequest {
        self.last_request += 1;
        ServeRequest(self.last_request)
    }

    /// Whether the preparation `request` asked for is the one awaited.
    pub(super) fn awaits_preparation(&self, request: ServeRequest) -> bool {
        matches!(self.state, ServeOverlayState::Preparing { request: awaited } if awaited == request)
    }

    pub(super) fn close(&mut self) {
        self.state = ServeOverlayState::Closed;
    }

    pub(super) fn is_open(&self) -> bool {
        !matches!(self.state, ServeOverlayState::Closed)
    }

    pub(super) fn is_preparing(&self) -> bool {
        matches!(
            self.state,
            ServeOverlayState::Preparing { .. } | ServeOverlayState::Issuing { .. }
        )
    }

    /// Offers the machine's `addresses` the preparation `request` found —
    /// listed at the port the Serving listener listens on, whichever they
    /// were found at — where it is the one awaited.
    pub(super) fn load_candidates(&mut self, request: ServeRequest, mut addresses: Vec<Way>) {
        if !self.awaits_preparation(request) {
            return;
        }
        addresses.sort_unstable();
        addresses.dedup();
        self.state = ServeOverlayState::Choosing(Choice {
            addresses,
            ..Choice::default()
        });
        self.window.open();
    }

    pub(super) fn fail_preparation(&mut self, request: ServeRequest, error: String) {
        if !self.awaits_preparation(request) {
            return;
        }
        self.state = ServeOverlayState::Choosing(Choice {
            error: Some(error),
            ..Choice::default()
        });
        self.window.open();
    }

    /// The ways the picker lists, as `ways` stand now.
    pub(super) fn candidates(&self, ways: ServeWays<'_>) -> Vec<CandidateWay> {
        match &self.state {
            ServeOverlayState::Choosing(choice) => choice.candidates(ways),
            _ => Vec::new(),
        }
    }

    /// Why an Invite has no way at all to offer, as `ways` stand now, where
    /// it has none for want of the Serving listener: every Relay listed, if
    /// any, withheld, and the listener off or not listening.
    pub(super) fn nothing_to_offer(&self, ways: ServeWays<'_>) -> Option<&'static str> {
        let ServeOverlayState::Choosing(choice) = &self.state else {
            return None;
        };
        let offerable = choice
            .candidates(ways)
            .iter()
            .any(|candidate| candidate.withheld.is_none());
        if offerable {
            return None;
        }
        match ways.listener {
            ListenerState::Open { .. } => None,
            ListenerState::Off => Some(NO_WAY),
            ListenerState::Failed { .. } => Some(NO_WAY_UNTIL_LISTENING),
        }
    }

    /// Where among `candidates` — the ways listed — the keys stand.
    pub(super) fn focused(&self, candidates: &[CandidateWay]) -> usize {
        match &self.state {
            ServeOverlayState::Choosing(choice) => choice.focused(candidates),
            _ => 0,
        }
    }

    /// The Peer the keys are on.
    pub(super) fn selected(&self) -> usize {
        match &self.state {
            ServeOverlayState::Managing { selected, .. } => *selected,
            _ => 0,
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        match &self.state {
            ServeOverlayState::Choosing(choice) => choice.error.as_deref(),
            ServeOverlayState::Managing { error, .. } => error.as_deref(),
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing { .. }
            | ServeOverlayState::Issuing { .. } => None,
        }
    }

    pub(super) fn select_previous(&mut self, ways: ServeWays<'_>) {
        match &mut self.state {
            ServeOverlayState::Choosing(choice) => {
                let candidates = choice.candidates(ways);
                if !candidates.is_empty() {
                    let index = choice
                        .focused(&candidates)
                        .checked_sub(1)
                        .unwrap_or(candidates.len() - 1);
                    choice.focus_on(&candidates, index);
                    self.window.reveal();
                }
            }
            ServeOverlayState::Managing {
                peers, selected, ..
            } => {
                if !peers.is_empty() {
                    *selected = selected.checked_sub(1).unwrap_or_else(|| peers.len() - 1);
                    self.window.reveal();
                }
            }
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing { .. }
            | ServeOverlayState::Issuing { .. } => {}
        }
    }

    pub(super) fn select_next(&mut self, ways: ServeWays<'_>) {
        match &mut self.state {
            ServeOverlayState::Choosing(choice) => {
                let candidates = choice.candidates(ways);
                if !candidates.is_empty() {
                    let index = (choice.focused(&candidates) + 1) % candidates.len();
                    choice.focus_on(&candidates, index);
                    self.window.reveal();
                }
            }
            ServeOverlayState::Managing {
                peers, selected, ..
            } => {
                if !peers.is_empty() {
                    *selected = (*selected + 1) % peers.len();
                    self.window.reveal();
                }
            }
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing { .. }
            | ServeOverlayState::Issuing { .. } => {}
        }
    }

    /// Offers the way the keys are on where it is left out, and leaves it
    /// out where it is offered. A Relay an Invite cannot offer says instead
    /// what would have one offer it.
    pub(super) fn toggle_selected(&mut self, ways: ServeWays<'_>) {
        let ServeOverlayState::Choosing(choice) = &mut self.state else {
            return;
        };
        let candidates = choice.candidates(ways);
        let index = choice.focused(&candidates);
        choice.focus_on(&candidates, index);
        let Some(candidate) = candidates.get(index) else {
            return;
        };
        if let (Some(withheld), Way::Relay(address)) = (candidate.withheld, &candidate.way) {
            choice.error = Some(withheld.in_full(address));
            return;
        }
        if !choice.left_out.remove(&candidate.way) {
            choice.left_out.insert(candidate.way.clone());
        }
        choice.error = None;
    }

    /// Asks for an Invite offering exactly the ways chosen among those
    /// `ways` leave an Invite able to offer, answering the request that
    /// asks.
    pub(super) fn issue_request(
        &mut self,
        ways: ServeWays<'_>,
    ) -> Option<(ServeRequest, IssueInviteRequest)> {
        let nothing_to_offer = self.nothing_to_offer(ways);
        let ServeOverlayState::Choosing(choice) = &mut self.state else {
            return None;
        };
        let chosen = choice
            .candidates(ways)
            .into_iter()
            .filter(|candidate| candidate.chosen)
            .map(|candidate| candidate.way)
            .collect::<Vec<_>>();
        if chosen.is_empty() {
            // Where nothing at all can be offered, the picker says so already.
            if nothing_to_offer.is_none() {
                choice.error = Some("Choose at least one address".to_owned());
            }
            return None;
        }
        let choice = std::mem::take(choice);
        let request = self.issue();
        self.state = ServeOverlayState::Issuing { request, choice };
        Some((request, IssueInviteRequest { ways: chosen }))
    }

    /// Shows the Invite the Server issued for the request awaited.
    pub(super) fn show_invite(
        &mut self,
        request: ServeRequest,
        invite: IssuedInvite,
        peers: Vec<Peer>,
    ) {
        if !self.awaits(request) {
            return;
        }
        self.state = ServeOverlayState::Managing {
            invite,
            peers,
            selected: 0,
            armed: false,
            removing: None,
            error: None,
        };
        self.window.open();
    }

    /// Returns the picker to the ways as the reader chose them, with why the
    /// Server issued no Invite for the request awaited.
    pub(super) fn issuance_failed(&mut self, request: ServeRequest, error: String) {
        if !self.awaits(request) {
            return;
        }
        let ServeOverlayState::Issuing { mut choice, .. } = std::mem::take(&mut self.state) else {
            return;
        };
        choice.error = Some(error);
        self.state = ServeOverlayState::Choosing(choice);
    }

    fn awaits(&self, request: ServeRequest) -> bool {
        matches!(self.state, ServeOverlayState::Issuing { request: awaited, .. } if awaited == request)
    }

    pub(super) fn window(&self) -> &ListWindow {
        &self.window
    }

    pub(super) fn invite(&self) -> Option<&IssuedInvite> {
        match &self.state {
            ServeOverlayState::Managing { invite, .. } => Some(invite),
            _ => None,
        }
    }

    pub(super) fn peers(&self) -> &[Peer] {
        match &self.state {
            ServeOverlayState::Managing { peers, .. } => peers,
            _ => &[],
        }
    }

    pub(super) fn copy_text(&self) -> Option<String> {
        self.invite().map(|invite| invite.invite.clone())
    }

    /// Ending a Pairing is asked for twice, as it is for a Remote: the first
    /// press arms the selected Peer's removal and only the second performs it.
    pub(super) fn remove_selected(&mut self) -> Option<String> {
        let ServeOverlayState::Managing {
            peers,
            selected,
            armed,
            removing,
            error,
            ..
        } = &mut self.state
        else {
            return None;
        };
        if removing.is_some() {
            return None;
        }
        let peer_id = peers.get(*selected)?.id.clone();
        *error = None;
        if !*armed {
            *armed = true;
            return None;
        }
        *armed = false;
        *removing = Some(peer_id.clone());
        Some(peer_id)
    }

    /// Puts down an armed removal, which every key but the one that arms it
    /// does, answering whether there was one to put down.
    pub(super) fn disarm_removal(&mut self) -> bool {
        match &mut self.state {
            ServeOverlayState::Managing { armed, .. } => std::mem::take(armed),
            _ => false,
        }
    }

    pub(super) fn removal_armed(&self) -> bool {
        matches!(self.state, ServeOverlayState::Managing { armed: true, .. })
    }

    pub(super) fn peer_removed(&mut self, peer_id: &str) {
        let ServeOverlayState::Managing {
            peers,
            selected,
            removing,
            ..
        } = &mut self.state
        else {
            return;
        };
        peers.retain(|peer| peer.id != peer_id);
        *selected = (*selected).min(peers.len().saturating_sub(1));
        *removing = None;
    }

    /// Says why a Peer's removal failed, beside the Peers.
    pub(super) fn fail_operation(&mut self, error: String) {
        if let ServeOverlayState::Managing {
            armed,
            removing,
            error: message,
            ..
        } = &mut self.state
        {
            *armed = false;
            *removing = None;
            *message = Some(error);
        }
    }
}
