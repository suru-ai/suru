//! How a Relay is run, as its operator says: in its configuration file, on
//! its command line, or both.
//!
//! The file is TOML, named by `--config` or by `SURU_RELAY_CONFIG`, and every
//! setting it holds can be given on the command line as well, by a flag named
//! for its key: `public_address` as `--public-address`, and a list such as
//! `admit_users` by one `--admit-user` for each name. A flag given overrides
//! its key in the file — a list given on the command line replaces the
//! file's whole — and a setting given neither way takes its default. A path
//! the file names is read from the file's own directory where it is relative,
//! and one the command line names from the working directory. The file is
//! read before any command is chosen, so the operator's commands work on the
//! same database as the Relay they run beside.
//!
//! A configuration the Relay cannot use is refused before anything is done
//! on it, saying what is wrong and where it was given: a key the file does
//! not know — a misspelled one could admit nobody, or everybody — a value of
//! the wrong kind, a setting the Relay needs and is not given, settings that
//! cannot be given together, and a file it names that cannot be read.

use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Deserialize;
use suru_relay_protocol::canonical_address;

use crate::{
    Admission, GitHub, GitHubApp, GitHubAppKey, IdentityProvider, JOINED_CONNECTIONS_PER_ACCOUNT,
    KEEPALIVE, LOGINS_PER_ACCOUNT, NoIdentityProvider, RelayConfig, TlsFiles, TrustedProxy,
};

/// Where the Relay keeps its records unless told otherwise: in the working
/// directory.
const DATABASE: &str = "suru-relay.db";

/// The environment variable that names the configuration file where
/// `--config` does not.
pub const CONFIG_VARIABLE: &str = "SURU_RELAY_CONFIG";

/// What the command line says of the configuration, whatever the command.
#[derive(Args, Debug, Default)]
pub struct Global {
    /// The Relay's configuration file, in TOML. Every setting it holds can
    /// be given on the command line too, by a flag named for its key, and a
    /// flag given overrides the file. The operator's commands read the
    /// database it names.
    #[arg(long, global = true, env = CONFIG_VARIABLE, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// The SQLite database the Relay keeps its Accounts and Logins in, unless
    /// the configuration file names one: `suru-relay.db` in the working
    /// directory unless given either way.
    #[arg(long, global = true, value_name = "PATH")]
    pub database: Option<PathBuf>,
}

