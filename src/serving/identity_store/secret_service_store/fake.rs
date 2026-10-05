//! A Secret Service held in memory, which the Linux identity store is
//! tested against. It answers at a Unix socket as a peer, with no bus
//! between, each connection served on a runtime of its own, so the store's
//! calls — each on a runtime of their own — reach it as they would a real
//! one. What it keeps, whether its collections are locked, and which
//! prompts it brought up and how they were answered can be seen and set
//! from outside.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use futures_util::StreamExt;
use zbus::{
    fdo, interface,
    object_server::{ObjectServer, SignalEmitter},
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

/// The Secret Service's own object.
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const LABEL_PROPERTY: &str = "org.freedesktop.Secret.Item.Label";
const ATTRIBUTES_PROPERTY: &str = "org.freedesktop.Secret.Item.Attributes";
/// The object path that names nothing.
const NOTHING: &str = "/";

/// The fake Secret Service, answering as long as it is held.
pub(super) struct FakeSecrets {
    socket: PathBuf,
    state: Arc<Mutex<State>>,
    /// Serves each connection, and stops serving once dropped.
    runtime: Option<tokio::runtime::Runtime>,
    _directory: tempfile::TempDir,
}

/// What the fake Secret Service keeps, and how it answers.
#[derive(Default)]
pub(super) struct State {
    /// Whether each collection is locked, by its path.
    collections: BTreeMap<String, bool>,
    /// The collection the `default` alias names.
    pub(super) default: Option<String>,
    /// Each item, by its path.
    items: BTreeMap<String, Kept>,
    /// How many objects it has made, which names the next.
    made: usize,
    /// Whether a search leaves out the items of a locked collection, rather
    /// than answer them as locked: as a Secret Service that cannot search a
    /// locked collection does.
    pub(super) hides_locked_items: bool,
    /// Whether it brings up a prompt before it keeps a new item, or deletes
    /// one, whatever is locked: as KeePassXC asks before it keeps one.
    pub(super) prompts_on_create: bool,
    pub(super) prompts_on_delete: bool,
    /// How a prompt it brings up is answered.
    pub(super) prompt_answer: Answer,
    /// How many of its prompts were shown.
    pub(super) prompts_shown: usize,
    /// Whether a search locks every collection before it searches, or
    /// unlocks every one after.
    pub(super) locks_as_searched: bool,
    pub(super) unlocks_as_searched: bool,
}

/// How a prompt the fake brings up is answered, once shown.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Answer {
    /// Its user goes ahead.
    #[default]
    Accepted,
    /// Its user dismisses it.
    Dismissed,
    /// Nobody answers it.
    Never,
}

/// An item the fake keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Kept {
    pub(super) collection: String,
    pub(super) label: String,
    pub(super) attributes: HashMap<String, String>,
    pub(super) secret: Vec<u8>,
}

impl State {
    /// Locks or unlocks every collection.
    pub(super) fn lock_all(&mut self, locked: bool) {
        self.collections
            .values_mut()
            .for_each(|lock| *lock = locked);
    }

    /// Locks or unlocks the collection at `collection`.
    pub(super) fn lock(&mut self, collection: &str, locked: bool) {
        *self
            .collections
            .get_mut(collection)
            .expect("the fake keeps the collection") = locked;
    }

    /// Every item it keeps.
    pub(super) fn items(&self) -> Vec<Kept> {
        self.items.values().cloned().collect()
    }

    /// The path of a new collection, unlocked, named `name`.
    pub(super) fn add_collection(&mut self, name: &str) -> String {
        let path = format!("{SERVICE_PATH}/collection/{name}");
        self.collections.insert(path.clone(), false);
        path
    }

    /// The path of a new object under `under`.
    fn next_path(&mut self, under: &str) -> String {
        self.made += 1;
        format!("{under}/{}", self.made)
    }

    fn is_locked(&self, collection: &str) -> bool {
        self.collections[collection]
    }

    /// Does what a prompt was brought up for, answering what it made.
    fn go_ahead(&mut self, pending: &Pending) -> OwnedObjectPath {
        match pending {
            Pending::Keep { replace, kept } => owned_path(&self.keep(*replace, kept.clone())),
            Pending::Delete(item) => {
                self.items.remove(item);
                owned_path(NOTHING)
            }
        }
    }

    /// Keeps `kept`, in place of the item of its collection with its
    /// attributes where `replace`, answering its path.
    fn keep(&mut self, replace: bool, kept: Kept) -> String {
        let replaced = self
            .items
            .iter()
            .find(|(_, other)| {
                replace
                    && other.collection == kept.collection
                    && other.attributes == kept.attributes
            })
            .map(|(path, _)| path.clone());
        let path = replaced.unwrap_or_else(|| self.next_path(&kept.collection));
        self.items.insert(path.clone(), kept);
        path
    }
}

