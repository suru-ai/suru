//! Classification shared by every stream crossing a Remote proxy.

use crate::protocol::{RemoteStatus, SessionError, SessionErrorCode};

pub(super) enum RemoteConnectionFailure {
    Transient,
    Terminal {
        status: RemoteStatus,
        message: String,
    },
    Rejected(String),
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
        Some(error) => Err(RemoteConnectionFailure::Rejected(error.message)),
        None => Err(RemoteConnectionFailure::Rejected(format!(
            "Remote proxy rejected the request with {status}"
        ))),
    }
}
