//! Correlating a Session attach with the reader's newest navigation.
//!
//! Attaching a Session is asynchronous: the client asks, and a snapshot and a
//! live subscription come back some time later. A reader does not wait for it.
//! They open one Session, change their mind and open another, go back to the
//! Landing, or move to a different Workspace — and every one of those is them
//! saying which Session they are on now.
//!
//! So attach work carries the Session it is for and the attempt that
//! started it, and only the answer to the question the reader is still asking
//! is allowed to land. Everything else is work they walked away from: its
//! snapshot must not become the open Session, its subscription must not be
//! adopted, and its failure must not be drawn at them.

use tokio::task::JoinHandle;

use crate::protocol::SessionReference;

/// One attempt to attach a Session, counted from the client's first and never
/// reused. It rides out with the work and comes back on both success and
/// failure, so a result names the attempt it belongs to rather than only the
/// Session it is for — which is what tells the reader's second attempt at a
/// Session apart from the first one they abandoned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AttachOperationId(u64);

/// Whether a finished attach is still the one the reader is waiting on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(super) enum AttachOutcome {
    /// The reader is still on this one: apply it, and adopt the subscription
    /// it carries.
    Current,
    /// The reader moved on before it answered. Nothing it carries may reach
    /// the client — not the snapshot, not the subscription, not the failure.
    Superseded,
}

impl AttachOutcome {
    pub(super) const fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }
}

/// The one Session attach the reader is waiting on, if any.
///
/// There is only ever one: navigating supersedes rather than queues, because
/// the reader's newest choice is the only one that can still be right. The
/// attach already in flight is let go of the moment a newer one starts,
/// and a pending attach is let go of outright when the client leaves for
/// the Landing, another Workspace, or another Outlook, or when it shuts down.
#[derive(Default)]
pub(super) struct SessionAttach {
    /// How many attaches this client has started, which is where the next
    /// attempt's identity comes from.
    operations: u64,
    pending: Option<PendingAttach>,
}

/// Attach work in flight, and the identity its result has to match.
struct PendingAttach {
    operation: AttachOperationId,
    task: JoinHandle<()>,
}

/// Letting go of attach work stops it. Dropping the coordinator — which
/// is what leaving the run loop does — is the same act as abandoning it, so
/// shutdown needs no separate path to keep a late result from landing.
impl Drop for PendingAttach {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SessionAttach {
    /// Starts attaching `target`, superseding whatever was already in flight.
    ///
    /// `spawn` is handed the operation identity to carry back with its result,
    /// so the work reports which navigation it was doing rather than only
    /// which Session it was for.
    pub(super) fn begin(
        &mut self,
        target: SessionReference,
        spawn: impl FnOnce(SessionReference, AttachOperationId) -> JoinHandle<()>,
    ) {
        self.operations += 1;
        let operation = AttachOperationId(self.operations);
        let task = spawn(target, operation);
        self.pending = Some(PendingAttach { operation, task });
    }

    /// Lets go of a pending attach, stopping the work and invalidating its
    /// result. What the reader is on is theirs to say, and they have said it.
    pub(super) fn abandon(&mut self) {
        self.pending = None;
    }

