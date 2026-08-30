//! View state for the Serving user's address picker and Invite manager.

use std::net::SocketAddr;

use crate::protocol::{IssueInviteRequest, IssuedInvite, Peer};

#[derive(Clone, Debug, Default)]
pub(super) struct ServeOverlay {
    state: ServeOverlayState,
}

#[derive(Clone, Debug, Default)]
enum ServeOverlayState {
    #[default]
    Closed,
    Preparing,
    Issuing,
    Choosing {
        candidates: Vec<CandidateAddress>,
        selected: usize,
        error: Option<String>,
    },
    Managing {
        invite: IssuedInvite,
        peers: Vec<Peer>,
        selected: usize,
        removing: Option<String>,
        error: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CandidateAddress {
    pub(super) address: SocketAddr,
    pub(super) chosen: bool,
}

impl ServeOverlay {
    pub(super) fn open(&mut self) {
        self.state = ServeOverlayState::Preparing;
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
            ServeOverlayState::Preparing | ServeOverlayState::Issuing
        )
    }

    pub(super) fn load_candidates(&mut self, mut candidates: Vec<SocketAddr>) {
        candidates.sort_unstable();
        candidates.dedup();
        self.state = ServeOverlayState::Choosing {
            candidates: candidates
                .into_iter()
                .map(|address| CandidateAddress {
                    address,
                    chosen: true,
                })
                .collect(),
            selected: 0,
            error: None,
        };
    }

    pub(super) fn fail_preparation(&mut self, error: String) {
        self.state = ServeOverlayState::Choosing {
            candidates: Vec::new(),
            selected: 0,
            error: Some(error),
        };
    }

    pub(super) fn candidates(&self) -> &[CandidateAddress] {
        match &self.state {
            ServeOverlayState::Choosing { candidates, .. } => candidates,
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing
            | ServeOverlayState::Issuing
            | ServeOverlayState::Managing { .. } => &[],
        }
    }

    pub(super) fn selected(&self) -> usize {
        match self.state {
            ServeOverlayState::Choosing { selected, .. } => selected,
            ServeOverlayState::Managing { selected, .. } => selected,
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing
            | ServeOverlayState::Issuing => 0,
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        match &self.state {
            ServeOverlayState::Choosing { error, .. } => error.as_deref(),
            ServeOverlayState::Managing { error, .. } => error.as_deref(),
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing
            | ServeOverlayState::Issuing => None,
        }
    }

    pub(super) fn select_previous(&mut self) {
        match &mut self.state {
            ServeOverlayState::Choosing {
                candidates,
                selected,
                ..
            } => {
                if !candidates.is_empty() {
                    *selected = selected
                        .checked_sub(1)
                        .unwrap_or_else(|| candidates.len() - 1);
                }
            }
            ServeOverlayState::Managing {
                peers, selected, ..
            } => {
                if !peers.is_empty() {
                    *selected = selected.checked_sub(1).unwrap_or_else(|| peers.len() - 1);
                }
            }
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing
            | ServeOverlayState::Issuing => {}
        }
    }

    pub(super) fn select_next(&mut self) {
        match &mut self.state {
            ServeOverlayState::Choosing {
                candidates,
                selected,
                ..
            } => {
                if !candidates.is_empty() {
                    *selected = (*selected + 1) % candidates.len();
                }
            }
            ServeOverlayState::Managing {
                peers, selected, ..
            } => {
                if !peers.is_empty() {
                    *selected = (*selected + 1) % peers.len();
                }
            }
            ServeOverlayState::Closed
            | ServeOverlayState::Preparing
            | ServeOverlayState::Issuing => {}
        }
    }

    pub(super) fn toggle_selected(&mut self) {
        let ServeOverlayState::Choosing {
            candidates,
            selected,
            error,
        } = &mut self.state
        else {
            return;
        };
        if let Some(candidate) = candidates.get_mut(*selected) {
            candidate.chosen = !candidate.chosen;
            *error = None;
        }
    }

    pub(super) fn issue_request(&mut self) -> Option<IssueInviteRequest> {
        let ServeOverlayState::Choosing {
            candidates, error, ..
        } = &mut self.state
        else {
            return None;
        };
        let addresses = candidates
            .iter()
            .filter(|candidate| candidate.chosen)
            .map(|candidate| candidate.address)
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            *error = Some("Choose at least one address".to_owned());
            return None;
        }
        self.state = ServeOverlayState::Issuing;
        Some(IssueInviteRequest { addresses })
    }

    pub(super) fn show_invite(&mut self, invite: IssuedInvite, peers: Vec<Peer>) {
        self.state = ServeOverlayState::Managing {
            invite,
            peers,
            selected: 0,
            removing: None,
            error: None,
        };
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

    pub(super) fn remove_selected(&mut self) -> Option<String> {
        let ServeOverlayState::Managing {
            peers,
            selected,
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
        *removing = Some(peer_id.clone());
        *error = None;
        Some(peer_id)
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

    pub(super) fn fail_operation(&mut self, error: String) {
        match &mut self.state {
            ServeOverlayState::Managing {
                removing,
                error: message,
                ..
            } => {
                *removing = None;
                *message = Some(error);
            }
            _ => self.fail_preparation(error),
        }
    }
}
