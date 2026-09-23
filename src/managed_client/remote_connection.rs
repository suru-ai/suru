//! Classification shared by every stream crossing a Remote proxy.

use crate::protocol::{RemoteStatus, SessionError, SessionErrorCode};

pub(super) enum RemoteConnectionFailure {
    Transient,
    Terminal {
        status: RemoteStatus,
        message: String,
    },
    Rejected(String),
    /// The Server holds no Session the request named — never did, or no
    /// longer does. A subscription that has already heard from that Session
    /// reads it as the Session having been deleted.
    Missing(String),
}

pub(super) async fn classify(
    response: reqwest::Result<reqwest::Response>,
) -> Result<reqwest::Response, RemoteConnectionFailure> {
    let response = response.map_err(|_| RemoteConnectionFailure::Transient)?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let error = response.json::<SessionError>().await.ok();
    match error {
        Some(error) if error.code == SessionErrorCode::PairingAuthenticationFailed => {
            Err(RemoteConnectionFailure::Terminal {
                status: RemoteStatus::Revoked,
                message: "Remote revoked this Pairing".to_owned(),
            })
        }
        Some(error) if error.code == SessionErrorCode::PairingProtocolMismatch => {
            Err(RemoteConnectionFailure::Terminal {
                status: RemoteStatus::ProtocolMismatch,
                message: error.message,
            })
        }
        Some(error) if error.code == SessionErrorCode::PairingConnectionFailed => {
            Err(RemoteConnectionFailure::Transient)
        }
        Some(_) | None if status.is_server_error() => Err(RemoteConnectionFailure::Transient),
        Some(error) if error.code == SessionErrorCode::SessionNotFound => {
            Err(RemoteConnectionFailure::Missing(error.message))
        }
        Some(error) => Err(RemoteConnectionFailure::Rejected(error.message)),
        None => Err(RemoteConnectionFailure::Rejected(format!(
            "Remote proxy rejected the request with {status}"
        ))),
    }
}
