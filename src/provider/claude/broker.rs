//! The Broker as a Claude Session's CLI process is handed it (ADR 0034).
//!
//! Claude takes MCP servers only through `--mcp-config`, which names a file or carries the JSON
//! itself. Carried inline, the Broker's token would ride the command line, which any local user can
//! read from the process table, so every launch is pointed at a file of its own instead: written to
//! the temporary directory under a name no one can guess, created exclusively so nothing already
//! there is written through, and readable by the user Suru runs as alone — `0600` on Unix, set as
//! the file is created, and on Windows a protected ACL granting that user alone, set before the
//! token is written. The file names one MCP server, `suru`, over HTTP, presenting the token as its
//! `Authorization` header, with a per-server `timeout` above the Broker's longest call, since
//! Claude's default would abort a call at 60 seconds. Beside it the launch allowlists
//! `mcp__suru__*`, so no Broker call raises an Approval, a native Subagent's included
//! (`docs/validation/0408-claude-http-mcp-long-calls.md`), and appends the Broker's note to the
//! Agent's system prompt with `--append-system-prompt`, as the handoff writes it for its Agent,
//! naming each Tool as Claude does — `mcp__suru__spawn_subagent` — which is what the Agent selects
//! a deferred MCP Tool by. A system prompt lives as long as the process it was given to, so every
//! launch, a `--resume` included, appends it again.
//!
//! A file lives as long as the process it was written for: it is removed once that process has
//! stopped, and in any case when the child holding it is dropped — replaced by the next launch, or
//! gone with its Session. A Suru killed outright leaves its files behind, but the tokens in them
//! died with it, since the Server holds its tokens in memory alone.
//!
//! None of this touches `--strict-mcp-config`: a Session loads the user's own MCP servers beside
//! the Broker, and an Errand's print mode keeps its locked flags and is handed no Broker at all.

