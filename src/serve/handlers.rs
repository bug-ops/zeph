// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::convert::Infallible;
use std::path::PathBuf;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use zeph_common::SessionId;
use zeph_core::serve::{SessionActor, SessionActorHandle, SessionCommand};

use super::AppState;
use super::agent_factory::build_agent_factory;

/// Attempt to reactivate a session that has no live actor (D-12, spec §9.3: "connect to absent
/// session: replay → spawn `SessionActor` → register → attach").
///
/// Before D-12, `POST /sessions/:id/prompt` and `GET /sessions/:id/events` returned `404` for
/// any session whose actor had ended (idle eviction, explicit delete via a *different* endpoint
/// racing this one, or a process restart) even though its durable log made it perfectly
/// resumable — contradicting `GET /sessions/:id`'s own documented claim ("the durable log allows
/// it to be resumed") and turning `[serve] session_idle_ttl_secs` eviction into a one-way trip.
/// This closes that gap: looks up the session's durable `acp_sessions` row, and if it exists,
/// replays its event log via [`build_agent_factory`] (which now always routes through
/// [`zeph_agent_persistence::hydrate_and_condense`], D-10/D-13) and spawns + registers a fresh
/// `SessionActor` exactly as `POST /sessions` does for a brand-new one.
///
/// Returns `None` — leaving the caller's `404`/`410` unchanged — when the session has no durable
/// record at all (a genuinely unknown id), has no linked `conversation_id` (a legacy session
/// session-persistence can't safely reconstruct), or the registry is already at
/// `[serve] max_sessions` capacity (mirrors `POST /sessions`' own `503` guard, just folded into
/// this `404` path since a capacity-rejected reactivation is still "not currently promptable").
#[tracing::instrument(
    name = "serve.handlers.reactivate_session",
    skip_all,
    level = "info",
    fields(session_id = session_id.as_str())
)]
async fn reactivate_session(
    state: &AppState,
    session_id: &SessionId,
) -> Option<SessionActorHandle> {
    if state.registry.len() >= state.max_sessions {
        tracing::warn!(
            session_id = %session_id.as_str(),
            max_sessions = state.max_sessions,
            "serve-sessions: reactivation rejected, at capacity"
        );
        return None;
    }

    let store = zeph_session::SessionStore::new(state.deps.memory.sqlite().pool().clone());
    let meta = store.get(session_id.as_str()).await.ok().flatten()?;
    let conversation_id = meta.conversation_id.map(zeph_memory::ConversationId)?;

    let build_agent =
        build_agent_factory(state.deps.clone(), session_id.clone(), conversation_id).await;
    let (handle, _blocking_handle) = SessionActor::spawn(
        &state.supervisor,
        &state.registry,
        session_id,
        build_agent,
        state.mailbox_capacity,
    );
    state.registry.insert(session_id.clone(), handle.clone());

    tracing::info!(session_id = %session_id.as_str(), "serve-sessions: session reactivated");
    Some(handle)
}

/// Response body for `GET /health`.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    live_sessions: usize,
}

/// Handler for the unauthenticated `GET /health` liveness check (spec §9.4).
pub(super) async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.started_at.elapsed().as_secs(),
        live_sessions: state.registry.len(),
    })
}

/// Response body for `POST /sessions`.
#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
    conversation_id: i64,
}

/// Handler for `POST /sessions` (spec §9.4): mints a new session, spawns its `SessionActor`, and
/// registers it in the [`zeph_core::serve::LiveSessionRegistry`].
///
/// Returns `503` when `[serve] max_sessions` is already reached.
#[tracing::instrument(name = "serve.handlers.create_session", skip_all, level = "info")]
pub(super) async fn create_session_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    if state.registry.len() >= state.max_sessions {
        tracing::warn!(
            max_sessions = state.max_sessions,
            "serve-sessions: POST /sessions rejected, at capacity"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let session_id = SessionId::generate();
    let conversation_id = state
        .deps
        .memory
        .sqlite()
        .create_conversation()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "serve-sessions: failed to mint conversation id");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let build_agent =
        build_agent_factory(state.deps.clone(), session_id.clone(), conversation_id).await;
    let (handle, _blocking_handle) = SessionActor::spawn(
        &state.supervisor,
        &state.registry,
        &session_id,
        build_agent,
        state.mailbox_capacity,
    );
    state.registry.insert(session_id.clone(), handle);

    tracing::info!(session_id = %session_id.as_str(), "serve-sessions: session created");
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: session_id.as_str().to_owned(),
            conversation_id: conversation_id.0,
        }),
    ))
}

/// Response body for `GET /sessions`.
#[derive(Serialize)]
struct ListSessionsResponse {
    sessions: Vec<String>,
}

