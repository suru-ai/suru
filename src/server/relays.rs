//! The Relay routes: listing the Server's Relays, adding one by address,
//! beginning a login there and following it, choosing whether the Server
//! Serves through one, saying a Client has raised its Notice of one coming to
//! need a login, and removing one. They are server administration, refused
//! to Peers, and no Sidekick Tool offers them (ADR-0045).

use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Request, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures_util::{Stream, stream};
use tokio::sync::watch;

use super::{AppState, decode_session_command, is_authenticated, session_error_response};
use crate::{
    protocol::{
        AddRelayRequest, RELAY_LOGIN_EVENT, RelayLogin, RelayServeThroughRequest, ServerShutdown,
    },
    relays::RelayFailure,
};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/relays", get(list_relays).post(add_relay))
        .route("/v1/relays/{address}", axum::routing::delete(remove_relay))
        .route(
            "/v1/relays/{address}/login",
            get(follow_relay_login).post(begin_relay_login),
        )
        .route(
            "/v1/relays/{address}/serve-through",
            axum::routing::put(set_relay_serve_through),
        )
        .route(
            "/v1/relays/{address}/login-needed-notice",
            axum::routing::post(notice_relay_login_needed),
        )
}

async fn list_relays(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.relays.list()).into_response()
}

async fn add_relay(State(state): State<AppState>, request: Request) -> Response {
    let request =
        match decode_session_command::<AddRelayRequest>(&state, request, "Relay addition").await {
            Ok(request) => request,
            Err(response) => return response,
        };
    match state.relays.add(&request.address) {
        Ok(relay) => (StatusCode::CREATED, Json(relay)).into_response(),
        Err(failure) => failure_response(failure),
    }
}

async fn begin_relay_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(address): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.relays.begin_login(&address).await {
        Ok(login) => Json(login).into_response(),
        Err(failure) => failure_response(failure),
    }
}

/// Streams the latest login begun at a Relay: where it stands now, then each
/// change, ending once it has ended or the Server begins to stop.
async fn follow_relay_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(address): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.relays.follow_login(&address) {
        Ok(progress) => Sse::new(login_events(progress, state.shutdown.subscribe_to_intent()))
            .keep_alive(KeepAlive::new().interval(state.timings.sse_keepalive_interval))
            .into_response(),
        Err(failure) => failure_response(failure),
    }
}

async fn set_relay_serve_through(
    State(state): State<AppState>,
    AxumPath(address): AxumPath<String>,
    request: Request,
) -> Response {
    let request = match decode_session_command::<RelayServeThroughRequest>(
        &state,
        request,
        "Relay Serve-through choice",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state
        .relays
        .set_serve_through(&address, request.serve_through)
    {
        Ok(relay) => Json(relay).into_response(),
        Err(failure) => failure_response(failure),
    }
}

/// Records that a Client has raised its Notice of the Relay coming to need a
/// login, answering the Relay as it then stands.
async fn notice_relay_login_needed(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(address): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.relays.notice_login_needed(&address) {
        Ok(relay) => Json(relay).into_response(),
        Err(failure) => failure_response(failure),
    }
}

async fn remove_relay(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(address): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.relays.remove(&address).await {
        Ok(removal) => Json(removal).into_response(),
        Err(failure) => failure_response(failure),
    }
}

fn login_events(
    progress: watch::Receiver<RelayLogin>,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold(Some((progress, shutdown, true)), |following| async move {
        let (mut progress, mut shutdown, first) = following?;
        // A Server that begins to stop lets go of every follower at once,
        // rather than waiting on a login that may take minutes yet.
        if shutdown.borrow().is_some() {
            return None;
        }
        if !first {
            tokio::select! {
                biased;
                _ = shutdown.changed() => return None,
                changed = progress.changed() => if changed.is_err() {
                    return None;
                },
            }
        }
        let login = progress.borrow_and_update().clone();
        let event = Event::default()
            .event(RELAY_LOGIN_EVENT)
            .json_data(&login)
            .expect("a Relay login always serializes");
        let settled = login.outcome.is_settled();
        Some((Ok(event), (!settled).then_some((progress, shutdown, false))))
    })
}

fn failure_response(failure: RelayFailure) -> Response {
    session_error_response(failure.status(), failure.code, failure.message)
}