use std::{
    ffi::OsString,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use super::claude_error;
use crate::{
    broker::{BROKER_CALL_TIMEOUT_MS, BROKER_SERVER_NAME},
    provider::{BrokerHandoff, ProviderError},
};

/// The MCP config file one launch is pointed at, removed when dropped, and the note that launch
/// appends to its Agent's system prompt.
pub(super) struct BrokerMcpConfig {
    path: PathBuf,
    note: String,
}

impl BrokerMcpConfig {
    /// Writes the MCP config `handoff` lowers onto, for one launch.
    pub(super) fn write(handoff: &BrokerHandoff) -> Result<Self, ProviderError> {
        Self::write_in(&std::env::temp_dir(), handoff)
    }

    /// Writes the same MCP config into `directory`, where a test keeps its files.
    pub(super) fn write_in(
        directory: &Path,
        handoff: &BrokerHandoff,
    ) -> Result<Self, ProviderError> {
        let path = directory.join(format!(
            "suru-claude-broker-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| unwritable(&path, &error))?;
        // Held from here, so a file that could not be finished is not left behind.
        let config = Self {
            path,
            note: handoff.instruction_note(tool_name),
        };
        #[cfg(windows)]
        crate::runtime::protect_current_user_file(&config.path)
            .map_err(|error| unwritable(&config.path, &error))?;
        file.write_all(mcp_config(handoff).to_string().as_bytes())
            .map_err(|error| unwritable(&config.path, &error))?;
        Ok(config)
    }

    /// The flags pointing a launch at this file, allowlisting every Tool the Broker serves, and
    /// telling the Agent the Broker is there.
    pub(super) fn launch_args(&self) -> [OsString; 6] {
        [
            OsString::from("--mcp-config"),
            self.path.clone().into_os_string(),
            OsString::from("--allowedTools"),
            OsString::from(tool_name("*")),
            OsString::from("--append-system-prompt"),
            OsString::from(&self.note),
        ]
    }

    /// Removes the file, once the process it was written for has stopped reading it. Removing it
    /// again, or finding it already gone, is no failure.
    pub(super) fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for BrokerMcpConfig {
    fn drop(&mut self) {
        self.remove();
    }
}

/// What Claude calls the Broker's Tool `tool`, or — for `*` — every Tool the Broker serves.
fn tool_name(tool: &str) -> String {
    format!("mcp__{BROKER_SERVER_NAME}__{tool}")
}

/// The failure a launch reports when its MCP config could not be written.
fn unwritable(path: &Path, error: &dyn std::fmt::Display) -> ProviderError {
    claude_error(format!(
        "could not write the Broker's MCP config to {}: {error}",
        path.display()
    ))
}

/// What the file says: the Broker as the one MCP server `suru`, reached over HTTP with the token
/// as its `Authorization` header.
fn mcp_config(handoff: &BrokerHandoff) -> Value {
    let (header, value) = handoff.authorization_header();
    json!({
        "mcpServers": {
            BROKER_SERVER_NAME: {
                "type": "http",
                "url": handoff.endpoint().as_str(),
                "headers": { header: value },
                "timeout": BROKER_CALL_TIMEOUT_MS,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::json;

    use super::{BrokerMcpConfig, mcp_config};
    use crate::{broker::BrokerRole, provider::BrokerHandoff};

    const ENDPOINT: &str = "http://127.0.0.1:1/broker";

    #[test]
    fn the_config_names_the_broker_over_http_with_its_token_and_call_timeout() {
        let handoff = BrokerHandoff::for_tests(ENDPOINT);
        assert_eq!(
            mcp_config(&handoff),
            json!({
                "mcpServers": {
                    "suru": {
                        "type": "http",
                        "url": ENDPOINT,
                        "headers": {"Authorization": handoff.token().bearer()},
                        "timeout": 900_000,
                    },
                },
            })
        );
    }

    #[test]
    fn a_launch_is_pointed_at_the_file_allowlists_the_brokers_tools_and_is_told_of_them() {
        let directory = tempfile::tempdir().expect("create a directory for the config");
        for role in [BrokerRole::Agent, BrokerRole::Sidekick] {
            let handoff = BrokerHandoff::for_tests_as(ENDPOINT, role);
            let config =
                BrokerMcpConfig::write_in(directory.path(), &handoff).expect("write the config");
            assert_eq!(
                config.launch_args(),
                [
                    OsString::from("--mcp-config"),
                    config.path.clone().into_os_string(),
                    OsString::from("--allowedTools"),
                    OsString::from("mcp__suru__*"),
                    OsString::from("--append-system-prompt"),
                    OsString::from(handoff.instruction_note(|tool| format!("mcp__suru__{tool}"))),
                ],
                "the note is the one the handoff writes for its Agent, as Claude names the Tools"
            );
        }
    }

    #[test]
    fn a_written_config_holds_the_token_for_its_owner_alone_until_it_is_dropped() {
        let directory = tempfile::tempdir().expect("create a directory for the config");
        let handoff = BrokerHandoff::for_tests(ENDPOINT);
        let config =
            BrokerMcpConfig::write_in(directory.path(), &handoff).expect("write the config");
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&config.path).expect("read the written config"),
        )
        .expect("the file is JSON");
        assert_eq!(written, mcp_config(&handoff));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&config.path)
                .expect("read the config's permissions")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "only the owner may read the token");
        }

        let path = config.path.clone();
        drop(config);
        assert!(!path.exists(), "the file goes with the launch it served");
    }

    #[test]
    fn every_launch_is_written_a_file_of_its_own_and_removing_one_twice_is_harmless() {
        let directory = tempfile::tempdir().expect("create a directory for the config");
        let handoff = BrokerHandoff::for_tests(ENDPOINT);
        let first = BrokerMcpConfig::write_in(directory.path(), &handoff).expect("write one");
        let second = BrokerMcpConfig::write_in(directory.path(), &handoff).expect("write two");
        assert_ne!(first.path, second.path);

        first.remove();
        assert!(!first.path.exists());
        assert!(
            second.path.exists(),
            "removing one launch's file leaves the other's"
        );
        first.remove();
        drop(first);
    }
}
