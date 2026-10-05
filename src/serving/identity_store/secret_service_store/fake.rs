//! A Secret Service held in memory, which the Linux identity store is
//! tested against over a peer-to-peer D-Bus connection, as it speaks to a
//! real one over the session bus. What it keeps, whether its collections
//! are locked, and which prompts it answered with and what became of them
//! can be seen and set from outside.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

use zbus::{
    Connection, fdo, interface,
    object_server::ObjectServer,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use super::{
    ALGORITHM, ATTRIBUTES_PROPERTY, KeyPair, LABEL_PROPERTY, NOTHING, SERVICE_PATH, Secret, Session,
};

/// The fake Secret Service, and the client's connection to it.
pub(super) struct FakeSecrets {
    /// The client's end of the connection.
    pub(super) bus: Connection,
    /// The fake's end, which answers as long as it is held.
    server: Connection,
    state: Arc<Mutex<State>>,
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
    /// The key of each session, by its path.
    sessions: HashMap<String, [u8; 16]>,
    /// How many objects it has made, which names the next.
    made: usize,
    /// How many of its prompts were shown, and how many dismissed.
    pub(super) prompts_shown: usize,
    pub(super) prompts_dismissed: usize,
    /// Whether a search leaves out the items of a locked collection, rather
    /// than answer them as locked: as a Secret Service that cannot search a
    /// locked collection does.
    pub(super) hides_locked_items: bool,
    /// Whether it answers putting an item, or deleting one, with a prompt
    /// whatever is locked: as it does for a collection that locked as it
    /// was asked.
    pub(super) prompts_on_create: bool,
    pub(super) prompts_on_delete: bool,
    /// Whether a search locks every collection before it searches, or
    /// unlocks every one after.
    pub(super) locks_as_searched: bool,
    pub(super) unlocks_as_searched: bool,
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

    /// The path of a new object under `under`.
    fn next_path(&mut self, under: &str) -> String {
        self.made += 1;
        format!("{under}/{}", self.made)
    }

    /// A new prompt's path, which `server` is to answer at.
    fn prompt(&mut self) -> String {
        self.next_path("/org/freedesktop/secrets/prompt")
    }

    fn is_locked(&self, collection: &str) -> bool {
        self.collections[collection]
    }
}

impl FakeSecrets {
    /// A fake Secret Service keeping one collection, unlocked, as its
    /// default, and nothing in it.
    pub(super) async fn new() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (client, server) = tokio::net::UnixStream::pair().expect("a socket pair");
        let server = zbus::connection::Builder::unix_stream(server)
            .server(zbus::Guid::generate())
            .expect("a server GUID")
            .p2p()
            .serve_at(SERVICE_PATH, Service(Arc::clone(&state)))
            .expect("the Secret Service served")
            .build();
        let client = zbus::connection::Builder::unix_stream(client).p2p().build();
        let (server, bus) = tokio::try_join!(server, client).expect("a peer-to-peer connection");
        let fake = Self { bus, server, state };
        let login = fake.add_collection("login").await;
        fake.state().default = Some(login);
        fake
    }

    /// What the fake keeps, and how it answers, to see and to set.
    pub(super) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("the fake's state is not poisoned")
    }

    /// The path of a new collection, unlocked, named `name`.
    pub(super) async fn add_collection(&self, name: &str) -> String {
        let path = format!("/org/freedesktop/secrets/collection/{name}");
        self.state().collections.insert(path.clone(), false);
        self.server
            .object_server()
            .at(
                path.as_str(),
                Collection {
                    path: path.clone(),
                    state: Arc::clone(&self.state),
                },
            )
            .await
            .expect("the collection served");
        path
    }
}

struct Service(Arc<Mutex<State>>);

#[interface(name = "org.freedesktop.Secret.Service")]
impl Service {
    #[zbus(out_args("output", "result"))]
    fn open_session(
        &self,
        algorithm: String,
        input: OwnedValue,
    ) -> fdo::Result<(OwnedValue, OwnedObjectPath)> {
        if algorithm != ALGORITHM {
            return Err(fdo::Error::NotSupported(algorithm));
        }
        let theirs = Vec::<u8>::try_from(input)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let ours = KeyPair::generate().expect("a key pair");
        let key = ours
            .shared_key(&theirs)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let mut state = lock(&self.0);
        let path = state.next_path("/org/freedesktop/secrets/session");
        state.sessions.insert(path.clone(), key);
        Ok((
            OwnedValue::try_from(Value::from(ours.public())).expect("bytes are a value"),
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
        let (path, made, prompt) = {
            let mut state = lock(&self.state);
            if state.is_locked(&self.path) || state.prompts_on_create {
                (None, false, Some(state.prompt()))
            } else {
                let key = *state
                    .sessions
                    .get(secret.0.as_str())
                    .ok_or_else(|| fdo::Error::InvalidArgs("no such session".to_owned()))?;
                let session = Session {
                    path: secret.0.clone(),
                    key,
                };
                let bytes = session
                    .decrypt(secret)
                    .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
                let kept = Kept {
                    collection: self.path.clone(),
                    label,
                    attributes,
                    secret: bytes,
                };
                let replaced = state
                    .items
                    .iter()
                    .find(|(_, other)| {
                        replace
                            && other.collection == kept.collection
                            && other.attributes == kept.attributes
                    })
                    .map(|(path, _)| path.clone());
                let (path, made) = match replaced {
                    Some(path) => (path, false),
                    None => (state.next_path(&self.path), true),
                };
                state.items.insert(path.clone(), kept);
                (Some(path), made, None)
            }
        };
        if let Some(prompt) = prompt {
            serve_prompt(server, &prompt, &self.state).await?;
            return Ok((owned_path(NOTHING), owned_path(&prompt)));
        }
        let path = path.expect("an item is made where no prompt is");
        if made {
            let item = Item {
                path: path.clone(),
                state: Arc::clone(&self.state),
            };
            server.at(path.as_str(), item).await?;
        }
        Ok((owned_path(&path), owned_path(NOTHING)))
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
        let key = *state
            .sessions
            .get(session.as_str())
            .ok_or_else(|| fdo::Error::InvalidArgs("no such session".to_owned()))?;
        Session { path: session, key }
            .encrypt(&kept.secret)
            .map_err(|error| fdo::Error::Failed(error.to_string()))
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
                Some(state.prompt())
            } else {
                state.items.remove(&self.path);
                None
            }
        };
        match prompt {
            Some(prompt) => {
                serve_prompt(server, &prompt, &self.state).await?;
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

/// A prompt the fake answered with, which counts whether it is shown or
/// dismissed, and does nothing else.
struct Prompt(Arc<Mutex<State>>);

#[interface(name = "org.freedesktop.Secret.Prompt")]
impl Prompt {
    fn prompt(&self, _window_id: String) {
        lock(&self.0).prompts_shown += 1;
    }

    fn dismiss(&self) {
        lock(&self.0).prompts_dismissed += 1;
    }
}

async fn serve_prompt(
    server: &ObjectServer,
    prompt: &str,
    state: &Arc<Mutex<State>>,
) -> fdo::Result<()> {
    server.at(prompt, Prompt(Arc::clone(state))).await?;
    Ok(())
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().expect("the fake's state is not poisoned")
}

fn owned_path(path: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(path).expect("an object path")
}
