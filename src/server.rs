use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::{get, post},
};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::RuntimeConfig;
use crate::build_identity;
use crate::protocol::{
    COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, LifecycleState, PROTOCOL_VERSION,
    RuntimeDescriptor, SERVER_SHUTDOWN_EVENT, SNAPSHOT_EVENT, ServerIdentity, ServerShutdown,
    ShutdownReason,
};
use crate::runtime::protect_current_user_file;

pub type ServerConfig = RuntimeConfig;

pub struct RunningServer {
    descriptor: RuntimeDescriptor,
    shutdown: ShutdownController,
    task: JoinHandle<Result<()>>,
}

impl RunningServer {
    pub fn descriptor(&self) -> &RuntimeDescriptor {
        &self.descriptor
    }

    pub async fn shutdown(self) -> Result<()> {
        self.request_shutdown();
        self.task.await.context("server task panicked")?
    }

    pub async fn run_until_ctrl_c(mut self) -> Result<()> {
        tokio::select! {
            task = &mut self.task => task.context("server task panicked")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                self.request_shutdown();
                self.task.await.context("server task panicked")?
            }
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.request(ServerShutdown {
            instance_id: self.descriptor.identity.instance_id,
            reason: ShutdownReason::Manual,
        });
    }
}

#[derive(Clone)]
struct ShutdownController {
    lifecycle: watch::Sender<LifecycleState>,
    shutdown_intent: watch::Sender<Option<ServerShutdown>>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl ShutdownController {
    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle.borrow().clone()
    }

    fn subscribe_to_intent(&self) -> watch::Receiver<Option<ServerShutdown>> {
        self.shutdown_intent.subscribe()
    }

    fn request(&self, request: ServerShutdown) {
        self.lifecycle.send_replace(LifecycleState::Stopping);
        self.shutdown_intent.send_replace(Some(request));
        let shutdown = self
            .shutdown
            .lock()
            .expect("shutdown sender lock is not poisoned")
            .take();
        if let Some(shutdown) = shutdown {
            tokio::spawn(async move {
                // Keep health and existing streams available briefly so the accepted response and
                // final authenticated intent can reach clients before graceful transport closure.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = shutdown.send(());
            });
        }
    }
}

#[derive(Clone)]
struct AppState {
    descriptor: Arc<RuntimeDescriptor>,
    counter: watch::Sender<CounterState>,
    shutdown: ShutdownController,
}

#[derive(Clone, Copy)]
struct CounterState {
    value: u64,
    revision: u64,
}

pub async fn spawn(config: ServerConfig) -> Result<RunningServer> {
    config.create_private_runtime_dir()?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(config.lock_path())
        .context("open server election lock")?;
    lock.try_lock_exclusive()
        .context("another server already owns this channel")?;
    protect_current_user_file(&config.lock_path())?;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind loopback server")?;
    let address = listener.local_addr().context("read server address")?;
    let descriptor = RuntimeDescriptor::new(
        format!("http://{address}"),
        format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        ServerIdentity {
            instance_id: Uuid::new_v4(),
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            build_identity: build_identity::for_current_executable()?,
        },
    );
    write_descriptor(&config.descriptor_path(), &descriptor)?;

    let (counter, _) = watch::channel(CounterState {
        value: 0,
        revision: 0,
    });
    let (lifecycle, _) = watch::channel(LifecycleState::Starting);
    let (shutdown_intent, _) = watch::channel(None);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown = ShutdownController {
        lifecycle: lifecycle.clone(),
        shutdown_intent: shutdown_intent.clone(),
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
    };
    let state = AppState {
        descriptor: Arc::new(descriptor.clone()),
        counter: counter.clone(),
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/server/stop", post(stop_server))
        .with_state(state);
    let descriptor_path = config.descriptor_path();
    let instance_id = descriptor.identity.instance_id;
    let task_lifecycle = lifecycle.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let counter_task = tokio::spawn(run_counter(counter));
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("serve local HTTP API");
        if result.is_err() {
            task_lifecycle.send_replace(LifecycleState::Failed);
        }
        counter_task.abort();
        remove_own_descriptor(&descriptor_path, instance_id);
        result
    });
    lifecycle.send_if_modified(|state| {
        if *state == LifecycleState::Starting {
            *state = LifecycleState::Ready;
            true
        } else {
            false
        }
    });

    Ok(RunningServer {
        descriptor,
        shutdown,
        task,
    })
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.shutdown.lifecycle() != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    Sse::new(event_stream(
        state.descriptor.identity.instance_id,
        state.counter.subscribe(),
        state.shutdown.subscribe_to_intent(),
        Duration::from_secs(10),
    ))
    .into_response()
}