impl FakeSecrets {
    /// A fake Secret Service keeping one collection, unlocked, as its
    /// default, and nothing in it.
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().expect("a directory for the socket");
        let socket = directory.path().join("secrets");
        let mut state = State::default();
        state.default = Some(state.add_collection("login"));
        let state = Arc::new(Mutex::new(state));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime to serve on");
        let listener = {
            let _entered = runtime.enter();
            tokio::net::UnixListener::bind(&socket).expect("a socket to listen on")
        };
        runtime.spawn(serve(listener, Arc::clone(&state)));
        Self {
            socket,
            state,
            runtime: Some(runtime),
            _directory: directory,
        }
    }

    /// Where it answers.
    pub(super) fn socket(&self) -> &Path {
        &self.socket
    }

    /// What it keeps, and how it answers, to see and to set.
    pub(super) fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
}

impl Drop for FakeSecrets {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// Serves each connection made at `listener`, until its peer closes it.
async fn serve(listener: tokio::net::UnixListener, state: Arc<Mutex<State>>) {
    while let Ok((socket, _)) = listener.accept().await {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let Ok(connection) = serve_connection(socket, state).await else {
                return;
            };
            let mut messages = zbus::MessageStream::from(&connection);
            while let Some(Ok(_)) = messages.next().await {}
        });
    }
}

/// A connection made at `socket`, serving the Secret Service and every
/// collection and item it keeps.
async fn serve_connection(
    socket: tokio::net::UnixStream,
    state: Arc<Mutex<State>>,
) -> zbus::Result<zbus::Connection> {
    let (collections, items) = {
        let state = lock(&state);
        let collections: Vec<String> = state.collections.keys().cloned().collect();
        let items: Vec<String> = state.items.keys().cloned().collect();
        (collections, items)
    };
    let mut builder = zbus::connection::Builder::unix_stream(socket)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(SERVICE_PATH, Service(Arc::clone(&state)))?;
    for path in collections {
        let collection = Collection {
            path: path.clone(),
            state: Arc::clone(&state),
        };
        builder = builder.serve_at(path, collection)?;
    }
    for path in items {
        let item = Item {
            path: path.clone(),
            state: Arc::clone(&state),
        };
        builder = builder.serve_at(path, item)?;
    }
    builder.build().await
}

struct Service(Arc<Mutex<State>>);

#[interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    /// Opens a session, plain only: the store's tests leave encrypting
    /// secrets to the crate, whose tests cover it.
    #[zbus(out_args("output", "result"))]
    fn open_session(
        &self,
        algorithm: String,
        _input: OwnedValue,
    ) -> fdo::Result<(OwnedValue, OwnedObjectPath)> {
        if algorithm != "plain" {
            return Err(fdo::Error::NotSupported(algorithm));
        }
        let path = lock(&self.0).next_path(&format!("{SERVICE_PATH}/session"));
        Ok((
            OwnedValue::try_from(Value::from("")).expect("a string is a value"),
            owned_path(&path),
        ))
    }

    #[zbus(out_args("unlocked", "locked"))]
    fn search_items(
        &self,
        attributes: HashMap<String, String>,
    ) -> (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) {
        let mut state = lock(&self.0);
        if state.locks_as_searched {
            state.lock_all(true);
        }
        let (mut unlocked, mut locked) = (Vec::new(), Vec::new());
        for (path, kept) in &state.items {
            let matches = attributes
                .iter()
                .all(|(name, value)| kept.attributes.get(name) == Some(value));
            if !matches {
                continue;
            }
            if !state.is_locked(&kept.collection) {
                unlocked.push(owned_path(path));
            } else if !state.hides_locked_items {
                locked.push(owned_path(path));
            }
        }
        if state.unlocks_as_searched {
            state.lock_all(false);
        }
        (unlocked, locked)
    }

    fn read_alias(&self, name: String) -> OwnedObjectPath {
        let state = lock(&self.0);
        let collection = match name.as_str() {
            "default" => state.default.as_deref(),
            _ => None,
        };
        owned_path(collection.unwrap_or(NOTHING))
    }

    #[zbus(property)]
    fn collections(&self) -> Vec<OwnedObjectPath> {
        let state = lock(&self.0);
        state
            .collections
            .keys()
            .map(|path| owned_path(path))
            .collect()
    }
}

/// A secret as the Secret Service takes and gives it: its session, the
/// parameters it is encrypted with, it, and its content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