/// How the Relay is run, as its command line says. Each is a key of the
/// configuration file as well, which these override.
#[derive(Args, Debug, Default)]
pub struct RunArguments {
    /// The address to listen for plain HTTP at, behind a reverse proxy that
    /// serves HTTPS for the Relay — such as `127.0.0.1:8080`. The Relay
    /// listens one way: this, or --listen-https.
    #[arg(long, value_name = "ADDRESS")]
    pub listen_http: Option<std::net::SocketAddr>,
    /// The address to serve HTTPS at, from the certificate in
    /// --tls-certificate-chain-file and --tls-private-key-file — such as
    /// `0.0.0.0:443`. The Relay listens one way: this, or --listen-http.
    #[arg(long, value_name = "ADDRESS")]
    pub listen_https: Option<std::net::SocketAddr>,
    /// The PEM file holding the certificate the Relay serves HTTPS with,
    /// followed by the certificates that issued it, as a certificate
    /// authority issues them. The Relay obtains no certificate itself: it
    /// reads this file and the key's again every minute, serving a renewed
    /// certificate from then on.
    #[arg(long, value_name = "PATH")]
    pub tls_certificate_chain_file: Option<PathBuf>,
    /// The PEM file holding the private key of the certificate the Relay
    /// serves HTTPS with.
    #[arg(long, value_name = "PATH")]
    pub tls_private_key_file: Option<PathBuf>,
    /// The address Servers reach this Relay at, as its users add it — such as
    /// `https://relay.example.com`. Every Server's proof names it, so it must
    /// be the very address its users are given; behind a reverse proxy, it
    /// is the proxy's.
    #[arg(long, value_name = "ADDRESS")]
    pub public_address: Option<String>,
    /// A reverse proxy whose `X-Forwarded-For` header the Relay believes
    /// about the address a Server connects from, named by its address, such
    /// as `10.0.0.5`, or by a network it is among, such as `10.0.0.0/8`.
    /// Give it once for each proxy. With none, the header is ignored.
    #[arg(long = "trusted-proxy", value_name = "ADDRESS")]
    pub trusted_proxies: Vec<TrustedProxy>,
    /// Requires every Account to have been logged in as, from any one of its
    /// Servers, within this many days: an Account not logged in as for
    /// longer is refused until one of its Servers logs in again, which
    /// restores them all. Unless given, a Login stands however long ago it
    /// was formed.
    #[arg(long, value_name = "DAYS")]
    pub fresh_login_days: Option<NonZeroU32>,
    /// The client ID of the GitHub App this Relay logs its users in through,
    /// which its operator registers with device login enabled — not the
    /// app's ID. Unless given, the Relay logs nobody in.
    #[arg(long, value_name = "CLIENT_ID")]
    pub github_client_id: Option<String>,
    /// The PEM file holding the private key of the GitHub App this Relay
    /// logs its users in through, as GitHub issued it, which the Relay
    /// checks the members of organizations with. It is read once, as the
    /// Relay starts.
    #[arg(long, value_name = "PATH")]
    pub github_private_key_file: Option<PathBuf>,
    /// A GitHub user this Relay admits, by their username. The name is looked
    /// up at GitHub once, as the Relay first starts naming them, and whoever
    /// went by it then is admitted by it ever after, whatever they or anyone
    /// else go by later. The Relay refuses to start naming someone it cannot
    /// look up. Give it once for each user; one no longer given has their
    /// Account lapse as the Relay starts, and given again, is the same user.
    #[arg(long = "admit-user", value_name = "USERNAME")]
    pub admit_users: Vec<String>,
    /// A GitHub organization whose members this Relay admits, private
    /// members among them, by its name. Its members are checked through the
    /// GitHub App's installation on it, which an owner of the organization
    /// installs, and which must be able to read who its members are. The
    /// organization is looked up once, as the Relay first starts naming it,
    /// and whichever went by the name then is the one whose members are
    /// admitted ever after. The Relay checks each organization as it starts,
    /// and refuses to start naming one whose members it cannot check, GitHub
    /// not answering included; once running, it keeps its Accounts while
    /// GitHub does not answer. Give it once for each organization; one no
    /// longer given has the Accounts it alone admitted lapse as the Relay
    /// starts.
    #[arg(long = "admit-organization", value_name = "ORGANIZATION")]
    pub admit_organizations: Vec<String>,
    /// How often, in minutes, the Relay checks every Account against its
    /// admission rules again — whether each user is still a member of an
    /// organization the rules name, say — counted from the end of one pass
    /// to the beginning of the next; every 15 minutes unless given. A member
    /// removed from an organization is cut off at the next check. Checking
    /// an Account against an organization takes about one request of GitHub
    /// through the app's installation on it, which GitHub allows at least
    /// 5,000 of an hour, more for a larger organization: a Relay with more
    /// Accounts than that allows for at this interval checks some of them a
    /// pass later, each pass beginning at the first Account the last could
    /// not check.
    #[arg(long, value_name = "MINUTES")]
    pub recheck_minutes: Option<NonZeroU32>,
    /// How many Servers each Account may have logged in at this Relay: the
    /// Logins standing under it, a lapsed Account's among them, since nothing
    /// of it is forgotten. A login past it is refused until a Server of that
    /// Account forgets its Login — its user removing this Relay from it — or
    /// the operator removes one; a Server already logged in logs in again in
    /// the place it holds. A login phished for a stranger's Server costs the
    /// Account one of these places, and nothing more. 64 unless given.
    #[arg(long, value_name = "LOGINS")]
    pub logins_per_account: Option<NonZeroU32>,
    /// How many connections this Relay joins for each Account at once. A
    /// Server keeping a Remote in view through the Relay holds one, so an
    /// Account whose Servers each keep the others in view holds one for each
    /// ordered pair of them. A join past it is refused until one ends. 256
    /// unless given.
    #[arg(long, value_name = "CONNECTIONS")]
    pub joined_connections_per_account: Option<NonZeroU32>,
    /// How many seconds the Relay lets a connection go with nothing sent on
    /// it before it pings — a Server's waiting to be reached, or either side
    /// of a join carrying nothing — so a reverse proxy, load balancer or
    /// firewall that closes connections idle for longer keeps them. 20
    /// unless given: keep it well below the shortest idle timeout on the way.
    #[arg(long, value_name = "SECONDS")]
    pub keepalive_seconds: Option<NonZeroU32>,
}

