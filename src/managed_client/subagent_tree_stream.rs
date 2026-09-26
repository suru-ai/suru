//! The per-tree Subagent stream: a snapshot of the tree a top-level Session
//! heads, then every change to it, reconnecting on its own.

use anyhow::{Context, Result, bail};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Duration,
};

use crate::protocol::{
    Outlook, RuntimeDescriptor, SUBAGENT_TREE_SNAPSHOT_EVENT, SUBAGENT_TREE_UPDATED_EVENT,
    SessionId, SubagentTreeChange, SubagentTreeRevision, SubagentTreeSnapshot, SubagentTreeUpdate,
};

use super::{
    RecoveryBackoff,
    remote_connection::{self, RemoteConnectionFailure},
    server_url,
};

/// What a per-tree subscription tells its reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubagentTreeEvent {
    /// The whole tree, naming its top-level Session. It arrives first and
    /// again after every reconnection, and replaces whatever tree the reader
    /// held — so a change missed while disconnected is already in it.
    Snapshot(SubagentTreeSnapshot),
    /// One change to the tree the latest snapshot named, in the order the
    /// Server made them. The tree's deletion never arrives as a change: it
    /// arrives as [`Self::Deleted`].
    Changed(SubagentTreeChange),
    /// The top-level Session heading the tree was deleted, and every Session
    /// in the tree with it — whether the Server said so on the live stream or
    /// the subscription found the tree gone when it reconnected. The
    /// subscription has ended.
    Deleted,
    /// The tree cannot be read — its Server holds no such Session, or the
    /// Remote serving it ended the Pairing — and the subscription has ended.
    Failed(String),
}

/// A reconnecting subscription to the tree one Session belongs to, on the
/// Server one Outlook names. Its lifetime is the interest: dropping it aborts
/// the task and releases the connection.
pub struct SubagentTreeSubscription {
    events: mpsc::Receiver<SubagentTreeEvent>,
    task: JoinHandle<()>,
}

impl SubagentTreeSubscription {
    pub(super) fn open(
        http: reqwest::Client,
        descriptor: watch::Receiver<RuntimeDescriptor>,
        outlook: Outlook,
        session_id: SessionId,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(32);
        let task = tokio::spawn(run(
            http,
            descriptor,
            outlook,
            session_id,
            events_tx,
            RecoveryBackoff::new(initial_backoff, max_backoff),
        ));
        Self {
            events: events_rx,
            task,
        }
    }

    /// The next event, or `None` once the subscription has ended.
    pub async fn next(&mut self) -> Option<SubagentTreeEvent> {
        self.events.recv().await
    }
}

impl Drop for SubagentTreeSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    http: reqwest::Client,
    mut descriptor: watch::Receiver<RuntimeDescriptor>,
    outlook: Outlook,
    session_id: SessionId,
    events: mpsc::Sender<SubagentTreeEvent>,
    mut backoff: RecoveryBackoff,
) {
    // Whether any connection has delivered the tree. Once one has, the tree
    // missing on a later connection was deleted while this one was away;
    // missing from the first, it was never there to subscribe to.
    let mut heard = false;
    loop {
        if events.is_closed() {
            return;
        }
        let active_descriptor = descriptor.borrow().clone();
        match open(&http, &active_descriptor, &outlook, session_id).await {
            Ok(response) => match consume(response, &events).await {
                StreamOutcome::Ended => return,
                StreamOutcome::Disconnected { hydrated } => {
                    if hydrated {
                        heard = true;
                        backoff.reset();
                    }
                }
            },
            Err(RemoteConnectionFailure::Missing(_)) if heard => {
                let _ = events.send(SubagentTreeEvent::Deleted).await;
                return;
            }
            Err(
                RemoteConnectionFailure::Terminal { message, .. }
                | RemoteConnectionFailure::Rejected(message)
                | RemoteConnectionFailure::Missing(message),
            ) => {
                let _ = events.send(SubagentTreeEvent::Failed(message)).await;
                return;
            }
            Err(RemoteConnectionFailure::Transient) => {}
        }
        if wait_to_reconnect(&mut descriptor, backoff.next()).await {
            backoff.reset();
        }
    }
}

