//! The operator's command line, run as the Relay binary is run: what it lists
//! of a Relay's Accounts and Logins, as a table and as JSON, and what removing
//! one comes to at a Relay running on the same records — refused from the
//! moment the removal is made, and everything standing on it cut at once,
//! with no restart — or at one that is not running at all.

use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{
    Admission, AdmissionRule, Clock, Identity, RelayConfig, RunningRelay,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider, Store,
};
use suru_relay_protocol::{
    Account, Bytes, Refusal, RelayMessage, SPOKEN, ServerMessage, proof_message,
};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

/// How long a wait for what a test expects may take before the test calls it
/// a failure; every wait returns the moment it arrives.
const DEADLINE: Duration = Duration::from_secs(30);

/// The address the Relays these tests start are known as.
const PUBLIC_ADDRESS: &str = "https://relay.example.com";

/// How often a Relay looks for Logins removed from its records where a test
/// wants what stood on them cut at once.
const AT_ONCE: Duration = Duration::from_millis(10);

/// How often a Relay looks for Logins removed from its records where a test
/// sees what a removal does before the Relay has cut anything: longer than
/// any test runs.
const NOT_YET: Duration = Duration::from_secs(60 * 60);

/// When the clock of every Relay these tests start begins:
/// 2026-10-04T09:30:00Z.
const BEGINNING: u64 = 1_791_106_200;

/// The identity provider the scripted one stands in for.
const PROVIDER: &str = "github";

/// Who logs in, as each identity is known at the provider: by its numeric
/// id and its username.
const OCTOCAT: (&str, &str) = ("583231", "octocat");
const HUBOT: (&str, &str) = ("9919", "hubot");

/// A Relay running in this process on records in a directory of its own,
/// logging Servers in through a scripted provider standing in for GitHub,
/// whose clock a test moves on.
struct Relay {
    stopped: StoppedRelay,
    running: RunningRelay,
}

impl Relay {
    /// A Relay looking for Logins removed from its records every
    /// `removal_interval`.
    async fn start(removal_interval: Duration) -> Self {
        Self::configured(removal_interval, |config| config).await
    }

    /// A Relay looking for Logins removed from its records every
    /// `removal_interval`, configured further as `configure` says.
    async fn configured(
        removal_interval: Duration,
        configure: fn(RelayConfig) -> RelayConfig,
    ) -> Self {
        let stopped = StoppedRelay {
            directory: tempfile::tempdir().expect("create the Relay's directory"),
            provider: Arc::new(ScriptedProvider::new().standing_in_for(PROVIDER)),
            now: Arc::new(Mutex::new(UNIX_EPOCH + Duration::from_secs(BEGINNING))),
            connection_log: ConnectionLog::default(),
            removal_interval,
            configure,
        };
        stopped.start().await
    }

    /// The database the Relay keeps its records in.
    fn database(&self) -> PathBuf {
        self.stopped.database()
    }

    fn provider(&self) -> &ScriptedProvider {
        &self.stopped.provider
    }

    fn connection_log(&self) -> &ConnectionLog {
        &self.stopped.connection_log
    }

    /// Moves the Relay's clock on by `by`.
    fn advance(&self, by: Duration) {
        *self.stopped.now.lock().unwrap() += by;
    }

    async fn stop(self) -> StoppedRelay {
        self.running.shutdown().await.expect("stop the Relay");
        self.stopped
    }
}

/// A Relay, stopped or yet to start, and everything it keeps across starts:
/// its records, its scripted provider, its clock, its connection log, and
/// how it is configured.
struct StoppedRelay {
    directory: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
    now: Arc<Mutex<SystemTime>>,
    connection_log: ConnectionLog,
    removal_interval: Duration,
    configure: fn(RelayConfig) -> RelayConfig,
}

impl StoppedRelay {
    fn database(&self) -> PathBuf {
        self.directory.path().join("relay.db")
    }