/// A Relay's configuration file, as written: every key it may hold, none of
/// them required by the file itself.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    database: Option<PathBuf>,
    public_address: Option<String>,
    listen_http: Option<std::net::SocketAddr>,
    listen_https: Option<std::net::SocketAddr>,
    tls_certificate_chain_file: Option<PathBuf>,
    tls_private_key_file: Option<PathBuf>,
    trusted_proxies: Option<Vec<TrustedProxy>>,
    github_client_id: Option<String>,
    github_private_key_file: Option<PathBuf>,
    admit_users: Option<Vec<String>>,
    admit_organizations: Option<Vec<String>>,
    recheck_minutes: Option<NonZeroU32>,
    fresh_login_days: Option<NonZeroU32>,
    logins_per_account: Option<NonZeroU32>,
    joined_connections_per_account: Option<NonZeroU32>,
    keepalive_seconds: Option<NonZeroU32>,
}

/// A configuration file the Relay has read: what it holds, and where it is.
#[derive(Debug)]
pub struct Configuration {
    path: PathBuf,
    file: ConfigFile,
}

impl Configuration {
    /// The configuration file at `path`, refusing one that cannot be read or
    /// that holds anything the Relay cannot use, saying what and where.
    pub fn read(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read the configuration file {}", path.display()))?;
        Self::parse(path, &text)
    }

    /// The configuration `text` holds, as though read from a file at
    /// `path`.
    pub fn parse(path: impl Into<PathBuf>, text: &str) -> Result<Self> {
        let path = path.into();
        let mut file: ConfigFile = toml::from_str(text).with_context(|| {
            format!(
                "the configuration file {} cannot be used; see the Relay's configuration \
                 reference for every key it may hold",
                path.display()
            )
        })?;
        // A path the file names is read from the file's own directory.
        let directory = path.parent().unwrap_or(Path::new("")).to_owned();
        for named in [
            &mut file.database,
            &mut file.tls_certificate_chain_file,
            &mut file.tls_private_key_file,
            &mut file.github_private_key_file,
        ]
        .into_iter()
        .flatten()
        {
            if named.is_relative() {
                *named = directory.join(&*named);
            }
        }
        Ok(Self { path, file })
    }