async fn open(
    http: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    outlook: &Outlook,
    session_id: SessionId,
) -> std::result::Result<reqwest::Response, RemoteConnectionFailure> {
    let url = server_url(
        &descriptor.base_url,
        outlook,
        &format!("/v1/sessions/{session_id}/subagent-tree"),
    )
    .expect("a validated runtime descriptor builds a Server URL");
    remote_connection::classify(http.get(url).bearer_auth(&descriptor.token).send().await).await
}

/// Waits for the retry delay or a replacement local Server descriptor,
/// answering whether the descriptor was replaced: a new one names a new
/// Server, which starts a fresh retry schedule.
async fn wait_to_reconnect(
    descriptor: &mut watch::Receiver<RuntimeDescriptor>,
    retry_in: Duration,
) -> bool {
    tokio::select! {
        changed = descriptor.changed(), if descriptor.has_changed().is_ok() => changed.is_ok(),
        _ = tokio::time::sleep(retry_in) => false,
    }
}

enum StreamOutcome {
    /// The connection ended, cleanly or not, or said something that cannot
    /// be trusted; a fresh connection's snapshot sets it right. `hydrated`
    /// says whether a snapshot arrived first.
    Disconnected { hydrated: bool },
    /// The subscription is over: its reader went away, or the tree was
    /// deleted and the reader has been told.
    Ended,
}

async fn consume(
    response: reqwest::Response,
    events: &mpsc::Sender<SubagentTreeEvent>,
) -> StreamOutcome {
    let mut stream = response.bytes_stream().eventsource();
    let mut revision = None;
    while let Some(next) = stream.next().await {
        let hydrated = revision.is_some();
        let event = match next {
            Ok(event) => event,
            Err(EventStreamError::Transport(_)) => return StreamOutcome::Disconnected { hydrated },
            Err(error) => {
                tracing::warn!(%error, "Subagent tree stream is unreadable");
                return StreamOutcome::Disconnected { hydrated };
            }
        };
        let event = match decode_event(event, &mut revision) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "Subagent tree stream broke its protocol");
                return StreamOutcome::Disconnected { hydrated };
            }
        };
        let deleted = event == SubagentTreeEvent::Deleted;
        if events.send(event).await.is_err() || deleted {
            return StreamOutcome::Ended;
        }
    }
    StreamOutcome::Disconnected {
        hydrated: revision.is_some(),
    }
}

fn decode_event(
    event: eventsource_stream::Event,
    revision: &mut Option<SubagentTreeRevision>,
) -> Result<SubagentTreeEvent> {
    match event.event.as_str() {
        SUBAGENT_TREE_SNAPSHOT_EVENT => {
            if revision.is_some() {
                bail!("Subagent tree stream sent more than one snapshot");
            }
            let snapshot: SubagentTreeSnapshot =
                serde_json::from_str(&event.data).context("decode Subagent tree snapshot")?;
            validate_event_id(&event.id, snapshot.revision)?;
            *revision = Some(snapshot.revision);
            Ok(SubagentTreeEvent::Snapshot(snapshot))
        }
        SUBAGENT_TREE_UPDATED_EVENT => {
            let Some(previous) = *revision else {
                bail!("Subagent tree stream sent an update before its snapshot");
            };
            let update: SubagentTreeUpdate =
                serde_json::from_str(&event.data).context("decode Subagent tree update")?;
            validate_event_id(&event.id, update.revision)?;
            if !update.revision.immediately_follows(previous) {
                bail!("Subagent tree revision sequence is discontinuous");
            }
            *revision = Some(update.revision);
            Ok(match update.change {
                SubagentTreeChange::TreeDeleted => SubagentTreeEvent::Deleted,
                change => SubagentTreeEvent::Changed(change),
            })
        }
        name => bail!("server sent unknown Subagent tree event type '{name}'"),
    }
}