    /// The Relay started on its records. The scripted provider admits
    /// whoever it logs in, unless a test says otherwise.
    async fn start(self) -> Relay {
        let now = self.now.clone();
        let running = suru_relay::start(
            (self.configure)(
                RelayConfig::new(
                    (std::net::Ipv4Addr::LOCALHOST, 0).into(),
                    self.database(),
                    PUBLIC_ADDRESS,
                )
                .with_clock(Clock::from_fn(move || *now.lock().unwrap()))
                .with_connection_log(self.connection_log.clone())
                .with_admission(Admission::by([
                    self.provider.clone() as Arc<dyn AdmissionRule>
                ]))
                .with_removal_interval(self.removal_interval),
            ),
            self.provider.clone(),
        )
        .await
        .expect("start the Relay");
        Relay {
            stopped: self,
            running,
        }
    }
}

/// What a Relay writes to its connection log.
#[derive(Clone, Default)]
struct ConnectionLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for ConnectionLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ConnectionLog {
    /// Every line written, read as JSON.
    fn lines(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Waits until `count` lines are written: every line written then.
    async fn written(&self, count: usize) -> Vec<serde_json::Value> {
        timeout(DEADLINE, async {
            loop {
                let lines = self.lines();
                if lines.len() >= count {
                    return lines;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the Relay logs {count} joined connections in time"))
    }
}

/// Runs the operator's command line on the records at `database`, as
/// `arguments` say.
async fn operate(database: &Path, arguments: &[&str]) -> Output {
    timeout(
        DEADLINE,
        tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"))
            .arg("--database")
            .arg(database)
            .args(arguments)
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the command line finishes in time")
    .expect("run the command line")
}

/// What a command that succeeded printed, having said nothing on standard
/// error.
fn printed(output: &Output) -> String {
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the command failed: {said}");
    assert!(
        said.is_empty(),
        "the command said on standard error: {said}"
    );
    String::from_utf8(output.stdout.clone()).expect("the command prints UTF-8")
}

/// What a command that failed said on standard error, having printed
/// nothing.
fn refused(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "the command succeeded, printing {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stdout.is_empty(),
        "a refused command prints nothing: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What `--json` printed.
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&printed(output)).expect("the command prints JSON")
}

fn key() -> KeyPair {
    KeyPair::generate().expect("generate an identity key as a Server does")
}

/// The fingerprint a Server's identity key is known by.
fn fingerprint(key: &KeyPair) -> String {
    suru_relay_protocol::fingerprint(&key.subject_public_key_info())
}

/// The time `minutes` past [`BEGINNING`], as the command line writes it.
fn minutes_in(minutes: u64) -> String {
    format!("2026-10-04T09:{:02}:00Z", 30 + minutes)
}

/// A connection to a Relay saying what a Server would.
struct Client {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    async fn connect(relay: &Relay) -> Self {
        let address = relay.running.address();
        let (socket, _) = timeout(
            DEADLINE,
            tokio_tungstenite::connect_async(format!("ws://{address}/connect")),
        )
        .await
        .expect("the Relay answers in time")
        .expect("open a WebSocket to the Relay");
        Self { socket }
    }

    async fn say(&mut self, message: &ServerMessage) {
        self.socket
            .send(Message::Text(
                serde_json::to_string(message).unwrap().into(),
            ))
            .await
            .expect("say something to the Relay");
    }

    async fn hear(&mut self) -> RelayMessage {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay answers in time")
                .expect("the Relay keeps the connection open")
                .expect("read the Relay's answer");
            match frame {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("the Relay answered {other:?}"),
            }
        }
    }

    /// Reads what the Relay says until it ends the connection: the refusal
    /// it ended it with, where it said one.
    async fn ended(&mut self) -> Option<Refusal> {
        let mut refused = None;
        timeout(DEADLINE, async {
            while let Some(Ok(frame)) = self.socket.next().await {
                match frame {
                    Message::Text(text) => {
                        if let Ok(RelayMessage::Refused { refusal, .. }) =
                            serde_json::from_str(text.as_str())
                        {
                            refused = Some(refusal);
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("the Relay ends the connection in time");
        refused
    }

    /// Says hello as `key` and proves it: what the Relay answers.
    async fn prove(&mut self, key: &KeyPair) -> RelayMessage {
        self.say(&ServerMessage::Hello {
            versions: SPOKEN.to_vec(),
            key: Bytes(key.subject_public_key_info()),
        })
        .await;
        let RelayMessage::Challenge { nonce, .. } = self.hear().await else {
            panic!("the Relay challenges a Server that says hello");
        };
        let message = proof_message(PUBLIC_ADDRESS, &nonce.0, &key.subject_public_key_info());
        self.say(&ServerMessage::Proof {
            signature: Bytes(key.sign(&message).unwrap()),
        })
        .await;
        self.hear().await
    }

    /// The Login `key` holds, as a connection proving it is told.
    async fn login_of(relay: &Relay, key: &KeyPair) -> Option<Account> {
        let mut client = Self::connect(relay).await;
        match client.prove(key).await {
            RelayMessage::Proven { login } => login,
            other => panic!("the Relay answers a proof, not {other:?}"),
        }
    }

    /// Proves `key`, whose Login stands, on a connection that then stands
    /// idle on it.
    async fn proven(relay: &Relay, key: &KeyPair) -> Self {
        let mut client = Self::connect(relay).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { login: Some(_) }
        ));
        client
    }

    /// Logs in as `key`, reporting `hostname`, as the identity `who`.
    async fn logged_in(relay: &Relay, key: &KeyPair, who: (&str, &str), hostname: &str) {
        let (subject, username) = who;
        let mut client = Self::connect(relay).await;
        assert!(matches!(
            client.prove(key).await,
            RelayMessage::Proven { .. }
        ));
        client
            .say(&ServerMessage::BeginLogin {
                hostname: hostname.to_owned(),
            })
            .await;
        let RelayMessage::LoginStarted {
            verification_uri,
            user_code,
            ..
        } = client.hear().await
        else {
            panic!("the Relay begins a login");
        };
        assert_eq!(verification_uri, SCRIPTED_VERIFICATION_URI);
        assert!(relay.provider().approve(
            &user_code,
            Identity {
                subject: subject.to_owned(),
                username: username.to_owned(),
            },
        ));
        assert_eq!(
            client.hear().await,
            RelayMessage::LoginDone {
                account: Account {
                    provider: PROVIDER.to_owned(),
                    username: username.to_owned(),
                },
            }
        );
    }

    /// Proves `key`, whose Login stands, and waits to be reached on this
    /// connection.
    async fn waiting(relay: &Relay, key: &KeyPair) -> Self {
        let mut client = Self::proven(relay, key).await;
        client.say(&ServerMessage::Wait).await;
        assert_eq!(client.hear().await, RelayMessage::Waiting);
        client
    }

    /// Proves `key` and asks to be joined to the Server whose identity key
    /// is `server`, without hearing the answer.
    async fn ask_to_join(relay: &Relay, key: &KeyPair, server: &KeyPair) -> Self {
        let mut client = Self::connect(relay).await;
        client.prove(key).await;
        client
            .say(&ServerMessage::Join {
                server: Bytes(server.subject_public_key_info()),
            })
            .await;
        client
    }

    /// The name of the next join the Relay asks this waiting Server to take
    /// up.
    async fn reached(&mut self) -> Bytes {
        match self.hear().await {
            RelayMessage::Reach { join } => join,
            other => panic!("the Relay tells a waiting Server of a join, not {other:?}"),
        }
    }

    /// Takes up the join named `join` as `key`, on a connection of its own:
    /// what the Relay answers.
    async fn take_up(relay: &Relay, key: &KeyPair, join: Bytes) -> (Self, RelayMessage) {
        let mut client = Self::connect(relay).await;
        client.prove(key).await;
        client.say(&ServerMessage::Accept { join }).await;
        let answer = client.hear().await;
        (client, answer)
    }

    async fn carry(&mut self, bytes: &[u8]) {
        self.socket
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .expect("send bytes over the join");
    }

    async fn carried(&mut self) -> Vec<u8> {
        loop {
            let frame = timeout(DEADLINE, self.socket.next())
                .await
                .expect("the Relay carries bytes in time")
                .expect("the join stays open")
                .expect("read what the Relay carried");
            match frame {
                Message::Binary(bytes) => return bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("a join carries bytes alone, not {other:?}"),
            }
        }
    }
}

fn refusal(answer: &RelayMessage) -> Option<&Refusal> {
    match answer {
        RelayMessage::Refused { refusal, .. } => Some(refusal),
        _ => None,
    }
}

/// Joins `joining` to `serving`, which waits on `waiting`, both of one
/// Account, and carries a few bytes each way: the joining end of the join
/// and the serving end.
async fn join_on(
    relay: &Relay,
    waiting: &mut Client,
    serving: &KeyPair,
    joining: &KeyPair,
) -> (Client, Client) {
    let mut asking = Client::ask_to_join(relay, joining, serving).await;
    let join = waiting.reached().await;
    let (mut taken_up, answer) = Client::take_up(relay, serving, join).await;
    assert_eq!(answer, RelayMessage::Joined);
    assert_eq!(asking.hear().await, RelayMessage::Joined);
    asking.carry(b"asked").await;
    assert_eq!(taken_up.carried().await, b"asked");
    taken_up.carry(b"answered").await;
    assert_eq!(asking.carried().await, b"answered");
    (asking, taken_up)
}

/// Joins `joining` to `serving` as [`join_on`] does, on a connection of
/// `serving`'s own that waits to be reached.
async fn joined(relay: &Relay, serving: &KeyPair, joining: &KeyPair) -> (Client, Client) {
    let mut waiting = Client::waiting(relay, serving).await;
    join_on(relay, &mut waiting, serving, joining).await
}

#[tokio::test]
async fn the_command_line_lists_accounts_and_logins_as_a_table_and_as_json() {
    let relay = Relay::configured(AT_ONCE, |config| config.with_admission_interval(AT_ONCE)).await;
    let (workstation, laptop, tablet) = (key(), key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    relay.advance(Duration::from_secs(60));
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    relay.advance(Duration::from_secs(60));
    Client::logged_in(&relay, &tablet, HUBOT, "tablet").await;
    // The rules stop admitting hubot, whose Account lapses.
    relay.provider().set_admitted(HUBOT.0, false);
    timeout(DEADLINE, async {
        while !relay.running.store().accounts().await.unwrap()[1].lapsed {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("hubot's Account lapses");
    let database = relay.database();

    assert_eq!(
        printed(&operate(&database, &["accounts", "list"]).await),
        "PROVIDER  ID      USERNAME  LOGINS  LAPSED\n\
         github    583231  octocat   2       no\n\
         github    9919    hubot     1       yes\n"
    );
    let row = |key: &KeyPair, hostname: &str, (id, username): (&str, &str), formed: u64| {
        format!(
            "{:<64}  {hostname:<11}  github    {id:<6}  {username:<8}  {}\n",
            fingerprint(key),
            minutes_in(formed)
        )
    };
    assert_eq!(
        printed(&operate(&database, &["logins", "list"]).await),
        format!(
            "{:<64}  HOSTNAME     PROVIDER  ID      USERNAME  FORMED\n{}{}{}",
            "FINGERPRINT",
            row(&workstation, "workstation", OCTOCAT, 0),
            row(&laptop, "laptop", OCTOCAT, 1),
            row(&tablet, "tablet", HUBOT, 2),
        ),
        "the Logins in the order they were formed"
    );

    assert_eq!(
        json(&operate(&database, &["accounts", "list", "--json"]).await),
        serde_json::json!([
            {
                "provider": "github",
                "subject": "583231",
                "username": "octocat",
                "lapsed": false,
                "logins": 2,
            },
            {
                "provider": "github",
                "subject": "9919",
                "username": "hubot",
                "lapsed": true,
                "logins": 1,
            },
        ])
    );
    let login = |key: &KeyPair, hostname: &str, (subject, username): (&str, &str), formed: u64| {
        serde_json::json!({
            "fingerprint": fingerprint(key),
            "hostname": hostname,
            "formed_at": minutes_in(formed),
            "account": {
                "provider": "github",
                "subject": subject,
                "username": username,
            },
        })
    };
    assert_eq!(
        json(&operate(&database, &["logins", "list", "--json"]).await),
        serde_json::json!([
            login(&workstation, "workstation", OCTOCAT, 0),
            login(&laptop, "laptop", OCTOCAT, 1),
            login(&tablet, "tablet", HUBOT, 2),
        ])
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_relay_with_nobody_on_it_lists_nothing_beneath_its_headings() {
    let relay = Relay::start(AT_ONCE).await;
    let database = relay.database();
    assert_eq!(
        printed(&operate(&database, &["accounts", "list"]).await),
        "PROVIDER  ID  USERNAME  LOGINS  LAPSED\n"
    );
    assert_eq!(
        printed(&operate(&database, &["logins", "list"]).await),
        "FINGERPRINT  HOSTNAME  PROVIDER  ID  USERNAME  FORMED\n"
    );
    for listing in ["accounts", "logins"] {
        assert_eq!(
            json(&operate(&database, &[listing, "list", "--json"]).await),
            serde_json::json!([])
        );
    }
    relay.running.shutdown().await.unwrap();
}

/// A username comes from the identity provider and a hostname from the
/// Server, so either may hold what a terminal would act on, or a reader not
/// see: the table shows each such character escaped, and JSON carries the
/// names whole.
#[tokio::test]
async fn untrusted_names_are_shown_escaped_in_a_table_and_whole_in_json() {
    let relay = Relay::start(AT_ONCE).await;
    let laptop = key();
    let username = "octo\u{1b}[2Jcat\u{200b}";
    let hostname = "laptop\u{202e}gpj.exe";
    Client::logged_in(&relay, &laptop, ("583231", username), hostname).await;
    let database = relay.database();

    let listed = printed(&operate(&database, &["accounts", "list"]).await);
    assert!(listed.contains("octo\\u{1b}[2Jcat\\u{200b}  1"), "{listed}");
    let listed = printed(&operate(&database, &["logins", "list"]).await);
    assert!(
        listed.contains("laptop\\u{202e}gpj.exe  github"),
        "{listed}"
    );
    assert!(
        !listed.contains('\u{1b}') && !listed.contains('\u{202e}'),
        "{listed:?}"
    );
    let listed = json(&operate(&database, &["logins", "list", "--json"]).await);
    assert_eq!(listed[0]["hostname"], hostname);
    assert_eq!(listed[0]["account"]["username"], username);

    let removed = printed(&operate(&database, &["logins", "remove", &fingerprint(&laptop)]).await);
    assert!(
        removed.contains("laptop\\u{202e}gpj.exe") && removed.contains("octo\\u{1b}[2Jcat"),
        "{removed}"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_a_login_cuts_what_stands_on_it_at_once_and_its_server_must_log_in_again() {
    let relay = Relay::start(AT_ONCE).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let (mut joining, mut serving) = join_on(&relay, &mut waiting, &workstation, &laptop).await;
    let mut idle = Client::proven(&relay, &laptop).await;

    // The laptop is lost: its Login is removed by the first characters of
    // its fingerprint, as the Relay goes on running.
    let laptops = fingerprint(&laptop);
    assert_eq!(
        printed(&operate(&relay.database(), &["logins", "remove", &laptops[..12]]).await),
        format!(
            "Removed the Login of laptop, {laptops}, under the Account octocat (github \
             583231): its Server must log in again to use this Relay. No Pairing ends.\n"
        )
    );
    assert_eq!(joining.ended().await, None, "the join is cut at once");
    assert_eq!(serving.ended().await, None);
    assert_eq!(
        idle.ended().await,
        Some(Refusal::LoginNeeded),
        "a connection standing on the Login is told its Server must log in again"
    );
    let logged = relay.connection_log().written(1).await;
    assert_eq!(logged.len(), 1, "the cut join is logged once: {logged:?}");
    assert_eq!(logged[0]["joining"]["fingerprint"], laptops);
    assert_eq!(logged[0]["joining"]["bytes_sent"], 5);
    assert_eq!(logged[0]["serving"]["bytes_sent"], 8);

    // The laptop holds no Login there from then on, and is joined to nothing.
    assert_eq!(Client::login_of(&relay, &laptop).await, None);
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::LoginNeeded));
    let listed = printed(&operate(&relay.database(), &["logins", "list"]).await);
    assert!(!listed.contains(&laptops), "{listed}");

    // Logging in again forms a new Login, which the workstation — waiting
    // all along — is joined to.
    relay.advance(Duration::from_secs(60));
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let listed = json(&operate(&relay.database(), &["logins", "list", "--json"]).await);
    let formed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|login| login["fingerprint"] == laptops)
        .expect("the laptop is listed again");
    assert_eq!(formed["formed_at"], minutes_in(1), "formed anew");
    join_on(&relay, &mut waiting, &workstation, &laptop).await;
    relay.running.shutdown().await.unwrap();
}

/// A removal is made in the Relay's records, and everything the Relay
/// decides on the strength of a Login it reads there as it decides: so a
/// removed Login is refused from the moment it is removed, before the
/// running Relay has looked for removals and cut what stood on it.
#[tokio::test]
async fn a_removed_login_is_refused_from_the_moment_it_is_removed() {
    let relay = Relay::start(NOT_YET).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    let join = waiting.reached().await;

    printed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&workstation)],
        )
        .await,
    );
    let (_late, answer) = Client::take_up(&relay, &workstation, join).await;
    assert_eq!(
        refusal(&answer),
        Some(&Refusal::Unexpected),
        "a join asked before the removal is taken up by nobody"
    );
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::UnknownServer));
    let mut again = Client::connect(&relay).await;
    assert_eq!(
        again.prove(&workstation).await,
        RelayMessage::Proven { login: None }
    );
    again.say(&ServerMessage::Wait).await;
    assert_eq!(refusal(&again.hear().await), Some(&Refusal::LoginNeeded));
    let mut asking = Client::ask_to_join(&relay, &laptop, &workstation).await;
    assert_eq!(refusal(&asking.hear().await), Some(&Refusal::UnknownServer));
    relay.running.shutdown().await.unwrap();
}

/// A Server whose Login was removed may log in again — under another
/// Account, even — before the running Relay has looked for removals: what
/// stood on the Login removed is cut as the new one is formed, so nothing of
/// the old Account's is carried for the new one.
#[tokio::test]
async fn a_login_formed_again_before_the_relay_has_cut_the_one_removed_carries_nothing_of_it() {
    let relay = Relay::start(NOT_YET).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let (mut joining, mut serving) = joined(&relay, &workstation, &laptop).await;
    let mut idle = Client::proven(&relay, &laptop).await;

    printed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&laptop)],
        )
        .await,
    );
    Client::logged_in(&relay, &laptop, HUBOT, "laptop").await;
    assert_eq!(
        joining.ended().await,
        None,
        "the join made under octocat's Account is cut"
    );
    assert_eq!(serving.ended().await, None);
    assert_eq!(
        idle.ended().await,
        None,
        "a connection that stood on the Login removed is let go, to connect again"
    );
    assert_eq!(relay.connection_log().written(1).await.len(), 1);
    assert_eq!(
        Client::login_of(&relay, &laptop).await,
        Some(Account {
            provider: PROVIDER.to_owned(),
            username: HUBOT.1.to_owned(),
        }),
        "the new Login stands"
    );
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_an_account_removes_and_cuts_every_login_under_it_and_touches_no_other() {
    let relay = Relay::start(AT_ONCE).await;
    let (workstation, laptop, tablet, phone) = (key(), key(), key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    Client::logged_in(&relay, &tablet, HUBOT, "tablet").await;
    Client::logged_in(&relay, &phone, HUBOT, "phone").await;
    let (mut joining, mut serving) = joined(&relay, &workstation, &laptop).await;
    let mut waiting = Client::waiting(&relay, &workstation).await;
    let mut hubots = Client::waiting(&relay, &tablet).await;

    // octocat has gone: their Account is removed by its provider and numeric
    // id, as the Relay goes on running.
    assert_eq!(
        printed(
            &operate(
                &relay.database(),
                &["accounts", "remove", "github", "583231"]
            )
            .await
        ),
        "Removed the Account octocat (github 583231) and the 2 Logins under it: their \
         Servers must log in again to use this Relay. No Pairing ends.\n\
         While this Relay's admission rules admit octocat (github 583231), they can log in \
         again, as a new Account; to keep them out, take them out of the rules.\n"
    );
    assert_eq!(joining.ended().await, None, "the join is cut at once");
    assert_eq!(serving.ended().await, None);
    assert_eq!(waiting.ended().await, Some(Refusal::LoginNeeded));
    let logged = relay.connection_log().written(1).await;
    assert_eq!(logged[0]["account"]["username"], "octocat");
    for key in [&workstation, &laptop] {
        assert_eq!(Client::login_of(&relay, key).await, None);
    }
    assert_eq!(
        printed(&operate(&relay.database(), &["accounts", "list"]).await),
        "PROVIDER  ID    USERNAME  LOGINS  LAPSED\n\
         github    9919  hubot     2       no\n"
    );
    let listed = json(&operate(&relay.database(), &["logins", "list", "--json"]).await);
    let mut hostnames = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|login| login["hostname"].as_str().unwrap())
        .collect::<Vec<_>>();
    hostnames.sort_unstable();
    assert_eq!(hostnames, ["phone", "tablet"]);

    // hubot's Servers are joined as before, on a connection that waited all
    // along.
    join_on(&relay, &mut hubots, &tablet, &phone).await;

    // octocat logs in again from the laptop, which the rules still admit: a
    // new Account, which restores no Login removed with the one before.
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    assert_eq!(
        printed(&operate(&relay.database(), &["accounts", "list"]).await),
        "PROVIDER  ID      USERNAME  LOGINS  LAPSED\n\
         github    9919    hubot     2       no\n\
         github    583231  octocat   1       no\n"
    );
    assert_eq!(Client::login_of(&relay, &workstation).await, None);
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_removal_made_while_the_relay_is_stopped_stands_once_it_starts() {
    let relay = Relay::start(AT_ONCE).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let stopped = relay.stop().await;

    printed(
        &operate(
            &stopped.database(),
            &["logins", "remove", &fingerprint(&laptop)],
        )
        .await,
    );
    let relay = stopped.start().await;
    assert_eq!(Client::login_of(&relay, &laptop).await, None);
    assert!(Client::login_of(&relay, &workstation).await.is_some());
    relay.running.shutdown().await.unwrap();
}

/// Records written into the database as a Relay would keep them, for what no
/// Relay would form: two Logins whose fingerprints share their beginning.
fn write_records(database: &Path, sql: &str) {
    let mut connection = SqliteConnection::establish(database.to_str().unwrap()).unwrap();
    connection.batch_execute(sql).unwrap();
}

#[tokio::test]
async fn removing_what_the_relay_does_not_hold_or_cannot_tell_apart_is_refused_saying_why() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    drop(Store::open(&database).unwrap());
    let (one, two) = (
        format!("abcdef01{}", "0".repeat(56)),
        format!("abcdef01{}", "1".repeat(56)),
    );
    write_records(
        &database,
        &format!(
            "INSERT INTO accounts (id, created_at, logged_in_at) VALUES (1, 0, 0);
             INSERT INTO identities (provider, subject, username, account_id)
                 VALUES ('github', '583231', 'octocat', 1);
             INSERT INTO logins (server_key, fingerprint, account_id, hostname, formed_at)
                 VALUES (x'01', '{one}', 1, 'one', 0), (x'02', '{two}', 1, 'two', 0);"
        ),
    );

    let said = refused(&operate(&database, &["accounts", "remove", "github", "4242"]).await);
    assert!(
        said.contains("no Account") && said.contains("github") && said.contains("4242"),
        "{said}"
    );
    let said = refused(&operate(&database, &["accounts", "remove", "okta", "583231"]).await);
    assert!(said.contains("no Account"), "{said}");
    let said = refused(&operate(&database, &["logins", "remove", "0123456789ab"]).await);
    assert!(
        said.contains("no Login") && said.contains("0123456789ab"),
        "{said}"
    );
    let said = refused(&operate(&database, &["logins", "remove", "abcdef01"]).await);
    assert!(
        said.contains(&one) && said.contains(&two) && said.contains("2 Logins"),
        "an ambiguous fingerprint names every Login it could be: {said}"
    );
    let said = refused(&operate(&database, &["logins", "remove", "abcd"]).await);
    assert!(said.contains("at least 8"), "{said}");
    let said = refused(&operate(&database, &["logins", "remove", "not-a-key!"]).await);
    assert!(said.contains("hexadecimal"), "{said}");
    assert_eq!(
        json(&operate(&database, &["logins", "list", "--json"]).await)
            .as_array()
            .unwrap()
            .len(),
        2,
        "nothing refused removes anything"
    );

    // Given in capitals and enough of it, it names one.
    printed(&operate(&database, &["logins", "remove", "ABCDEF010"]).await);
    let listed = json(&operate(&database, &["logins", "list", "--json"]).await);
    assert_eq!(listed[0]["fingerprint"], two);
    assert_eq!(listed.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_command_line_refuses_a_database_that_is_not_there_creating_none() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("elsewhere.db");
    for arguments in [
        &["accounts", "list"][..],
        &["logins", "list"],
        &["logins", "remove", "0123456789ab"],
        &["accounts", "remove", "github", "583231"],
    ] {
        let said = refused(&operate(&database, arguments).await);
        assert!(
            said.contains("no Relay database") && said.contains("--database"),
            "{said}"
        );
    }
    assert!(!database.exists(), "nothing is created where it was not");
}

#[tokio::test]
async fn a_database_a_newer_relay_carried_forward_is_refused_by_the_command_line_and_the_relay() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    drop(Store::open(&database).unwrap());
    write_records(
        &database,
        "INSERT INTO __diesel_schema_migrations (version) VALUES ('99991231000000');",
    );

    let said = refused(&operate(&database, &["accounts", "list"]).await);
    assert!(
        said.contains("newer Relay") && said.contains("99991231000000"),
        "{said}"
    );
    let ran = timeout(
        DEADLINE,
        tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"))
            .args(["run", "--listen", "127.0.0.1:0", "--public-address"])
            .arg(PUBLIC_ADDRESS)
            .arg("--database")
            .arg(&database)
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the Relay refuses to start in time")
    .unwrap();
    let said = refused(&ran);
    assert!(said.contains("newer Relay"), "{said}");
}