    /// Where the file was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The configuration file `global` names, read: none where it names none.
pub fn read(global: &Global) -> Result<Option<Configuration>> {
    global
        .config
        .as_deref()
        .map(Configuration::read)
        .transpose()
}

/// The database a command works on: as `--database` names it, else as the
/// configuration file does, else `suru-relay.db` in the working directory.
pub fn database(global: &Global, configuration: Option<&Configuration>) -> PathBuf {
    global
        .database
        .clone()
        .or_else(|| configuration.and_then(|configuration| configuration.file.database.clone()))
        .unwrap_or_else(|| PathBuf::from(DATABASE))
}

/// A setting, as the configuration file and the command line each name it.
#[derive(Clone, Copy)]
struct Name {
    key: &'static str,
    flag: &'static str,
}

const PUBLIC_ADDRESS: Name = Name {
    key: "public_address",
    flag: "--public-address",
};
const LISTEN_HTTP: Name = Name {
    key: "listen_http",
    flag: "--listen-http",
};
const LISTEN_HTTPS: Name = Name {
    key: "listen_https",
    flag: "--listen-https",
};
const TLS_CERTIFICATE_CHAIN_FILE: Name = Name {
    key: "tls_certificate_chain_file",
    flag: "--tls-certificate-chain-file",
};
const TLS_PRIVATE_KEY_FILE: Name = Name {
    key: "tls_private_key_file",
    flag: "--tls-private-key-file",
};
const GITHUB_CLIENT_ID: Name = Name {
    key: "github_client_id",
    flag: "--github-client-id",
};
const GITHUB_PRIVATE_KEY_FILE: Name = Name {
    key: "github_private_key_file",
    flag: "--github-private-key-file",
};
const ADMIT_USERS: Name = Name {
    key: "admit_users",
    flag: "--admit-user",
};
const ADMIT_ORGANIZATIONS: Name = Name {
    key: "admit_organizations",
    flag: "--admit-organization",
};
const TRUSTED_PROXIES: Name = Name {
    key: "trusted_proxies",
    flag: "--trusted-proxy",
};

impl Name {
    /// How to give this setting, where it was given neither way.
    fn wanted(self) -> String {
        format!(
            "give `{}`, or set `{}` in the configuration file",
            self.flag, self.key
        )
    }
}

/// A setting's value, and where it was given.
struct Given<T> {
    value: T,
    /// Named as it was given: by its flag, or by its key in the file.
    named: String,
}

/// What the command line and the configuration file each give of the
/// settings, and how to name each as given.
struct Sources<'a> {
    configuration: Option<&'a Configuration>,
}

impl Sources<'_> {
    /// `name`'s value as the command line gives it, or else as the file
    /// does.
    fn pick<T>(&self, name: Name, flag: Option<T>, file: Option<T>) -> Option<Given<T>> {
        match (flag, file, self.configuration) {
            (Some(value), _, _) => Some(Given {
                value,
                named: format!("`{}`", name.flag),
            }),
            (None, Some(value), Some(configuration)) => Some(Given {
                value,
                named: format!("`{}` in {}", name.key, configuration.path.display()),
            }),
            _ => None,
        }
    }

    /// A list's items as the command line gives them, replacing the file's,
    /// or else as the file does: none where neither gives any.
    fn pick_list<T>(
        &self,
        name: Name,
        flag: Vec<T>,
        file: Option<Vec<T>>,
    ) -> Option<Given<Vec<T>>> {
        self.pick(name, (!flag.is_empty()).then_some(flag), file)
            .filter(|given| !given.value.is_empty())
    }
}

/// How a Relay listens.
#[derive(Clone, Debug)]
pub enum Listen {
    /// For plain HTTP at this address, behind a reverse proxy that serves
    /// HTTPS for it.
    Http(std::net::SocketAddr),
    /// Serving HTTPS itself at this address, from these certificate files.
    Https(std::net::SocketAddr, TlsFiles),
}

/// How a Relay runs, settled from its command line and configuration file.
#[derive(Debug)]
pub struct Settings {
    pub database: PathBuf,
    pub listen: Listen,
    pub public_address: String,
    pub trusted_proxies: Vec<TrustedProxy>,
    pub github_client_id: Option<String>,
    pub github_private_key_file: Option<PathBuf>,
    pub admit_users: Vec<String>,
    pub admit_organizations: Vec<String>,
    pub recheck: Option<Duration>,
    pub fresh_login_every: Option<Duration>,
    pub logins_per_account: NonZeroU32,
    pub joined_connections_per_account: NonZeroU32,
    pub keepalive: Duration,
}

