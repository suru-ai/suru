use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::KeepAlive, sse::Sse},
    routing::get,
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
use crate::protocol::{
    BUILD_IDENTITY, COUNTER_UPDATED_EVENT, CounterSnapshot, CounterUpdate, Health, LifecycleState,
    PROTOCOL_VERSION, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT, SNAPSHOT_EVENT, ServerShutdown,
    ShutdownReason,
};
use crate::runtime::protect_current_user_file;

pub type ServerConfig = RuntimeConfig;

pub struct RunningServer {
    descriptor: RuntimeDescriptor,
    lifecycle: watch::Sender<LifecycleState>,
    shutdown_intent: watch::Sender<Option<ServerShutdown>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl RunningServer {
    pub fn descriptor(&self) -> &RuntimeDescriptor {
        &self.descriptor
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.request_shutdown().await;
        self.task.await.context("server task panicked")?
    }

    pub async fn run_until_ctrl_c(mut self) -> Result<()> {
        tokio::select! {
            task = &mut self.task => task.context("server task panicked")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                self.request_shutdown().await;
                self.task.await.context("server task panicked")?
            }
        }
    }

    async fn request_shutdown(&mut self) {
        self.lifecycle.send_replace(LifecycleState::Stopping);
        self.shutdown_intent.send_replace(Some(ServerShutdown {
            instance_id: self.descriptor.instance_id,
            reason: ShutdownReason::Manual,
        }));
        tokio::task::yield_now().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[derive(Clone)]
struct AppState {
    descriptor: Arc<RuntimeDescriptor>,
    lifecycle: watch::Sender<LifecycleState>,
    counter: watch::Sender<CounterState>,
    shutdown_intent: watch::Sender<Option<ServerShutdown>>,
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
    let descriptor = RuntimeDescriptor {
        base_url: format!("http://{address}"),
        token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        instance_id: Uuid::new_v4(),
        pid: std::process::id(),
        protocol_version: PROTOCOL_VERSION,
        build_identity: BUILD_IDENTITY.to_owned(),
    };
    write_descriptor(&config.descriptor_path(), &descriptor)?;

    let (counter, _) = watch::channel(CounterState {
        value: 0,
        revision: 0,
    });
    let (lifecycle, _) = watch::channel(LifecycleState::Starting);
    let (shutdown_intent, _) = watch::channel(None);
    let state = AppState {
        descriptor: Arc::new(descriptor.clone()),
        lifecycle: lifecycle.clone(),
        counter: counter.clone(),
        shutdown_intent: shutdown_intent.clone(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .with_state(state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let descriptor_path = config.descriptor_path();
    let instance_id = descriptor.instance_id;
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
        lifecycle,
        shutdown_intent,
        shutdown: Some(shutdown_tx),
        task,
    })
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if *state.lifecycle.borrow() != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let mut receiver = state.counter.subscribe();
    let shutdown_receiver = state.shutdown_intent.subscribe();
    let current = *receiver.borrow_and_update();
    let snapshot = CounterSnapshot {
        instance_id: state.descriptor.instance_id,
        value: current.value,
        revision: current.revision,
    };
    let snapshot_event = Event::default()
        .event(SNAPSHOT_EVENT)
        .id(snapshot.revision.to_string())
        .json_data(snapshot)
        .expect("counter snapshots always serialize");
    let first = stream::once(async move { Ok::<_, std::convert::Infallible>(snapshot_event) });
    let updates = stream::unfold(
        (receiver, shutdown_receiver, false),
        |(mut receiver, mut shutdown_receiver, finished)| async move {
            if finished {
                return None;
            }
            tokio::select! {
                biased;
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let shutdown = shutdown_receiver.borrow_and_update().clone()?;
                    let revision = receiver.borrow().revision;
                    let event = Event::default()
                        .event(SERVER_SHUTDOWN_EVENT)
                        .id(revision.to_string())
                        .json_data(shutdown)
                        .expect("server shutdown intents always serialize");
                    Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (receiver, shutdown_receiver, true),
                    ))
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let current = *receiver.borrow_and_update();
                    let update = CounterUpdate {
                        value: current.value,
                        revision: current.revision,
                    };
                    let event = Event::default()
                        .event(COUNTER_UPDATED_EVENT)
                        .id(update.revision.to_string())
                        .json_data(update)
                        .expect("counter updates always serialize");
                    Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (receiver, shutdown_receiver, false),
                    ))
                }
            }
        },
    );

    Sse::new(first.chain(updates))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .text("keep-alive"),
        )
        .into_response()
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

    Json(Health {
        instance_id: state.descriptor.instance_id,
        pid: state.descriptor.pid,
        lifecycle: state.lifecycle.borrow().clone(),
        protocol_version: state.descriptor.protocol_version,
        build_identity: state.descriptor.build_identity.clone(),
    })
    .into_response()
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
        .is_some_and(|descriptor| descriptor.instance_id == instance_id);
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
        .is_some_and(|descriptor| descriptor.instance_id == instance_id);
    if quarantined_belongs_to_instance {
        let _ = fs::remove_file(&quarantine_path);
        return;
    }

    if fs::hard_link(&quarantine_path, path).is_ok() {
        let _ = fs::remove_file(&quarantine_path);
    }
}
