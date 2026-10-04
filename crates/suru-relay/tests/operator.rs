//! The operator's command line, run as the Relay binary is run: what it lists
//! of a Relay's Accounts and Logins, as a table and as JSON, and what removing
//! one comes to at a Relay running on the same records — refused from the
//! moment the removal is made, and everything standing on it cut at once,
//! with no restart, the command returning once the Relay has cut it — or at
//! one that is not running at all; and how it exits.

use std::{
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use diesel::{Connection, RunQueryDsl, SqliteConnection, connection::SimpleConnection};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use futures_util::{SinkExt, StreamExt};
use rcgen::{KeyPair, PublicKeyData, SigningKey};
use suru_relay::{
    Admission, AdmissionRule, Clock, Identity, REMOVALS_CUT_AT_ONCE, RelayConfig, RunningRelay,
    SCRIPTED_VERIFICATION_URI, ScriptedProvider, Store,
};
use suru_relay_protocol::{
    Account, Bytes, Cap, Refusal, RelayMessage, SPOKEN, ServerMessage, proof_message,
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
/// any test runs. Such a test has the Relay look when it says.
const NOT_YET: Duration = Duration::from_secs(60 * 60);

/// The Relay's migrations, to make records as an older Relay left them.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// How the command line exits: having done as it was asked; having failed,
/// saying why; given a command line it cannot read; and having removed
/// something whose cut the running Relay did not confirm in time.
const DONE: i32 = 0;
const FAILED: i32 = 1;
const MALFORMED: i32 = 2;
const CUT_UNCONFIRMED: i32 = 3;

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

/// The operator's command line on the records at `database`, as `arguments`
/// say, ready to run.
fn command_line(database: &Path, arguments: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_suru-relay"));
    command
        .arg("--database")
        .arg(database)
        .args(arguments)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Runs the operator's command line on the records at `database`, as
/// `arguments` say.
async fn operate(database: &Path, arguments: &[&str]) -> Output {
    timeout(DEADLINE, command_line(database, arguments).output())
        .await
        .expect("the command line finishes in time")
        .expect("run the command line")
}

/// What a command that did as it was asked printed, having said nothing on
/// standard error.
fn printed(output: &Output) -> String {
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(DONE),
        "the command failed: {said}"
    );
    assert!(
        said.is_empty(),
        "the command said on standard error: {said}"
    );
    String::from_utf8(output.stdout.clone()).expect("the command prints UTF-8")
}

/// What a command that failed said on standard error, having printed
/// nothing.
fn refused(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(FAILED),
        "the command did not fail as a command that cannot do what it is asked does: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "a refused command prints nothing: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What a removal whose cut the running Relay did not confirm printed of
/// what it removed, and what it said on standard error of the cut.
fn unconfirmed(output: &Output) -> (String, String) {
    assert_eq!(
        output.status.code(),
        Some(CUT_UNCONFIRMED),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
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
        assert_eq!(
            Self::logging_in(relay, key, who, hostname).await,
            RelayMessage::LoginDone {
                account: Account {
                    provider: PROVIDER.to_owned(),
                    username: who.1.to_owned(),
                },
            }
        );
    }

    /// Logs in as `key`, reporting `hostname`, as the identity `who`: how the
    /// Relay says the login ended.
    async fn logging_in(
        relay: &Relay,
        key: &KeyPair,
        who: (&str, &str),
        hostname: &str,
    ) -> RelayMessage {
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
        client.hear().await
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
             583231): its Server must log in again to use this Relay. No Pairing ends.\n\
             The Relay running on these records has cut every connection its Server held \
             there.\n"
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

    unconfirmed(
        &operate(
            &relay.database(),
            &[
                "logins",
                "remove",
                &fingerprint(&workstation),
                "--wait",
                "0",
            ],
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
/// the old Account's is carried for the new one; and the new Login stands
/// through the Relay's next look, which finds nothing of the removal to cut.
#[tokio::test]
async fn a_login_formed_again_before_the_relay_has_cut_the_one_removed_carries_nothing_of_it() {
    let relay = Relay::start(NOT_YET).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let (mut joining, mut serving) = joined(&relay, &workstation, &laptop).await;
    let mut idle = Client::proven(&relay, &laptop).await;

    unconfirmed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&laptop), "--wait", "0"],
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
    let mut standing = Client::proven(&relay, &laptop).await;
    relay
        .running
        .look_for_removals()
        .await
        .expect("the Relay looks for removals");
    standing.say(&ServerMessage::Wait).await;
    assert_eq!(
        standing.hear().await,
        RelayMessage::Waiting,
        "what stands on the new Login stands through the Relay's next look"
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
         The Relay running on these records has cut every connection their Servers held \
         there.\n\
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

    let began = std::time::Instant::now();
    assert_eq!(
        printed(
            &operate(
                &stopped.database(),
                &["logins", "remove", &fingerprint(&laptop)],
            )
            .await,
        ),
        format!(
            "Removed the Login of laptop, {}, under the Account octocat (github 583231): its \
             Server must log in again to use this Relay. No Pairing ends.\n\
             No Relay is running on these records, so its Server held no connection there to \
             cut.\n",
            fingerprint(&laptop)
        )
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "with no Relay to cut anything, the command waits for none"
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

/// A removal returns once the Relay running on the records has cut what
/// stood on what it removed — not as soon as it is made — so the operator
/// knows the joins it carried have been closed by the time it returns.
#[tokio::test]
async fn a_removal_returns_once_the_running_relay_has_cut_what_stood_on_it() {
    let relay = Relay::start(NOT_YET).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let (mut joining, mut serving) = joined(&relay, &workstation, &laptop).await;

    let mut removing = command_line(
        &relay.database(),
        &["logins", "remove", &fingerprint(&laptop)],
    )
    .spawn()
    .expect("run the command line");
    // The removal is made, and the command goes on waiting while the Relay
    // has yet to look for it.
    timeout(DEADLINE, async {
        while relay.running.store().logins().await.unwrap().len() > 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the removal is made");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        removing.try_wait().unwrap().is_none(),
        "the command waits for the running Relay to cut what stood on the Login"
    );

    relay
        .running
        .look_for_removals()
        .await
        .expect("the Relay looks for removals");
    let removed = timeout(DEADLINE, removing.wait_with_output())
        .await
        .expect("the command returns once the Relay has cut")
        .unwrap();
    assert!(printed(&removed).ends_with(
        "The Relay running on these records has cut every connection its Server held \
             there.\n"
    ));
    assert_eq!(joining.ended().await, None);
    assert_eq!(serving.ended().await, None);
    assert_eq!(relay.connection_log().written(1).await.len(), 1);
    relay.running.shutdown().await.unwrap();
}

/// A Relay running on the records that never looks for removals leaves the
/// command waiting no longer than it is told: it returns, saying the removal
/// stands and is refused already, but that the Relay has not confirmed the
/// cut, and exits saying so.
#[tokio::test]
async fn a_removal_the_running_relay_does_not_confirm_in_time_stands_and_says_so() {
    let relay = Relay::start(NOT_YET).await;
    let laptop = key();
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;

    let began = std::time::Instant::now();
    let (removed, said) = unconfirmed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&laptop), "--wait", "0.2"],
        )
        .await,
    );
    assert!(began.elapsed() >= Duration::from_millis(200));
    assert!(
        removed.starts_with("Removed the Login of laptop"),
        "{removed}"
    );
    assert!(
        said.contains("did not confirm within 200ms")
            && said.contains("refused from now on")
            && said.contains("next looks for removals"),
        "{said}"
    );
    assert_eq!(Client::login_of(&relay, &laptop).await, None);
    relay.running.shutdown().await.unwrap();
}

/// What a Relay cuts it confirms afterwards, by forgetting the removal in its
/// records: so a Relay that cannot write there — another process holding
/// them, or a fault that persists — cuts what stood on every removal all the
/// same, however many it has to look through, confirming them once it can.
#[tokio::test]
async fn removals_are_cut_however_many_there_are_though_the_relay_cannot_confirm_them() {
    let relay = Relay::start(AT_ONCE).await;
    let (workstation, laptop) = (key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    let (mut joining, mut serving) = joined(&relay, &workstation, &laptop).await;
    // Removals of Logins long gone, more than the Relay cuts at once, stand
    // ahead of the laptop's, and nothing removed can be forgotten.
    let ahead = 2 * REMOVALS_CUT_AT_ONCE + 1;
    write_records(
        &relay.database(),
        &format!(
            "CREATE TRIGGER nothing_is_forgotten BEFORE DELETE ON removed_logins
                 BEGIN SELECT RAISE(ABORT, 'the removal cannot be forgotten'); END;
             WITH RECURSIVE removal(number) AS (
                 SELECT 1 UNION ALL SELECT number + 1 FROM removal WHERE number < {ahead}
             )
             INSERT INTO removed_logins (server_key, removed_at)
                 SELECT CAST(printf('gone-%06d', number) AS BLOB), 0 FROM removal;"
        ),
    );

    let (removed, said) = unconfirmed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&laptop), "--wait", "0.3"],
        )
        .await,
    );
    assert!(
        removed.starts_with("Removed the Login of laptop"),
        "{removed}"
    );
    assert!(said.contains("did not confirm"), "{said}");
    assert_eq!(joining.ended().await, None, "the join is cut all the same");
    assert_eq!(serving.ended().await, None);

    // Once the Relay can forget them, it does.
    write_records(&relay.database(), "DROP TRIGGER nothing_is_forgotten;");
    let mut connection = SqliteConnection::establish(relay.database().to_str().unwrap()).unwrap();
    timeout(DEADLINE, async {
        loop {
            let left = diesel::select(diesel::dsl::sql::<diesel::sql_types::BigInt>(
                "(SELECT COUNT(*) FROM removed_logins)",
            ))
            .get_result::<i64>(&mut connection)
            .unwrap();
            if left == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Relay confirms every removal it cut once it can");
    relay.running.shutdown().await.unwrap();
}

/// Records an older Relay left are carried forward only by running a Relay
/// on them: the command line refuses them, leaving them as they are, so it
/// never changes them under the older Relay that may be running on them.
#[tokio::test]
async fn the_command_line_refuses_records_an_older_relay_left_and_leaves_them_as_they_are() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    let mut connection = SqliteConnection::establish(database.to_str().unwrap()).unwrap();
    while connection.pending_migrations(MIGRATIONS).unwrap().len() > 1 {
        connection.run_next_migration(MIGRATIONS).unwrap();
    }
    connection
        .batch_execute(
            "PRAGMA journal_mode = WAL;
             INSERT INTO accounts (id, created_at, logged_in_at) VALUES (1, 0, 0);
             INSERT INTO identities (provider, subject, username, account_id)
                 VALUES ('github', '583231', 'octocat', 1);
             INSERT INTO logins (server_key, fingerprint, account_id, hostname, formed_at)
                 VALUES (x'01', 'abcdef0123456789', 1, 'laptop', 0);",
        )
        .unwrap();
    let applied = connection.applied_migrations().unwrap().len();

    for arguments in [
        &["accounts", "list"][..],
        &["logins", "list", "--json"],
        &["logins", "remove", "abcdef0123456789"],
        &["accounts", "remove", "github", "583231"],
    ] {
        let said = refused(&operate(&database, arguments).await);
        assert!(
            said.contains("older Relay") && said.contains("suru-relay run"),
            "{said}"
        );
    }
    assert_eq!(
        connection.applied_migrations().unwrap().len(),
        applied,
        "the command line carries nothing forward"
    );
    drop(connection);

    // Run, a Relay carries them forward, and the command line reads them.
    drop(Store::open(&database).unwrap());
    let listed = json(&operate(&database, &["logins", "list", "--json"]).await);
    assert_eq!(listed[0]["hostname"], "laptop");
}

/// An Account's id is never given to another once its Account is removed,
/// the latest Account's included, so it names one Account in the connection
/// log for good.
#[tokio::test]
async fn an_account_id_is_never_given_again_once_its_account_is_removed() {
    let relay = Relay::start(AT_ONCE).await;
    let (workstation, laptop, tablet, phone) = (key(), key(), key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &tablet, HUBOT, "tablet").await;
    let hubots = relay.running.store().accounts().await.unwrap()[1].id;
    printed(&operate(&relay.database(), &["accounts", "remove", "github", "9919"]).await);

    let newcomer = ("4242", "newcomer");
    Client::logged_in(&relay, &laptop, newcomer, "laptop").await;
    Client::logged_in(&relay, &phone, newcomer, "phone").await;
    let newcomers = relay.running.store().accounts().await.unwrap()[1].id;
    assert!(
        newcomers > hubots,
        "a new Account takes an id no Account had: {newcomers}"
    );
    drop(joined(&relay, &laptop, &phone).await);
    let logged = relay.connection_log().written(1).await;
    assert_eq!(logged[0]["account"]["id"], newcomers);
    assert_eq!(logged[0]["account"]["username"], "newcomer");
    relay.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_command_line_exits_saying_how_it_came_out() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    drop(Store::open(&database).unwrap());
    for arguments in [
        &["logins", "remove"][..],
        &["accounts", "remove", "github"],
        &["accounts", "list", "--table"],
        &["logins", "remove", "abcdef0123", "--wait", "soon"],
    ] {
        let ran = operate(&database, arguments).await;
        assert_eq!(
            ran.status.code(),
            Some(MALFORMED),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }
    let help = printed(&operate(&database, &["--help"]).await);
    assert!(
        help.contains("Exit status")
            && help.contains("0 ")
            && help.contains("1 ")
            && help.contains("2 ")
            && help.contains("3 "),
        "{help}"
    );
    let help = printed(&operate(&database, &["logins", "remove", "--help"]).await);
    assert!(help.contains("already begun"), "{help}");
}

/// A list whose reader goes before it is printed — a pager quit, or `head`
/// read enough — ends quietly, as having done as it was asked.
#[tokio::test]
async fn a_list_whose_reader_has_gone_ends_quietly() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("relay.db");
    drop(Store::open(&database).unwrap());
    for arguments in [&["accounts", "list"][..], &["logins", "list", "--json"]] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let ran = command_line(&database, arguments)
            .stdout(Stdio::from(writer))
            .spawn()
            .expect("run the command line");
        let ran = timeout(DEADLINE, ran.wait_with_output())
            .await
            .expect("the command line finishes in time")
            .unwrap();
        assert_eq!(
            (ran.status.code(), String::from_utf8_lossy(&ran.stderr)),
            (Some(DONE), "".into())
        );
    }
}

/// A Login removed gives back its place under its Account's cap of Logins,
/// so another of its Servers can log in.
#[tokio::test]
async fn a_removed_login_gives_back_its_place_under_the_cap_on_logins() {
    let relay = Relay::configured(AT_ONCE, |config| {
        config.with_logins_per_account(std::num::NonZeroU32::MIN)
    })
    .await;
    let (laptop, workstation) = (key(), key());
    Client::logged_in(&relay, &laptop, OCTOCAT, "laptop").await;
    assert_eq!(
        refusal(&Client::logging_in(&relay, &workstation, OCTOCAT, "workstation").await),
        Some(&Refusal::CapReached {
            cap: Cap::Logins,
            limit: 1
        })
    );

    printed(
        &operate(
            &relay.database(),
            &["logins", "remove", &fingerprint(&laptop)],
        )
        .await,
    );
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    relay.running.shutdown().await.unwrap();
}

/// The connections joined for an Account removed — still carried, the Relay
/// having yet to look for the removal — count against no Account that comes
/// after it, since no later Account takes its id.
#[tokio::test]
async fn a_removed_accounts_joined_connections_count_against_no_later_account() {
    let relay = Relay::configured(NOT_YET, |config| {
        config.with_joined_connections_per_account(std::num::NonZeroU32::MIN)
    })
    .await;
    let (workstation, tablet, phone, laptop, desktop) = (key(), key(), key(), key(), key());
    Client::logged_in(&relay, &workstation, OCTOCAT, "workstation").await;
    Client::logged_in(&relay, &tablet, HUBOT, "tablet").await;
    Client::logged_in(&relay, &phone, HUBOT, "phone").await;
    let (mut joining, mut serving) = joined(&relay, &tablet, &phone).await;

    // hubot's Account, the latest, is removed as its one join is carried.
    unconfirmed(
        &operate(
            &relay.database(),
            &["accounts", "remove", "github", "9919", "--wait", "0"],
        )
        .await,
    );
    let newcomer = ("4242", "newcomer");
    Client::logged_in(&relay, &laptop, newcomer, "laptop").await;
    Client::logged_in(&relay, &desktop, newcomer, "desktop").await;
    joined(&relay, &laptop, &desktop).await;

    relay
        .running
        .look_for_removals()
        .await
        .expect("the Relay looks for removals");
    assert_eq!(joining.ended().await, None);
    assert_eq!(serving.ended().await, None);
    relay.running.shutdown().await.unwrap();
}