/// Settles how the Relay runs on `database` from what `arguments`, the
/// command line, and `configuration`, the file, say, the command line's word
/// taking precedence: refusing settings it cannot use, saying what is wrong
/// and where it was given. Nothing named is read yet.
pub fn settle(
    arguments: RunArguments,
    database: PathBuf,
    configuration: Option<&Configuration>,
) -> Result<Settings> {
    let sources = Sources { configuration };
    let file = configuration.map(|configuration| &configuration.file);

    let listen = listen(
        sources.pick(
            LISTEN_HTTP,
            arguments.listen_http,
            file.and_then(|file| file.listen_http),
        ),
        sources.pick(
            LISTEN_HTTPS,
            arguments.listen_https,
            file.and_then(|file| file.listen_https),
        ),
        sources.pick(
            TLS_CERTIFICATE_CHAIN_FILE,
            arguments.tls_certificate_chain_file,
            file.and_then(|file| file.tls_certificate_chain_file.clone()),
        ),
        sources.pick(
            TLS_PRIVATE_KEY_FILE,
            arguments.tls_private_key_file,
            file.and_then(|file| file.tls_private_key_file.clone()),
        ),
    )?;

    let Some(public_address) = sources.pick(
        PUBLIC_ADDRESS,
        arguments.public_address,
        file.and_then(|file| file.public_address.clone()),
    ) else {
        bail!(
            "the Relay is not told its public address: the address Servers reach it at, such as \
             `https://relay.example.com`, which every Server's proof names; {}",
            PUBLIC_ADDRESS.wanted()
        );
    };
    if canonical_address(&public_address.value).is_none() {
        bail!(
            "{} gives the Relay's public address as `{}`, which is not an https:// or http:// \
             address naming a host",
            public_address.named,
            public_address.value
        );
    }

    let github_client_id = sources.pick(
        GITHUB_CLIENT_ID,
        arguments.github_client_id,
        file.and_then(|file| file.github_client_id.clone()),
    );
    let github_private_key_file = sources.pick(
        GITHUB_PRIVATE_KEY_FILE,
        arguments.github_private_key_file,
        file.and_then(|file| file.github_private_key_file.clone()),
    );
    let admit_users = sources.pick_list(
        ADMIT_USERS,
        arguments.admit_users,
        file.and_then(|file| file.admit_users.clone()),
    );
    let admit_organizations = sources.pick_list(
        ADMIT_ORGANIZATIONS,
        arguments.admit_organizations,
        file.and_then(|file| file.admit_organizations.clone()),
    );
    if github_client_id.is_none() {
        let needing = [
            admit_users
                .as_ref()
                .map(|given| (&given.named, "names GitHub users to admit")),
            admit_organizations
                .as_ref()
                .map(|given| (&given.named, "names GitHub organizations to admit")),
            github_private_key_file
                .as_ref()
                .map(|given| (&given.named, "names a GitHub App's private key")),
        ];
        if let Some((named, what)) = needing.into_iter().flatten().next() {
            bail!(
                "{named} {what}, yet the Relay is not told which GitHub App it logs its users in \
                 through: {}",
                GITHUB_CLIENT_ID.wanted()
            );
        }
    }
    if let Some(organizations) = &admit_organizations
        && github_private_key_file.is_none()
    {
        bail!(
            "{} names GitHub organizations to admit, whose members the Relay checks as its \
             GitHub App, signing with the app's private key — yet it is not told where that key \
             is: {}",
            organizations.named,
            GITHUB_PRIVATE_KEY_FILE.wanted()
        );
    }
    let trusted_proxies = sources.pick_list(
        TRUSTED_PROXIES,
        arguments.trusted_proxies,
        file.and_then(|file| file.trusted_proxies.clone()),
    );

    let number = |flag: Option<NonZeroU32>, key: fn(&ConfigFile) -> Option<NonZeroU32>| {
        flag.or_else(|| file.and_then(key))
    };
    Ok(Settings {
        database,
        listen,
        public_address: public_address.value,
        trusted_proxies: trusted_proxies.map(|given| given.value).unwrap_or_default(),
        github_client_id: github_client_id.map(|given| given.value),
        github_private_key_file: github_private_key_file.map(|given| given.value),
        admit_users: admit_users.map(|given| given.value).unwrap_or_default(),
        admit_organizations: admit_organizations
            .map(|given| given.value)
            .unwrap_or_default(),
        recheck: number(arguments.recheck_minutes, |file| file.recheck_minutes)
            .map(|minutes| Duration::from_secs(u64::from(minutes.get()) * 60)),
        fresh_login_every: number(arguments.fresh_login_days, |file| file.fresh_login_days)
            .map(|days| Duration::from_secs(u64::from(days.get()) * 24 * 60 * 60)),
        logins_per_account: number(arguments.logins_per_account, |file| file.logins_per_account)
            .unwrap_or(LOGINS_PER_ACCOUNT),
        joined_connections_per_account: number(arguments.joined_connections_per_account, |file| {
            file.joined_connections_per_account
        })
        .unwrap_or(JOINED_CONNECTIONS_PER_ACCOUNT),
        keepalive: number(arguments.keepalive_seconds, |file| file.keepalive_seconds)
            .map_or(KEEPALIVE, |seconds| {
                Duration::from_secs(u64::from(seconds.get()))
            }),
    })
}