    /// Answers what a finished attach may do, and forgets it once it is
    /// the one that was being waited on.
    ///
    /// The attempt's identity is the whole of the correlation: it is never
    /// reused, so it already says which Session the work was for and which of
    /// the reader's choices asked for it.
    pub(super) fn settle(&mut self, operation: AttachOperationId) -> AttachOutcome {
        if self.pending.as_ref().map(|pending| pending.operation) != Some(operation) {
            return AttachOutcome::Superseded;
        }
        // Letting go of finished work stops nothing: the task's last act was
        // reporting this result, so the abort its drop asks for is a no-op.
        self.pending = None;
        AttachOutcome::Current
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    use super::{AttachOperationId, AttachOutcome, SessionAttach};
    use crate::protocol::{Outlook, SessionId, SessionReference};

    fn session() -> SessionReference {
        SessionReference::new(Outlook::Local, SessionId::new())
    }

    /// Attach work that never answers by itself, so the only way a test
    /// sees it end is something stopping it. Dropping the parked future — what
    /// aborting the task does — reports the attempt that was let go of.
    struct ParkedAttaches {
        stopped: UnboundedSender<AttachOperationId>,
        letting_go: UnboundedReceiver<AttachOperationId>,
    }

    /// Reports its attempt when the work holding it is dropped.
    struct StopWitness {
        stopped: UnboundedSender<AttachOperationId>,
        operation: AttachOperationId,
    }

    impl Drop for StopWitness {
        fn drop(&mut self) {
            let _ = self.stopped.send(self.operation);
        }
    }

    impl ParkedAttaches {
        fn new() -> Self {
            let (stopped, letting_go) = unbounded_channel();
            Self {
                stopped,
                letting_go,
            }
        }

        /// Starts an attach to `target` whose work parks forever, and
        /// answers with the attempt identity the coordinator gave it — which
        /// is otherwise only ever seen by the work itself.
        fn begin(
            &self,
            attach: &mut SessionAttach,
            target: &SessionReference,
        ) -> AttachOperationId {
            let mut started = None;
            attach.begin(target.clone(), |_, operation| {
                started = Some(operation);
                let witness = StopWitness {
                    stopped: self.stopped.clone(),
                    operation,
                };
                tokio::spawn(async move {
                    let _witness = witness;
                    std::future::pending::<()>().await;
                })
            });
            started.expect("beginning an attach starts its work")
        }

        /// Waits for the next attach whose work was stopped. Parked work
        /// only ends this way, so nothing here waits on elapsed time.
        async fn stopped(&mut self) -> AttachOperationId {
            self.letting_go
                .recv()
                .await
                .expect("the coordinator outlives the work it started")
        }
    }

    /// Only the newest attempt may land, which is what decides whose snapshot
    /// becomes the open Session and whose subscription the run loop adopts:
    /// B's answer is refused where C's is taken, however they are ordered.
    #[tokio::test]
    async fn opening_a_newer_session_supersedes_the_attach_already_in_flight() {
        let mut attach = SessionAttach::default();
        let mut parked = ParkedAttaches::new();
        let (b, c) = (session(), session());

        let attaching_b = parked.begin(&mut attach, &b);
        let attaching_c = parked.begin(&mut attach, &c);

        assert_eq!(
            parked.stopped().await,
            attaching_b,
            "opening C left B's attach running"
        );
        assert_eq!(
            attach.settle(attaching_b),
            AttachOutcome::Superseded,
            "B's snapshot and subscription arrived after the reader opened C"
        );
        assert_eq!(
            attach.settle(attaching_c),
            AttachOutcome::Current,
            "refusing B's answer cost C the attach the reader is waiting on"
        );
    }

    /// Opening the Landing and moving to another Workspace both leave the
    /// Session behind, and neither leaves anything for an attach to land
    /// on.
    #[tokio::test]
    async fn leaving_the_session_behind_abandons_the_attach_in_flight() {
        let mut attach = SessionAttach::default();
        let mut parked = ParkedAttaches::new();

        let attaching_b = parked.begin(&mut attach, &session());
        attach.abandon();

        assert_eq!(
            parked.stopped().await,
            attaching_b,
            "leaving the Session behind left its attach running"
        );
        assert_eq!(attach.settle(attaching_b), AttachOutcome::Superseded);
    }

    /// Nothing is left running once the client is gone, so a result cannot
    /// outlive the run loop that would have applied it.
    #[tokio::test]
    async fn shutting_down_stops_the_attach_in_flight() {
        let mut attach = SessionAttach::default();
        let mut parked = ParkedAttaches::new();

        let attaching = parked.begin(&mut attach, &session());
        drop(attach);

        assert_eq!(parked.stopped().await, attaching);
    }

    /// Retrying a Session is a second attempt at it rather than the same one
    /// again, so the first attempt's answer is no answer to the second — which
    /// is why the Session's own identity cannot be the correlation.
    #[tokio::test]
    async fn a_second_attempt_at_a_session_refuses_the_first_attempts_result() {
        let mut attach = SessionAttach::default();
        let parked = ParkedAttaches::new();
        let b = session();

        let first = parked.begin(&mut attach, &b);
        let second = parked.begin(&mut attach, &b);

        assert_eq!(attach.settle(first), AttachOutcome::Superseded);
        assert_eq!(attach.settle(second), AttachOutcome::Current);
    }

    /// A stale failure is refused the same way a stale success is, and refusing
    /// it leaves the attach the reader is waiting on still pending — the
    /// error belongs to work they walked away from.
    #[tokio::test]
    async fn a_stale_failure_is_refused_and_leaves_the_newer_attach_pending() {
        let mut attach = SessionAttach::default();
        let parked = ParkedAttaches::new();
        let (b, c) = (session(), session());

        let attaching_b = parked.begin(&mut attach, &b);
        let attaching_c = parked.begin(&mut attach, &c);

        assert_eq!(
            attach.settle(attaching_b),
            AttachOutcome::Superseded,
            "B's failure landed after the reader opened C"
        );
        assert_eq!(
            attach.settle(attaching_c),
            AttachOutcome::Current,
            "refusing B's failure dropped the attach to C"
        );
    }
}
