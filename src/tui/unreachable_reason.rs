//! What the Client says of why a Remote cannot be reached, where its user can
//! do something about it rather than wait: in brief, on a line that names the
//! Remote, and in full, with what to do, wherever there is room for it. Each
//! names what is in the way first, so a line cut short still says it. A loss
//! for any other reason says no more than that the Remote is Unreachable.

use crate::protocol::UnreachableReason;

/// `reason` in brief.
pub(super) fn brief(reason: &UnreachableReason) -> String {
    match reason {
        UnreachableReason::RelayCapReached { limit, .. } => {
            let connections = if *limit == 1 {
                "connection"
            } else {
                "connections"
            };
            format!("Relay cap reached: {limit} joined {connections}")
        }
    }
}

/// `reason` in full, with what its user can do about it.
pub(super) fn in_full(reason: &UnreachableReason) -> String {
    match reason {
        UnreachableReason::RelayCapReached { relay, limit } => {
            let connections = if *limit == 1 {
                "connection"
            } else {
                "connections"
            };
            format!(
                "Your Account has reached the Relay's cap of {limit} {connections} joined at \
                 once at {relay}, so it joins no more until one ends; ask the Relay's operator \
                 to raise the cap if that is too few"
            )
        }
    }
}