/// Handler for `GET /sessions` (spec §9.4): lists ids of all live (in-memory) sessions.
///
/// Does not include sessions that exist durably (`[session] data_dir`) but have no live actor —
/// only `LiveSessionRegistry` entries. Listing sessions restored purely from the durable log is
/// covered by the existing `sessions list` CLI verb, not this endpoint.
#[tracing::instrument(name = "serve.handlers.list_sessions", skip_all, level = "debug")]
pub(super) async fn list_sessions_handler(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state
        .registry
        .ids()
        .into_iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    Json(ListSessionsResponse { sessions })
}

/// Response body for `GET /sessions/:id`.
#[derive(Serialize)]
struct SessionMetadataResponse {
    #[serde(flatten)]
    metadata: zeph_session::SessionMetadata,
    /// Whether this session currently has a live `SessionActor` in this process's
    /// [`zeph_core::serve::LiveSessionRegistry`] — distinct from `metadata.status`, which is the
    /// durably persisted lifecycle state and does not change when a process restarts.
    live: bool,
}

/// Handler for `GET /sessions/:id` (spec §9.4): returns durable session metadata (from
/// `acp_sessions`, via `SessionStore`) plus whether the session currently has a live actor.
///
/// Returns `404` only when the session is neither live nor known to `SessionStore` — a session
/// that was created, then its actor ended (idle eviction, explicit delete, or process restart),
/// still returns its metadata with `live: false`, since the durable log allows it to be resumed.
#[tracing::instrument(name = "serve.handlers.get_session", skip_all, level = "debug", fields(session_id = %id))]
pub(super) async fn get_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let session_id = SessionId::new(id);
    let live = state.registry.get(&session_id).is_some();

    let store = zeph_session::SessionStore::new(state.deps.memory.sqlite().pool().clone());
    let metadata = store.get(session_id.as_str()).await.map_err(|e| {
        tracing::error!(error = %e, "serve-sessions: failed to read session metadata");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    match metadata {
        Some(metadata) => Ok(Json(SessionMetadataResponse { metadata, live })),
        None if live => {
            // Registered in the live registry but not yet in `SessionStore` — a narrow race
            // between `SessionActor::spawn` returning and `build_agent_factory`'s
            // `SessionStore::create` completing inside the dedicated thread. Retrying shortly
            // resolves it; report `404` for this snapshot rather than fabricating metadata.
            Err(StatusCode::NOT_FOUND)
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Handler for `DELETE /sessions/:id` (spec §9.4): ends one live session gracefully by cancelling
/// its own [`zeph_core::serve::SessionActorHandle::cancel`] token — the same mechanism
/// `serve.evict` uses for idle eviction, just caller-initiated instead of TTL-triggered.
///
/// Returns `404` if the session is not currently live (already ended, or never existed).
#[tracing::instrument(name = "serve.handlers.delete_session", skip_all, level = "info", fields(session_id = %id))]
pub(super) async fn delete_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> StatusCode {
    let session_id = SessionId::new(id);
    match state.registry.remove(&session_id) {
        Some(handle) => {
            handle.cancel.cancel();
            tracing::info!(session_id = %session_id.as_str(), "serve-sessions: session deleted");
            StatusCode::NO_CONTENT
        }
        None => StatusCode::NOT_FOUND,
    }
}

/// Request body for `POST /sessions/:id/prompt`.
#[derive(Deserialize)]
pub(super) struct PromptRequest {
    text: String,
}

/// Handler for `POST /sessions/:id/prompt` (spec §9.4): submits a prompt to a live session's
/// mailbox. Fire-and-forget — the response streams separately via `GET /sessions/:id/events`.
///
/// Returns `202 Accepted` once queued, `404` if the session is neither live nor durably known
/// (or reactivation failed — see [`reactivate_session`], D-12), or `410 Gone` if the session's
/// mailbox has already closed (actor exiting/exited between the registry lookup and the send).
#[tracing::instrument(name = "serve.handlers.prompt_session", skip_all, level = "info", fields(session_id = %id))]
pub(super) async fn prompt_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PromptRequest>,
) -> StatusCode {
    let session_id = SessionId::new(id);
    let Some(handle) = state
        .registry
        .get_or_reactivate(&session_id, || reactivate_session(&state, &session_id))
        .await
    else {
        return StatusCode::NOT_FOUND;
    };
    if handle
        .tx
        .send(SessionCommand::Prompt { text: body.text })
        .await
        .is_ok()
    {
        StatusCode::ACCEPTED
    } else {
        tracing::warn!(
            session_id = %session_id.as_str(),
            "serve-sessions: prompt mailbox closed"
        );
        StatusCode::GONE
    }
}

/// Handler for `GET /sessions/:id/events` (spec §9.4): subscribes to a live session's
/// [`zeph_core::serve::SessionOutput`] broadcast and streams it as Server-Sent Events.
///
/// Multiple concurrent subscribers are supported (`broadcast::Sender` fans out to every
/// receiver) — e.g. an SSE client and a TUI `/conv` attach on the same session simultaneously.
/// A subscriber that falls behind (`BroadcastStreamRecvError::Lagged`) has those missed events
/// dropped rather than the connection closed — the durable event log (when `[session] enabled =
/// true`) is the source of truth for anything a lagged subscriber missed.
///
/// Returns `404` if the session is neither live nor durably known (or reactivation failed — see
/// [`reactivate_session`], D-12).
#[tracing::instrument(name = "serve.handlers.events_session", skip_all, level = "info", fields(session_id = %id))]
pub(super) async fn events_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let session_id = SessionId::new(id);
    let Some(handle) = state
        .registry
        .get_or_reactivate(&session_id, || reactivate_session(&state, &session_id))
        .await
    else {
        return Err(StatusCode::NOT_FOUND);
    };

    let stream = BroadcastStream::new(handle.tx_out.subscribe()).filter_map(|item| match item {
        Ok(output) => match Event::default().json_data(&output) {
            Ok(event) => Some(Ok(event)),
            Err(e) => {
                tracing::error!(error = %e, "serve-sessions: failed to serialize SessionOutput");
                None
            }
        },
        Err(_lagged) => None,
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Request body for `POST /sessions/:id/fork`.
#[derive(Deserialize, Default)]
pub(super) struct ForkRequest {
    /// Exclusive upper bound on copied events; `None` forks at the current end of the log
    /// (copies everything) — see [`zeph_session::ForkEngine::fork`].
    at_seq: Option<u64>,
}

/// Response body for `POST /sessions/:id/fork`.
#[derive(Serialize)]
struct ForkSessionResponse {
    session_id: String,
    conversation_id: i64,
    events_copied: usize,
}

/// Handler for `POST /sessions/:id/fork` (spec §9.4, §7.2): eager-copies the source session's
/// durable event log up to `at_seq` into a fresh child session via
/// [`zeph_session::ForkEngine::fork`], then immediately spawns a live `SessionActor` for the
/// child — the fork is usable via `/prompt`+`/events` right away, not just durably persisted.
///
/// Returns `404` if the source session has no durable log (`ForkEngine::fork`'s
/// `SessionError::NotFound`), `400` if `at_seq` exceeds the source log's event count
/// (`SessionError::InvalidForkPoint`), or `503` when `[serve] max_sessions` is already reached
/// (the child is a new live session, counted the same as any other).
#[tracing::instrument(name = "serve.handlers.fork_session", skip_all, level = "info", fields(session_id = %id))]
pub(super) async fn fork_session_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ForkRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if state.registry.len() >= state.max_sessions {
        tracing::warn!(
            max_sessions = state.max_sessions,
            "serve-sessions: POST /sessions/:id/fork rejected, at capacity"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let src_id = SessionId::new(id);
    let new_id = SessionId::generate();
    let data_dir = PathBuf::from(&state.deps.session_persistence_config.data_dir);
    let store = zeph_session::SessionStore::new(state.deps.memory.sqlite().pool().clone());

    let fork_result = zeph_session::ForkEngine::fork(
        &data_dir,
        src_id.as_str(),
        new_id.as_str(),
        body.at_seq,
        &store,
    )
    .await
    .map_err(|e| match e {
        zeph_session::SessionError::NotFound(_) => StatusCode::NOT_FOUND,
        zeph_session::SessionError::InvalidForkPoint(_) => StatusCode::BAD_REQUEST,
        e => {
            tracing::error!(error = %e, "serve-sessions: fork failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;

    let conversation_id = state
        .deps
        .memory
        .sqlite()
        .create_conversation()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "serve-sessions: failed to mint conversation id for fork");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let build_agent =
        build_agent_factory(state.deps.clone(), new_id.clone(), conversation_id).await;
    let (handle, _blocking_handle) = SessionActor::spawn(
        &state.supervisor,
        &state.registry,
        &new_id,
        build_agent,
        state.mailbox_capacity,
    );
    state.registry.insert(new_id.clone(), handle);

    tracing::info!(
        src_session_id = %src_id.as_str(),
        session_id = %new_id.as_str(),
        events_copied = fork_result.events_copied,
        "serve-sessions: session forked"
    );
    Ok((
        StatusCode::CREATED,
        Json(ForkSessionResponse {
            session_id: new_id.as_str().to_owned(),
            conversation_id: conversation_id.0,
            events_copied: fork_result.events_copied,
        }),
    ))
}
