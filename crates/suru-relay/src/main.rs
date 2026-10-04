use std::{net::SocketAddr, num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use suru_relay::{
    Admission, GitHub, GitHubApp, GitHubAppKey, IdentityProvider, NoIdentityProvider,
    OperatorCommand, RelayConfig, TrustedProxy,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// A Relay for Suru: carries Pairings between Servers that cannot reach each
/// other directly, for Servers logged in under one Account.
///
/// `suru-relay run` runs the Relay. Its operator lists and removes the
/// Accounts and Logins in its records with the other commands, whether or not
/// the Relay is running on them: a removal takes effect on a running Relay at
/// once, with no restart.
#[derive(Parser)]
#[command(version)]
struct CommandLine {
    /// The SQLite database the Relay keeps its Accounts and Logins in.
    #[arg(long, global = true, default_value = "suru-relay.db")]
    database: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the Relay.
    ///
    /// It writes one JSON line to standard output for each connection it
    /// joins, naming the Account and each Server, and its own diagnostics to
    /// standard error.
    Run(Arguments),
    #[command(flatten)]
    Operate(OperatorCommand),
}

/// How the Relay is run.
#[derive(Args)]
struct Arguments {
    /// Where to listen for plain HTTP, behind a reverse proxy that serves
    /// HTTPS.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// The address Servers reach this Relay at, as its users add it — such as
    /// `https://relay.example.com`. Every Server's proof names it.
    #[arg(long)]
    public_address: String,
    /// A reverse proxy whose `X-Forwarded-For` header the Relay believes
    /// about the address a Server connects from, named by its address, such
    /// as `10.0.0.5`, or by a network it is among, such as `10.0.0.0/8`.
    /// Give it once for each proxy. With none, the header is ignored.
    #[arg(long = "trusted-proxy", value_name = "ADDRESS")]
    trusted_proxies: Vec<TrustedProxy>,
    /// Requires every Account to have been logged in as, from any one of its
    /// Servers, within this many days: an Account not logged in as for
    /// longer is refused until one of its Servers logs in again, which
    /// restores them all. Unless given, a Login stands however long ago it
    /// was formed.
    #[arg(long, value_name = "DAYS")]
    fresh_login_days: Option<NonZeroU32>,
    /// The client ID of the GitHub App this Relay logs its users in through,
    /// which its operator registers with device login enabled. Unless given,
    /// the Relay logs nobody in.
    #[arg(long, value_name = "CLIENT_ID")]
    github_client_id: Option<String>,
    /// A GitHub user this Relay admits, by their username. The name is looked
    /// up at GitHub once, as the Relay first starts naming them, and whoever
    /// went by it then is admitted by it ever after, whatever they or anyone
    /// else go by later. The Relay refuses to start naming someone it cannot
    /// look up. Give it once for each user; one no longer given has their
    /// Account lapse as the Relay starts, and given again, is the same user.
    #[arg(
        long = "admit-user",
        value_name = "USERNAME",
        requires = "github_client_id"
    )]
    admitted_users: Vec<String>,
    /// The PEM file holding the private key of the GitHub App this Relay
    /// logs its users in through, as GitHub issued it, which the Relay
    /// checks the members of organizations with. It is read once, as the
    /// Relay starts.
    #[arg(long, value_name = "PATH", requires = "github_client_id")]
    github_private_key_file: Option<PathBuf>,
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
    #[arg(
        long = "admit-org",
        value_name = "ORGANIZATION",
        requires = "github_private_key_file"
    )]
    admitted_organizations: Vec<String>,
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
    recheck_minutes: Option<NonZeroU32>,
    /// How many Servers each Account may have logged in at this Relay: the
    /// Logins standing under it, a lapsed Account's among them, since nothing
    /// of it is forgotten. A login past it is refused until a Server of that
    /// Account forgets its Login — its user removing this Relay from it — or
    /// the operator removes one; a Server already logged in logs in again in
    /// the place it holds. A login phished for a stranger's Server costs the
    /// Account one of these places, and nothing more.
    #[arg(long, value_name = "LOGINS", default_value_t = suru_relay::LOGINS_PER_ACCOUNT)]
    logins_per_account: NonZeroU32,
    /// How many connections this Relay joins for each Account at once. A
    /// Server keeping a Remote in view through the Relay holds one, so an
    /// Account whose Servers each keep the others in view holds one for each
    /// ordered pair of them. A join past it is refused until one ends.
    #[arg(
        long,
        value_name = "CONNECTIONS",
        default_value_t = suru_relay::JOINED_CONNECTIONS_PER_ACCOUNT
    )]
    joined_connections_per_account: NonZeroU32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(suru_relay::log_layer(
            std::env::var("RUST_LOG").ok().as_deref(),
            std::io::stderr,
        ))
        .init();
    let CommandLine { database, command } = CommandLine::parse();
    match command {
        Command::Run(arguments) => run(database, arguments).await,
        Command::Operate(command) => {
            suru_relay::operate(&database, command, &mut std::io::stdout().lock()).await
        }
    }
}

async fn run(database: PathBuf, arguments: Arguments) -> Result<()> {
    let mut config = RelayConfig::new(arguments.listen, database, arguments.public_address)
        .with_trusted_proxies(arguments.trusted_proxies)
        .with_logins_per_account(arguments.logins_per_account)
        .with_joined_connections_per_account(arguments.joined_connections_per_account)
        .with_admission(
            Admission::nobody()
                .with_named_users(arguments.admitted_users)
                .with_organizations(arguments.admitted_organizations),
        );
    if let Some(minutes) = arguments.recheck_minutes {
        config = config.with_admission_interval(Duration::from_secs(u64::from(minutes.get()) * 60));
    }
    if let Some(days) = arguments.fresh_login_days {
        config = config
            .with_fresh_login_every(Duration::from_secs(u64::from(days.get()) * 24 * 60 * 60));
    }
    let provider: Arc<dyn IdentityProvider> = match arguments.github_client_id {
        Some(client_id) => {
            let mut app = GitHubApp::new(client_id);
            if let Some(path) = &arguments.github_private_key_file {
                app = app.with_private_key(GitHubAppKey::from_pem_file(path)?);
            }
            Arc::new(GitHub::new(app)?)
        }
        None => Arc::new(NoIdentityProvider),
    };
    suru_relay::start(config, provider)
        .await?
        .run_until_ctrl_c()
        .await
}
