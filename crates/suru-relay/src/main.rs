use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use suru_relay::{
    OperatorCommand, Outcome,
    config::{self, Configuration, Global, RunArguments},
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// A Relay for Suru: carries Pairings between Servers that cannot reach each
/// other directly, for Servers logged in under one Account.
///
/// `suru-relay run` runs the Relay, as its configuration file — named by
/// --config, or by SURU_RELAY_CONFIG — and its flags say. Its operator lists
/// and removes the Accounts and Logins in its records with the other
/// commands, whether or not the Relay is running on them: a removal takes
/// effect on a running Relay at once, with no restart.
#[derive(Parser)]
#[command(version = suru_relay::version(), after_help = EXIT_STATUS)]
struct CommandLine {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the Relay.
    ///
    /// It writes one JSON line to standard output for each connection it
    /// joins, naming the Account and each Server, and its own diagnostics to
    /// standard error. It stops on Ctrl-C, and on Unix on SIGTERM, once the
    /// connection log has written what it owes.
    Run(Box<RunArguments>),
    #[command(flatten)]
    Operate(OperatorCommand),
}

/// How the binary exits, as its help says. A command line that cannot be
/// read exits with clap's own status, 2.
const EXIT_STATUS: &str = "Exit status: 0 when it did as asked; 1 when it could not, saying why \
                           on standard error — a configuration it cannot use among them; 2 when \
                           its command line cannot be read; 3 when a removal was made, and is \
                           refused from then on, but the Relay running on the records did not \
                           confirm in time that it had cut what stood on what was removed.";

/// The exit status of a command that could not do as it was asked.
const FAILED: u8 = 1;

/// The exit status of a removal whose cut the running Relay did not confirm
/// in time.
const CUT_UNCONFIRMED: u8 = 3;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(suru_relay::log_layer(
            std::env::var("RUST_LOG").ok().as_deref(),
            std::io::stderr,
        ))
        .init();
    let CommandLine { global, command } = CommandLine::parse();
    let outcome = match config::read(&global) {
        Ok(configuration) => {
            let database = config::database(&global, configuration.as_ref());
            match command {
                Command::Run(arguments) => run(*arguments, database, configuration.as_ref())
                    .await
                    .map(|()| Outcome::Done),
                Command::Operate(command) => {
                    suru_relay::operate(
                        &database,
                        command,
                        &mut std::io::stdout().lock(),
                        &mut std::io::stderr().lock(),
                    )
                    .await
                }
            }
        }
        Err(error) => Err(error),
    };
    match outcome {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::CutUnconfirmed) => ExitCode::from(CUT_UNCONFIRMED),
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::from(FAILED)
        }
    }
}

async fn run(
    arguments: RunArguments,
    database: std::path::PathBuf,
    configuration: Option<&Configuration>,
) -> Result<()> {
    let settings = config::settle(arguments, database, configuration)?;
    if settings.admit_nobody() {
        tracing::warn!(
            "the Relay admits nobody: its configuration names no GitHub user and no organization \
             to admit"
        );
    }
    let provider = settings.identity_provider()?;
    suru_relay::start(settings.relay_config(), provider)
        .await?
        .run_until_stopped()
        .await
}