/// How the Relay listens, as the settings given say: one way, and with the
/// certificate files that way needs and only those.
fn listen(
    http: Option<Given<std::net::SocketAddr>>,
    https: Option<Given<std::net::SocketAddr>>,
    certificate_chain: Option<Given<PathBuf>>,
    private_key: Option<Given<PathBuf>>,
) -> Result<Listen> {
    match (http, https) {
        (Some(http), Some(https)) => bail!(
            "{} and {} are both given, yet the Relay listens one way: for plain HTTP behind a \
             reverse proxy that serves HTTPS for it, or serving HTTPS itself; give one",
            http.named,
            https.named
        ),
        (None, None) => bail!(
            "the Relay is not told where to listen: {} to listen for plain HTTP behind a reverse \
             proxy that serves HTTPS for it, or `{}` (`{}` in the configuration file) to serve \
             HTTPS itself from certificate files",
            LISTEN_HTTP.wanted(),
            LISTEN_HTTPS.flag,
            LISTEN_HTTPS.key
        ),
        (Some(http), None) => {
            if let Some(named) = certificate_chain
                .map(|given| given.named)
                .or(private_key.map(|given| given.named))
            {
                bail!(
                    "{named} names a certificate file, yet the Relay listens for plain HTTP, as \
                     {} says, serving no certificate: leave HTTPS to the reverse proxy and remove \
                     it, or serve HTTPS with `{}` instead",
                    http.named,
                    LISTEN_HTTPS.flag
                );
            }
            Ok(Listen::Http(http.value))
        }
        (None, Some(https)) => match (certificate_chain, private_key) {
            (Some(chain), Some(key)) => Ok(Listen::Https(
                https.value,
                TlsFiles::new(chain.value, key.value),
            )),
            (chain, _) => {
                let missing = if chain.is_none() {
                    TLS_CERTIFICATE_CHAIN_FILE
                } else {
                    TLS_PRIVATE_KEY_FILE
                };
                bail!(
                    "{} has the Relay serve HTTPS itself, which takes its certificate chain file \
                     and its private key file, yet it is not told where the {} is: {}",
                    https.named,
                    if chain.is_none() {
                        "certificate chain"
                    } else {
                        "private key"
                    },
                    missing.wanted()
                )
            }
        },
    }
}

impl Settings {
    /// How the Relay runs, as these settings say.
    pub fn relay_config(&self) -> RelayConfig {
        let (address, tls) = match &self.listen {
            Listen::Http(address) => (*address, None),
            Listen::Https(address, files) => (*address, Some(files.clone())),
        };
        let mut config = RelayConfig::new(address, &self.database, &self.public_address)
            .with_trusted_proxies(self.trusted_proxies.clone())
            .with_logins_per_account(self.logins_per_account)
            .with_joined_connections_per_account(self.joined_connections_per_account)
            .with_keepalive(self.keepalive)
            .with_admission(
                Admission::nobody()
                    .with_named_users(self.admit_users.clone())
                    .with_organizations(self.admit_organizations.clone()),
            );
        if let Some(files) = tls {
            config = config.with_tls(files);
        }
        if let Some(interval) = self.recheck {
            config = config.with_admission_interval(interval);
        }
        if let Some(every) = self.fresh_login_every {
            config = config.with_fresh_login_every(every);
        }
        config
    }

