use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
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

use crate::protocol::{
    BUILD_IDENTITY, CounterSnapshot, CounterUpdate, Health, LifecycleState, PROTOCOL_VERSION,
    RuntimeDescriptor,
};

const RUNTIME_FILE: &str = "runtime.json";
const LOCK_FILE: &str = "server.lock";

#[derive(Clone, Debug)]
pub struct ServerConfig {
    state_dir: PathBuf,
    channel: String,
}

impl ServerConfig {
    pub fn new(state_dir: impl AsRef<Path>, channel: impl Into<String>) -> Self {
        Self {
            state_dir: state_dir.as_ref().to_path_buf(),
            channel: channel.into(),
        }
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.state_dir.join(&self.channel)
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn descriptor_path(&self) -> PathBuf {
        self.runtime_dir().join(RUNTIME_FILE)
    }

    fn validate(&self) -> Result<()> {
        let channel_is_safe = !self.channel.is_empty()
            && self.channel != "."
            && self.channel != ".."
            && self
                .channel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !channel_is_safe {
            bail!("channel must contain only letters, numbers, '.', '-', or '_'");
        }
        Ok(())
    }
}

pub struct RunningServer {
    descriptor: RuntimeDescriptor,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl RunningServer {
    pub fn descriptor(&self) -> &RuntimeDescriptor {
        &self.descriptor
    }

    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await.context("server task panicked")?
    }

    pub async fn run_until_ctrl_c(mut self) -> Result<()> {
        tokio::select! {
            task = &mut self.task => task.context("server task panicked")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                if let Some(shutdown) = self.shutdown.take() {
                    let _ = shutdown.send(());
                }
                self.task.await.context("server task panicked")?
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    descriptor: Arc<RuntimeDescriptor>,
    counter: watch::Sender<CounterState>,
}

#[derive(Clone, Copy)]
struct CounterState {
    value: u64,
    revision: u64,
}

pub async fn spawn(config: ServerConfig) -> Result<RunningServer> {
    config.validate()?;
    let runtime_dir = config.runtime_dir();
    create_private_dir(&runtime_dir)?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(runtime_dir.join(LOCK_FILE))
        .context("open server election lock")?;
    lock.try_lock_exclusive()
        .context("another server already owns this channel")?;

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
    let state = AppState {
        descriptor: Arc::new(descriptor.clone()),
        counter: counter.clone(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .with_state(state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let descriptor_path = config.descriptor_path();
    let instance_id = descriptor.instance_id;
    let task = tokio::spawn(async move {
        let _lock = lock;
        let counter_task = tokio::spawn(run_counter(counter));
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("serve local HTTP API");
        counter_task.abort();
        remove_own_descriptor(&descriptor_path, instance_id);
        result
    });

    Ok(RunningServer {
        descriptor,
        shutdown: Some(shutdown_tx),
        task,
    })
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let mut receiver = state.counter.subscribe();
    let current = *receiver.borrow_and_update();
    let snapshot = CounterSnapshot {
        instance_id: state.descriptor.instance_id,
        value: current.value,
        revision: current.revision,
    };
    let snapshot_event = Event::default()
        .event("snapshot")
        .id(snapshot.revision.to_string())
        .json_data(snapshot)
        .expect("counter snapshots always serialize");
    let first = stream::once(async move { Ok::<_, std::convert::Infallible>(snapshot_event) });
    let updates = stream::unfold(receiver, |mut receiver| async move {
        if receiver.changed().await.is_err() {
            return None;
        }
        let current = *receiver.borrow_and_update();
        let update = CounterUpdate {
            value: current.value,
            revision: current.revision,
        };
        let event = Event::default()
            .event("counter_updated")
            .id(update.revision.to_string())
            .json_data(update)
            .expect("counter updates always serialize");
        Some((Ok::<_, std::convert::Infallible>(event), receiver))
    });

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
        lifecycle: LifecycleState::Ready,
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

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create runtime directory {path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect runtime directory {path:?}"))?;
    }
    Ok(())
}

fn write_descriptor(path: &Path, descriptor: &RuntimeDescriptor) -> Result<()> {
    let temporary_path = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary_path)
        .with_context(|| format!("create temporary runtime descriptor {temporary_path:?}"))?;
    serde_json::to_writer(&mut file, descriptor).context("encode runtime descriptor")?;
    file.write_all(b"\n").context("finish runtime descriptor")?;
    file.sync_all().context("flush runtime descriptor")?;
    fs::rename(&temporary_path, path).context("publish runtime descriptor atomically")?;
    Ok(())
}

fn remove_own_descriptor(path: &Path, instance_id: Uuid) {
    let belongs_to_instance = File::open(path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
        .is_some_and(|descriptor| descriptor.instance_id == instance_id);
    if belongs_to_instance {
        let _ = fs::remove_file(path);
    }
}