struct Collection {
    path: String,
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.freedesktop.Secret.Collection")]
impl Collection {
    #[zbus(out_args("item", "prompt"))]
    async fn create_item(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        mut properties: HashMap<String, OwnedValue>,
        secret: Secret,
        replace: bool,
    ) -> fdo::Result<(OwnedObjectPath, OwnedObjectPath)> {
        let mut property = |name: &str| {
            properties
                .remove(name)
                .ok_or_else(|| fdo::Error::InvalidArgs(format!("no {name}")))
        };
        let label = String::try_from(property(LABEL_PROPERTY)?)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let attributes = HashMap::<String, String>::try_from(property(ATTRIBUTES_PROPERTY)?)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let kept = Kept {
            collection: self.path.clone(),
            label,
            attributes,
            secret: secret.2,
        };
        let made = {
            let mut state = lock(&self.state);
            if state.is_locked(&self.path) || state.prompts_on_create {
                Err(state.next_path(&format!("{SERVICE_PATH}/prompt")))
            } else {
                Ok(state.keep(replace, kept.clone()))
            }
        };
        match made {
            Ok(path) => {
                let item = Item {
                    path: path.clone(),
                    state: Arc::clone(&self.state),
                };
                server.at(path.as_str(), item).await?;
                Ok((owned_path(&path), owned_path(NOTHING)))
            }
            Err(prompt) => {
                let pending = Pending::Keep { replace, kept };
                serve_prompt(server, &prompt, pending, &self.state).await?;
                Ok((owned_path(NOTHING), owned_path(&prompt)))
            }
        }
    }

    #[zbus(property)]
    fn locked(&self) -> bool {
        lock(&self.state).is_locked(&self.path)
    }
}

struct Item {
    path: String,
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.freedesktop.Secret.Item")]
impl Item {
    fn get_secret(&self, session: OwnedObjectPath) -> fdo::Result<Secret> {
        let state = lock(&self.state);
        let kept = state
            .items
            .get(&self.path)
            .ok_or_else(|| fdo::Error::UnknownObject(self.path.clone()))?;
        if state.is_locked(&kept.collection) {
            return Err(fdo::Error::AccessDenied("the item is locked".to_owned()));
        }
        Ok((
            session,
            Vec::new(),
            kept.secret.clone(),
            "application/octet-stream".to_owned(),
        ))
    }

    async fn delete(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let prompt = {
            let mut state = lock(&self.state);
            let kept = state
                .items
                .get(&self.path)
                .ok_or_else(|| fdo::Error::UnknownObject(self.path.clone()))?;
            if state.is_locked(&kept.collection) || state.prompts_on_delete {
                Some(state.next_path(&format!("{SERVICE_PATH}/prompt")))
            } else {
                state.items.remove(&self.path);
                None
            }
        };
        match prompt {
            Some(prompt) => {
                let pending = Pending::Delete(self.path.clone());
                serve_prompt(server, &prompt, pending, &self.state).await?;
                Ok(owned_path(&prompt))
            }
            None => Ok(owned_path(NOTHING)),
        }
    }

    #[zbus(property)]
    fn locked(&self) -> bool {
        let state = lock(&self.state);
        state
            .items
            .get(&self.path)
            .is_some_and(|kept| state.is_locked(&kept.collection))
    }
}

/// What a prompt was brought up for, done once its user goes ahead.
enum Pending {
    Keep { replace: bool, kept: Kept },
    Delete(String),
}

/// A prompt the fake brought up, answered as its state says once shown.
struct Prompt {
    pending: Pending,
    state: Arc<Mutex<State>>,
}

#[interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    async fn prompt(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        _window_id: String,
    ) -> fdo::Result<()> {
        let completed = {
            let mut state = lock(&self.state);
            state.prompts_shown += 1;
            match state.prompt_answer {
                Answer::Accepted => Some((false, state.go_ahead(&self.pending))),
                Answer::Dismissed => Some((true, owned_path(NOTHING))),
                Answer::Never => None,
            }
        };
        if let Some((dismissed, result)) = completed {
            Self::completed(&emitter, dismissed, Value::from(ObjectPath::from(result))).await?;
        }
        Ok(())
    }

    async fn dismiss(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
        Self::completed(
            &emitter,
            true,
            Value::from(ObjectPath::from(owned_path(NOTHING))),
        )
        .await?;
        Ok(())
    }

    #[zbus(signal)]
    async fn completed(
        emitter: &SignalEmitter<'_>,
        dismissed: bool,
        result: Value<'_>,
    ) -> zbus::Result<()>;
}

async fn serve_prompt(
    server: &ObjectServer,
    prompt: &str,
    pending: Pending,
    state: &Arc<Mutex<State>>,
) -> fdo::Result<()> {
    let prompt_object = Prompt {
        pending,
        state: Arc::clone(state),
    };
    server.at(prompt, prompt_object).await?;
    Ok(())
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().expect("the fake's state is not poisoned")
}

fn owned_path(path: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path).expect("an object path")
}