struct EventStreamState {
    counter: watch::Receiver<CounterState>,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive: tokio::time::Interval,
    delivered_revision: u64,
    finished: bool,
}

fn event_stream(
    instance_id: Uuid,
    mut counter: watch::Receiver<CounterState>,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let current = *counter.borrow_and_update();
    let snapshot = CounterSnapshot {
        instance_id,
        value: current.value,
        revision: current.revision,
    };
    let snapshot_event = Event::default()
        .event(SNAPSHOT_EVENT)
        .id(snapshot.revision.to_string())
        .json_data(snapshot)
        .expect("counter snapshots always serialize");
    let first = stream::once(async move { Ok::<_, std::convert::Infallible>(snapshot_event) });
    let state = EventStreamState {
        counter,
        shutdown,
        keepalive: tokio::time::interval_at(
            Instant::now() + keepalive_interval,
            keepalive_interval,
        ),
        delivered_revision: current.revision,
        finished: false,
    };
    let updates = stream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        tokio::select! {
            biased;
            changed = state.shutdown.changed() => {
                if changed.is_err() {
                    return None;
                }
                let shutdown = state.shutdown.borrow_and_update().clone()?;
                let revision = state.counter.borrow().revision;
                let event = Event::default()
                    .event(SERVER_SHUTDOWN_EVENT)
                    .id(revision.to_string())
                    .json_data(shutdown)
                    .expect("server shutdown intents always serialize");
                state.finished = true;
                Some((Ok::<_, std::convert::Infallible>(event), state))
            }
            changed = state.counter.changed() => {
                if changed.is_err() {
                    return None;
                }
                let current = *state.counter.borrow_and_update();
                if current.revision != state.delivered_revision.saturating_add(1) {
                    return None;
                }
                let update = CounterUpdate {
                    value: current.value,
                    revision: current.revision,
                };
                let event = Event::default()
                    .event(COUNTER_UPDATED_EVENT)
                    .id(update.revision.to_string())
                    .json_data(update)
                    .expect("counter updates always serialize");
                state.delivered_revision = current.revision;
                Some((Ok::<_, std::convert::Infallible>(event), state))
            }
            _ = state.keepalive.tick() => Some((
                Ok::<_, std::convert::Infallible>(
                    Event::default().comment("keep-alive"),
                ),
                state,
            )),
        }
    });

    first.chain(updates)
}

async fn run_counter(counter: watch::Sender<CounterState>) {
    let mut interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    loop {
        interval.tick().await;
        counter.send_modify(|state| {
            state.value += 1;
            state.revision += 1;
        });
    }
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(state.descriptor.health(state.shutdown.lifecycle())).into_response()
}

async fn stop_server(State(state): State<AppState>, request: Request) -> StatusCode {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(body) = to_bytes(request.into_body(), 16 * 1024).await else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(request) = serde_json::from_slice::<ServerShutdown>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if request.instance_id != state.descriptor.identity.instance_id {
        return StatusCode::CONFLICT;
    }

    state.shutdown.request(request);
    StatusCode::ACCEPTED
}

fn is_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