    /// The identity provider the Relay logs its users in through, as these
    /// settings say: GitHub, through the app they name, or none. Reads the
    /// app's private key, where they name one.
    pub fn identity_provider(&self) -> Result<Arc<dyn IdentityProvider>> {
        let Some(client_id) = &self.github_client_id else {
            return Ok(Arc::new(NoIdentityProvider));
        };
        let mut app = GitHubApp::new(client_id);
        if let Some(path) = &self.github_private_key_file {
            app = app.with_private_key(GitHubAppKey::from_pem_file(path)?);
        }
        Ok(Arc::new(GitHub::new(app)?))
    }

    /// Whether these settings admit nobody, naming no user and no
    /// organization.
    pub fn admit_nobody(&self) -> bool {
        self.admit_users.is_empty() && self.admit_organizations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration file the Relay's documentation gives as its example.
    const EXAMPLE: &str = include_str!("../../../docs/relay/suru-relay.toml");

    /// Where the example is, beside the documentation.
    fn example_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/relay/suru-relay.toml")
    }

    /// The keys of [`ConfigFile`], every one: a key added to it and not
    /// named here fails to build.
    macro_rules! keys {
        ($($key:ident),* $(,)?) => {{
            let ConfigFile { $($key: _),* } = ConfigFile::default();
            [$(stringify!($key)),*]
        }};
    }