fn validate_event_id(id: &str, revision: SubagentTreeRevision) -> Result<()> {
    if id == revision.0.to_string() {
        Ok(())
    } else {
        bail!("Subagent tree event id does not match its revision")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ActivityStatus, SubagentTreeTopLevel};

    fn event(name: &str, id: u64, data: &impl serde::Serialize) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: name.to_owned(),
            data: serde_json::to_string(data).expect("encode event data"),
            id: id.to_string(),
            retry: None,
        }
    }

    fn snapshot(revision: u64) -> SubagentTreeSnapshot {
        SubagentTreeSnapshot {
            revision: SubagentTreeRevision(revision),
            top_level: SubagentTreeTopLevel {
                session_id: SessionId::new(),
                title: "Delegate".to_owned(),
                working_since: Some(crate::protocol::SessionTimestamp(500)),
                needs_intervention: false,
            },
            subagents: Vec::new(),
        }
    }

    fn settle(revision: u64) -> SubagentTreeUpdate {
        SubagentTreeUpdate {
            revision: SubagentTreeRevision(revision),
            change: SubagentTreeChange::SubagentWorkingChanged {
                session_id: SessionId::new(),
                status: ActivityStatus::Completed,
                worked_ms: Some(5),
                working_since: None,
            },
        }
    }

    #[test]
    fn changes_follow_their_snapshot_without_a_gap() {
        let mut revision = None;
        let opened = snapshot(4);
        assert_eq!(
            decode_event(
                event(SUBAGENT_TREE_SNAPSHOT_EVENT, 4, &opened),
                &mut revision
            )
            .expect("the snapshot opens the stream"),
            SubagentTreeEvent::Snapshot(opened)
        );
        let next = settle(5);
        assert_eq!(
            decode_event(event(SUBAGENT_TREE_UPDATED_EVENT, 5, &next), &mut revision)
                .expect("the next revision follows"),
            SubagentTreeEvent::Changed(next.change)
        );
        assert!(
            decode_event(
                event(SUBAGENT_TREE_UPDATED_EVENT, 7, &settle(7)),
                &mut revision
            )
            .is_err(),
            "a skipped revision is a missed change, which only a fresh snapshot recovers"
        );
    }

    #[test]
    fn the_trees_deletion_is_its_own_event_rather_than_a_change() {
        let mut revision = None;
        decode_event(
            event(SUBAGENT_TREE_SNAPSHOT_EVENT, 1, &snapshot(1)),
            &mut revision,
        )
        .expect("the snapshot opens the stream");
        let deleted = SubagentTreeUpdate {
            revision: SubagentTreeRevision(2),
            change: SubagentTreeChange::TreeDeleted,
        };
        assert_eq!(
            decode_event(
                event(SUBAGENT_TREE_UPDATED_EVENT, 2, &deleted),
                &mut revision
            )
            .expect("the deletion follows in sequence"),
            SubagentTreeEvent::Deleted
        );
    }

    /// A stand-in Server for the per-tree route, answering each connection
    /// with the next of its scripted answers and counting how many it has had.
    mod fixture {
        use std::{
            convert::Infallible,
            sync::{
                Arc, Mutex,
                atomic::{AtomicUsize, Ordering},
            },
        };

        use axum::{
            Json, Router,
            extract::State,
            http::StatusCode,
            response::{IntoResponse, Response, Sse, sse::Event},
            routing::get,
        };
        use futures_util::stream;
        use tokio::sync::watch;

        use crate::protocol::{
            PROTOCOL_VERSION, RuntimeDescriptor, SUBAGENT_TREE_SNAPSHOT_EVENT,
            SUBAGENT_TREE_UPDATED_EVENT, ServerIdentity, SessionError, SessionErrorCode,
            SubagentTreeSnapshot, SubagentTreeUpdate,
        };

        /// One connection's answer.
        pub(super) enum Answer {
            /// The snapshot and changes, after which the Server ends the
            /// stream, as a Server shutting down or dropping a lagging
            /// subscriber does.
            Ends(SubagentTreeSnapshot, Vec<SubagentTreeUpdate>),
            /// The snapshot and changes, after which the stream stays open.
            Stays(SubagentTreeSnapshot, Vec<SubagentTreeUpdate>),
            /// The refusal a Server holding no such Session gives.
            NotFound,
        }

        struct Script {
            answers: Mutex<Vec<Answer>>,
            connections: AtomicUsize,
        }

        pub(super) struct TreeServer {
            script: Arc<Script>,
            pub(super) descriptor: watch::Receiver<RuntimeDescriptor>,
            _descriptor: watch::Sender<RuntimeDescriptor>,
            task: tokio::task::JoinHandle<()>,
        }

        impl TreeServer {
            pub(super) async fn spawn(answers: Vec<Answer>) -> Self {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .expect("bind the tree fixture");
                let address = listener.local_addr().expect("read the fixture address");
                let script = Arc::new(Script {
                    answers: Mutex::new(answers.into_iter().rev().collect()),
                    connections: AtomicUsize::new(0),
                });
                let app = Router::new()
                    .route("/v1/sessions/{session_id}/subagent-tree", get(answer))
                    .with_state(script.clone());
                let task = tokio::spawn(async move {
                    axum::serve(listener, app)
                        .await
                        .expect("serve the tree fixture");
                });
                let (sender, descriptor) = watch::channel(RuntimeDescriptor::new(
                    format!("http://{address}"),
                    "tree-fixture-token".to_owned(),
                    ServerIdentity {
                        instance_id: uuid::Uuid::new_v4(),
                        pid: std::process::id(),
                        protocol_version: PROTOCOL_VERSION,
                        build_identity: "tree-fixture".to_owned(),
                    },
                ));
                Self {
                    script,
                    descriptor,
                    _descriptor: sender,
                    task,
                }
            }

            pub(super) fn connections(&self) -> usize {
                self.script.connections.load(Ordering::SeqCst)
            }
        }

        impl Drop for TreeServer {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        async fn answer(State(script): State<Arc<Script>>) -> Response {
            script.connections.fetch_add(1, Ordering::SeqCst);
            let next = script
                .answers
                .lock()
                .expect("the script lock is not poisoned")
                .pop();
            let (snapshot, updates, stays) = match next {
                Some(Answer::Ends(snapshot, updates)) => (snapshot, updates, false),
                Some(Answer::Stays(snapshot, updates)) => (snapshot, updates, true),
                Some(Answer::NotFound) | None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(SessionError {
                            code: SessionErrorCode::SessionNotFound,
                            message: "Session does not exist on this server instance".to_owned(),
                        }),
                    )
                        .into_response();
                }
            };
            let mut events = vec![
                Event::default()
                    .event(SUBAGENT_TREE_SNAPSHOT_EVENT)
                    .id(snapshot.revision.0.to_string())
                    .json_data(snapshot)
                    .expect("encode the fixture snapshot"),
            ];
            events.extend(updates.into_iter().map(|update| {
                Event::default()
                    .event(SUBAGENT_TREE_UPDATED_EVENT)
                    .id(update.revision.0.to_string())
                    .json_data(update)
                    .expect("encode the fixture update")
            }));
            let events = stream::iter(events.into_iter().map(Ok::<_, Infallible>));
            if stays {
                Sse::new(futures_util::StreamExt::chain(events, stream::pending())).into_response()
            } else {
                Sse::new(events).into_response()
            }
        }
    }

    use fixture::{Answer, TreeServer};

    const DEADLINE: Duration = Duration::from_secs(10);

    fn subscribe(server: &TreeServer) -> SubagentTreeSubscription {
        SubagentTreeSubscription::open(
            reqwest::Client::new(),
            server.descriptor.clone(),
            Outlook::Local,
            SessionId::new(),
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
    }

    async fn next_event(subscription: &mut SubagentTreeSubscription) -> Option<SubagentTreeEvent> {
        tokio::time::timeout(DEADLINE, subscription.next())
            .await
            .expect("the subscription answers in time")
    }

    /// A tree with one working Subagent in it.
    fn working_tree(revision: u64) -> (SubagentTreeSnapshot, SessionId) {
        let mut tree = snapshot(revision);
        let subagent = SessionId::new();
        tree.subagents.push(crate::protocol::SubagentTreeEntry {
            session_id: subagent,
            parent_session_id: tree.top_level.session_id,
            spawn_order: 0,
            name: "Explore".to_owned(),
            title: "Map the seams".to_owned(),
            status: ActivityStatus::Active,
            worked_ms: Some(0),
            working_since: Some(crate::protocol::SessionTimestamp(1_000)),
            needs_intervention: false,
        });
        (tree, subagent)
    }

    #[tokio::test]
    async fn a_reconnection_recovers_a_settle_missed_while_disconnected() {
        let (before, _) = working_tree(1);
        let mut after = before.clone();
        // A fresh connection counts its revisions afresh, and its snapshot is
        // authoritative whatever it says.
        after.revision = SubagentTreeRevision::INITIAL;
        after.subagents[0].status = ActivityStatus::Completed;
        after.subagents[0].worked_ms = Some(40);
        after.subagents[0].working_since = None;
        let server = TreeServer::spawn(vec![
            Answer::Ends(before.clone(), Vec::new()),
            Answer::Stays(after.clone(), Vec::new()),
        ])
        .await;
        let mut subscription = subscribe(&server);

        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(before))
        );
        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(after)),
            "the connection lost, the subscription reconnects on its own and its fresh \
             snapshot carries the settle it missed"
        );
        assert_eq!(server.connections(), 2);
    }

    #[tokio::test]
    async fn a_gap_in_the_changes_is_answered_by_a_fresh_snapshot_rather_than_passed_on() {
        let (before, subagent) = working_tree(1);
        let mut after = before.clone();
        after.subagents[0].status = ActivityStatus::Failed;
        after.subagents[0].worked_ms = None;
        after.subagents[0].working_since = None;
        let skipped = SubagentTreeUpdate {
            revision: SubagentTreeRevision(3),
            change: SubagentTreeChange::SubagentWorkingChanged {
                session_id: subagent,
                status: ActivityStatus::Failed,
                worked_ms: None,
                working_since: None,
            },
        };
        let server = TreeServer::spawn(vec![
            Answer::Stays(before.clone(), vec![skipped]),
            Answer::Stays(after.clone(), Vec::new()),
        ])
        .await;
        let mut subscription = subscribe(&server);

        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(before))
        );
        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(after)),
            "a change after a missing one is not trusted; the reconnection's snapshot is"
        );
    }

    #[tokio::test]
    async fn a_tree_deleted_on_the_live_stream_ends_the_subscription_without_reconnecting() {
        let (tree, _) = working_tree(1);
        let deleted = SubagentTreeUpdate {
            revision: SubagentTreeRevision(2),
            change: SubagentTreeChange::TreeDeleted,
        };
        let server = TreeServer::spawn(vec![Answer::Stays(tree.clone(), vec![deleted])]).await;
        let mut subscription = subscribe(&server);

        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(tree))
        );
        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Deleted)
        );
        assert_eq!(
            next_event(&mut subscription).await,
            None,
            "the deletion is the subscription's last word"
        );
        assert_eq!(
            server.connections(),
            1,
            "nothing reconnects to a deleted tree"
        );
    }

    #[tokio::test]
    async fn a_tree_deleted_while_disconnected_ends_the_subscription_as_deleted() {
        let (tree, _) = working_tree(1);
        let server = TreeServer::spawn(vec![
            Answer::Ends(tree.clone(), Vec::new()),
            Answer::NotFound,
        ])
        .await;
        let mut subscription = subscribe(&server);

        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Snapshot(tree))
        );
        assert_eq!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Deleted),
            "a tree once heard and then not found was deleted while the subscription was away"
        );
        assert_eq!(next_event(&mut subscription).await, None);
        assert_eq!(server.connections(), 2);
    }

    #[tokio::test]
    async fn a_tree_never_found_fails_rather_than_reading_as_deleted() {
        let server = TreeServer::spawn(vec![Answer::NotFound]).await;
        let mut subscription = subscribe(&server);

        assert!(matches!(
            next_event(&mut subscription).await,
            Some(SubagentTreeEvent::Failed(_))
        ));
        assert_eq!(next_event(&mut subscription).await, None);
        assert_eq!(server.connections(), 1);
    }

    #[test]
    fn a_change_before_any_snapshot_is_refused() {
        assert!(
            decode_event(event(SUBAGENT_TREE_UPDATED_EVENT, 2, &settle(2)), &mut None).is_err()
        );
    }
}