fn write_descriptor(path: &Path, descriptor: &RuntimeDescriptor) -> Result<()> {
    let runtime_dir = path
        .parent()
        .context("runtime descriptor has no directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".runtime-")
        .suffix(".tmp")
        .tempfile_in(runtime_dir)
        .with_context(|| format!("create temporary runtime descriptor in {runtime_dir:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("protect temporary runtime descriptor")?;
    }
    serde_json::to_writer(temporary.as_file_mut(), descriptor)
        .context("encode runtime descriptor")?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .context("finish runtime descriptor")?;
    temporary
        .as_file()
        .sync_all()
        .context("flush runtime descriptor")?;
    let published = temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("publish runtime descriptor atomically")?;
    protect_current_user_file(path)?;
    published
        .sync_all()
        .context("flush published runtime descriptor")?;
    sync_runtime_directory(runtime_dir)?;
    Ok(())
}

#[cfg(unix)]
fn sync_runtime_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open runtime directory {path:?}"))?
        .sync_all()
        .with_context(|| format!("flush runtime directory {path:?}"))
}

#[cfg(not(unix))]
fn sync_runtime_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn remove_own_descriptor(path: &Path, instance_id: Uuid) {
    let initially_belongs_to_instance = File::open(path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
        .is_some_and(|descriptor| descriptor.identity.instance_id == instance_id);
    if !initially_belongs_to_instance {
        return;
    }

    // Move the exact path entry aside before deleting it. If another writer replaced the
    // descriptor after the first identity check, the quarantined file will fail the second check
    // and be restored without overwriting anything newer at the canonical path.
    let quarantine_path = path.with_extension(format!(
        "cleanup-{}-{}",
        instance_id,
        Uuid::new_v4().simple()
    ));
    if fs::rename(path, &quarantine_path).is_err() {
        return;
    }

    let quarantined_belongs_to_instance = File::open(&quarantine_path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
        .is_some_and(|descriptor| descriptor.identity.instance_id == instance_id);
    if quarantined_belongs_to_instance {
        let _ = fs::remove_file(&quarantine_path);
        return;
    }

    if fs::hard_link(&quarantine_path, path).is_ok() {
        let _ = fs::remove_file(&quarantine_path);
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, pin_mut};

    use super::*;

    #[tokio::test]
    async fn lagging_event_stream_closes_without_affecting_counter_or_healthy_stream() {
        let instance_id = Uuid::new_v4();
        let (counter, _) = watch::channel(CounterState {
            value: 0,
            revision: 0,
        });
        let (shutdown, _) = watch::channel(None);
        let healthy = event_stream(
            instance_id,
            counter.subscribe(),
            shutdown.subscribe(),
            Duration::from_secs(60),
        );
        let lagging = event_stream(
            instance_id,
            counter.subscribe(),
            shutdown.subscribe(),
            Duration::from_secs(60),
        );
        pin_mut!(healthy);
        pin_mut!(lagging);

        assert!(healthy.next().await.is_some(), "healthy snapshot arrives");
        assert!(lagging.next().await.is_some(), "lagging snapshot arrives");

        counter.send_modify(|state| {
            state.value = 1;
            state.revision = 1;
        });
        assert!(
            healthy.next().await.is_some(),
            "healthy subscriber receives revision 1"
        );
        counter.send_modify(|state| {
            state.value = 2;
            state.revision = 2;
        });
        assert!(
            healthy.next().await.is_some(),
            "healthy subscriber receives revision 2"
        );

        assert_eq!(counter.borrow().revision, 2);
        assert!(
            lagging.next().await.is_none(),
            "subscriber that missed bounded latest-state delivery is disconnected"
        );

        counter.send_modify(|state| {
            state.value = 3;
            state.revision = 3;
        });
        assert!(
            healthy.next().await.is_some(),
            "healthy subscriber continues after lagging stream closes"
        );
        assert_eq!(counter.borrow().revision, 3);
    }
}