    fn every_key() -> Vec<&'static str> {
        keys!(
            database,
            public_address,
            listen_http,
            listen_https,
            tls_certificate_chain_file,
            tls_private_key_file,
            trusted_proxies,
            github_client_id,
            github_private_key_file,
            admit_users,
            admit_organizations,
            recheck_minutes,
            fresh_login_days,
            logins_per_account,
            joined_connections_per_account,
            keepalive_seconds,
        )
        .to_vec()
    }

    fn settled(text: &str, arguments: RunArguments) -> Result<Settings> {
        let configuration = Configuration::parse(Path::new("relay").join("suru-relay.toml"), text)?;
        let database = database(&Global::default(), Some(&configuration));
        settle(arguments, database, Some(&configuration))
    }

    fn refusal(text: &str, arguments: RunArguments) -> String {
        format!(
            "{:#}",
            settled(text, arguments).expect_err("the configuration is refused")
        )
    }

    const MINIMAL: &str = "public_address = \"https://relay.example.com\"\n\
                           listen_http = \"127.0.0.1:8080\"\n";

    #[test]
    fn the_documented_example_is_a_configuration_the_relay_runs_on() {
        let configuration = Configuration::parse(example_path(), EXAMPLE).unwrap();
        let settings = settle(
            RunArguments::default(),
            database(&Global::default(), Some(&configuration)),
            Some(&configuration),
        )
        .unwrap();
        assert_eq!(settings.public_address, "https://relay.example.com");
        assert!(matches!(settings.listen, Listen::Http(_)));
        assert!(settings.github_client_id.is_some());
        assert!(!settings.admit_nobody());
        // As the file names it, read from the file's own directory where the
        // platform takes it to be relative — as Windows takes a path with no
        // drive.
        let named = Path::new("/var/lib/suru-relay/suru-relay.db");
        let expected = if named.is_relative() {
            example_path().parent().unwrap().join(named)
        } else {
            named.to_owned()
        };
        assert_eq!(settings.database, expected);
    }

    #[test]
    fn the_documented_example_shows_every_key_the_file_may_hold() {
        for key in every_key() {
            assert!(
                EXAMPLE.lines().any(|line| line
                    .trim_start_matches(['#', ' '])
                    .starts_with(&format!("{key} = "))),
                "the example shows no `{key}`"
            );
        }
        let reference = include_str!("../../../docs/relay/configuration.md");
        for key in every_key() {
            assert!(
                reference.contains(&format!("`{key}`")),
                "the configuration reference says nothing of `{key}`"
            );
        }
    }

    #[test]
    fn a_flag_overrides_its_key_and_a_list_flag_replaces_the_list() {
        let file = format!(
            "{MINIMAL}admit_users = [\"octocat\", \"mona\"]\ngithub_client_id = \"Iv23liFile\"\n\
             logins_per_account = 8\nkeepalive_seconds = 5\n"
        );
        let settings = settled(
            &file,
            RunArguments {
                public_address: Some("https://relay.example.org".to_owned()),
                admit_users: vec!["hubot".to_owned()],
                logins_per_account: NonZeroU32::new(9),
                ..RunArguments::default()
            },
        )
        .unwrap();
        assert_eq!(settings.public_address, "https://relay.example.org");
        assert_eq!(settings.admit_users, ["hubot"]);
        assert_eq!(settings.github_client_id.as_deref(), Some("Iv23liFile"));
        assert_eq!(settings.logins_per_account.get(), 9);
        assert_eq!(settings.keepalive, Duration::from_secs(5));
        assert_eq!(
            settings.joined_connections_per_account,
            JOINED_CONNECTIONS_PER_ACCOUNT
        );
    }

    #[test]
    fn a_path_the_file_names_is_read_from_its_directory() {
        let settings = settled(
            &format!("{MINIMAL}database = \"records.db\"\n"),
            RunArguments::default(),
        )
        .unwrap();
        assert_eq!(settings.database, Path::new("relay").join("records.db"));
        let global = Global {
            database: Some(PathBuf::from("elsewhere.db")),
            ..Global::default()
        };
        let configuration =
            Configuration::parse("suru-relay.toml", "database = \"records.db\"").unwrap();
        assert_eq!(
            database(&global, Some(&configuration)),
            PathBuf::from("elsewhere.db"),
            "--database overrides the file"
        );
        assert_eq!(database(&Global::default(), None), PathBuf::from(DATABASE));
    }

    #[test]
    fn a_key_the_file_does_not_know_is_refused_naming_it() {
        let said = refusal(
            &format!("{MINIMAL}admit_user = [\"octocat\"]\n"),
            RunArguments::default(),
        );
        assert!(
            said.contains("admit_user") && said.contains("unknown field"),
            "{said}"
        );
    }

    #[test]
    fn listening_both_ways_or_neither_is_refused_naming_where_each_was_given() {
        let said = refusal(
            MINIMAL,
            RunArguments {
                listen_https: Some("0.0.0.0:443".parse().unwrap()),
                ..RunArguments::default()
            },
        );
        assert!(
            said.contains("`--listen-https`") && said.contains("`listen_http` in "),
            "{said}"
        );
        let said = refusal(
            "public_address = \"https://relay.example.com\"",
            RunArguments::default(),
        );
        assert!(
            said.contains("not told where to listen") && said.contains("--listen-http"),
            "{said}"
        );
    }

    #[test]
    fn a_certificate_named_for_plain_http_or_missing_for_https_is_refused() {
        let said = refusal(
            &format!("{MINIMAL}tls_private_key_file = \"key.pem\"\n"),
            RunArguments::default(),
        );
        assert!(
            said.contains("`tls_private_key_file` in ") && said.contains("plain HTTP"),
            "{said}"
        );
        let said = refusal(
            "public_address = \"https://relay.example.com\"\nlisten_https = \"0.0.0.0:443\"\n\
             tls_certificate_chain_file = \"chain.pem\"\n",
            RunArguments::default(),
        );
        assert!(said.contains("--tls-private-key-file"), "{said}");
    }

    #[test]
    fn github_settings_wanting_the_app_they_need_are_refused() {
        let said = refusal(
            &format!("{MINIMAL}admit_organizations = [\"acme\"]\n"),
            RunArguments::default(),
        );
        assert!(said.contains("--github-client-id"), "{said}");
        let said = refusal(
            &format!("{MINIMAL}github_client_id = \"Iv23li\"\n"),
            RunArguments {
                admit_organizations: vec!["acme".to_owned()],
                ..RunArguments::default()
            },
        );
        assert!(
            said.contains("`--admit-organization`") && said.contains("--github-private-key-file"),
            "{said}"
        );
    }
}
