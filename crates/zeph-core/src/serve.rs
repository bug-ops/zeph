// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SessionActor` and `LiveSessionRegistry` for `zeph serve` (spec-068 §9, #5343).
//!
//! Each live conversation-session under `zeph serve` is a [`SessionActor`] task: it owns an
//! [`Agent<LoopbackChannel>`](crate::agent::Agent) exclusively, bridges [`SessionCommand`]s into
//! the agent's channel input, and forwards the channel's output as [`SessionOutput`] over a
//! broadcast channel any number of HTTP/SSE or TUI attachments can subscribe to.
//!
//! [`LiveSessionRegistry`] is pure bookkeeping (a `HashMap` behind a `parking_lot::Mutex`, never
//! held across `.await`) — it does not itself supervise tasks. [`SessionActor::spawn`] registers
//! a *coordinator* task under `TaskSupervisor` via `spawn_oneshot(name: Arc<str>, factory)`
//! (architect ruling D-7): the dynamic `serve.session.<id>` name and non-restarting `RunOnce`
//! policy are exactly right for a session actor (re-driving a torn turn/replay after a crash is
//! unsafe; recovery is a fresh spawn that replays the durable log from the last committed `seq`).
//!
//! `Agent<C>`'s futures are `!Send` (documented precedent: `crates/zeph-acp/src/transport/
//! stdio.rs`, "Agent futures are `!Send` and deeply nested") and `Agent<LoopbackChannel>` itself
//! cannot cross *any* thread boundary — not just `spawn_oneshot`'s `Send` bound but a
//! `std::thread::spawn` one too. [`SessionActor::spawn`] resolves this exactly as `zeph-acp`'s
//! `serve_stdio`/`transport/http.rs` do (architect ruling D-8): the `Agent` is constructed and
//! driven entirely inside a dedicated OS thread with its own `current_thread` runtime and
//! `LocalSet` — only `Send`-safe state (a `FnOnce(LoopbackChannel) -> Agent<LoopbackChannel>`
//! factory, mirroring `zeph-acp`'s `SendAgentSpawner`) crosses into that thread. The
//! `spawn_oneshot` task is a *thin coordinator*, never the agent driver itself: it awaits the
//! thread's completion signal and, on process-wide supervisor shutdown, forwards cancellation
//! onto the session's own [`CancellationToken`] (distinct from the supervisor's) so idle
//! eviction (spec §9.3) can cancel exactly one session without tearing down every live actor,
//! while `drive` only ever needs to select on a single cancellation source regardless of trigger.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zeph_common::SessionId;
use zeph_common::task_supervisor::{BlockingHandle, TaskSupervisor};

use crate::agent::Agent;
use crate::channel::{ChannelMessage, LoopbackChannel, LoopbackEvent, LoopbackHandle};

/// Stack size for the dedicated per-session thread — mirrors `zeph-acp`'s
/// `ACP_AGENT_STACK_SIZE` (Agent futures are deeply nested, ~512 KiB measured on overflow with
/// the default 2 MiB worker-thread stack).
const SESSION_ACTOR_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Buffer capacity for the `LoopbackChannel` constructed inside [`SessionActor::spawn`]'s
/// dedicated thread — matches the buffer used by every other headless `LoopbackChannel` consumer
/// (e.g. `zeph-acp`'s A2A bridge).
const LOOPBACK_CHANNEL_CAPACITY: usize = 8;

/// A command sent to a live [`SessionActor`] (spec §9.2).
#[derive(Debug)]
pub enum SessionCommand {
    /// Submit a new user prompt for this turn.
    Prompt {
        /// The prompt text.
        text: String,
    },
    /// Interrupt the agent's current operation (mirrors the existing `LoopbackHandle` cancel
    /// signal used by every other headless channel consumer).
    Cancel,
    /// Gracefully end the actor: closes the agent's channel input, letting `Agent::run` observe
    /// channel closure and exit its loop on its own rather than being aborted mid-turn.
    Shutdown,
}

/// An event streamed out of a live [`SessionActor`] (spec §9.2).
///
/// `Serialize`s as an adjacently tagged JSON object (`{"type": "token", "data": "..."}`) for
/// `GET /sessions/:id/events`'s SSE stream (spec §9.4) — adjacent rather than internal tagging
/// because [`Self::Token`] and [`Self::Error`] wrap a bare `String`, which cannot be flattened
/// into an internally tagged object.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionOutput {
    /// A streamed or full-message text chunk from the agent's response.
    Token(String),
    /// A tool call started.
    ToolCall {
        /// Name of the tool being invoked.
        tool_name: String,
        /// Opaque tool call ID assigned by the LLM.
        tool_call_id: String,
    },
    /// A tool call produced output.
    ToolResult {
        /// Name of the tool that produced this output.
        tool_name: String,
        /// Human-readable output text.
        display: String,
    },
    /// The current turn finished.
    TurnComplete,
    /// The agent loop ended with an error.
    Error(String),
}

/// Default broadcast channel capacity for [`SessionOutput`] — generous enough to absorb a burst
/// of streamed tokens between a slow subscriber's polls without lagging.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;

/// Owns a live [`Agent<LoopbackChannel>`](crate::agent::Agent) for one conversation-session
/// (spec §9.2).
///
/// This is a driver, not a stored value — [`SessionActor::spawn`] returns a
/// [`SessionActorHandle`] (cheap to clone, holds only channel senders) for
/// [`LiveSessionRegistry`] to track; the actor's own state lives entirely inside the spawned
/// task.
pub struct SessionActor;

impl SessionActor {
    /// Spawn a new `SessionActor` under `supervisor` (architect ruling D-8).
    ///
    /// `build_agent` is called *inside* a dedicated thread (see the module doc) with a freshly
    /// constructed `LoopbackChannel`, and must return the `Agent<LoopbackChannel>` built from it
    /// — never pass an already-built `Agent` in, since `Agent<LoopbackChannel>` is `!Send` and
    /// cannot cross the thread boundary. Mirrors `zeph-acp`'s `SendAgentSpawner`
    /// (`Arc<dyn Fn(...) -> Agent + Send + Sync>`): typical callers wrap an existing
    /// `AgentBuilder` pipeline (the same one used for CLI/TUI/Telegram/ACP sessions) in a closure
    /// capturing only `Send`-safe dependencies (provider, skill registry, tool executor, the
    /// session's `Arc<SessionEventLog>`, session id — all `Send`/`Sync`). `Agent::new` is sync,
    /// so `build_agent` is a plain sync closure — no async-construction-in-thread complexity.
    ///
    /// Registers a *coordinator* task under the dynamic name `serve.session.<id>` via
    /// [`TaskSupervisor::spawn_oneshot`] — visible through `supervisor.snapshot()`. The
    /// coordinator never touches the `!Send` `Agent`; it only awaits the dedicated thread's
    /// completion signal and, if the supervisor's own `CancellationToken` fires first (process
    /// shutdown), forwards cancellation onto the session's own token so `Self::drive` observes
    /// exactly one cancellation source regardless of trigger. Session actors intentionally do not
    /// auto-restart on panic or unexpected exit (`spawn_oneshot`'s `RestartPolicy::RunOnce`):
    /// re-driving a torn turn or replay in place is unsafe. Recovery is a fresh spawn (re-attach)
    /// that replays the durable log from the last committed `seq`, not an in-place restart.
    ///
    /// Returns the [`SessionActorHandle`] for [`LiveSessionRegistry`] plus the raw
    /// [`BlockingHandle`] — callers that need a forced-abort fallback (e.g. if a `serve.evict`
    /// idle-eviction task's cancellation overruns its TTL grace) should hold onto the latter. The
    /// graceful paths are sending [`SessionCommand::Shutdown`] over [`SessionActorHandle::tx`],
    /// or cancelling [`SessionActorHandle::cancel`] directly (what idle eviction uses, spec §9.3,
    /// to target exactly this session without affecting any other live actor).
    #[must_use]
    pub fn spawn<F>(
        supervisor: &TaskSupervisor,
        registry: &Arc<LiveSessionRegistry>,
        session_id: &SessionId,
        build_agent: F,
        mailbox_capacity: usize,
        resume_banner: Option<String>,
    ) -> (SessionActorHandle, BlockingHandle<()>)
    where
        F: FnOnce(LoopbackChannel) -> Agent<LoopbackChannel> + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(mailbox_capacity.max(1));
        let (tx_out, _first_subscriber) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);
        let tx_out_for_actor = tx_out.clone();
        let id_str = session_id.as_str().to_owned();

        // Per-session cancellation, distinct from the supervisor's process-wide token, so idle
        // eviction can target exactly this session (spec §9.3) without tearing down every live
        // actor. The coordinator forwards a supervisor-wide shutdown onto this same token.
        let session_cancel = CancellationToken::new();
        let session_cancel_for_thread = session_cancel.clone();
        let session_cancel_for_coordinator = session_cancel.clone();

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let thread_name = format!("serve-session-{id_str}");
        let thread_session_id = id_str.clone();
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .stack_size(SESSION_ACTOR_STACK_SIZE)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(
                            session_id = %thread_session_id,
                            error = %e,
                            "failed to build session actor tokio runtime"
                        );
                        let _ = done_tx.send(());
                        return;
                    }
                };
                let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
                let agent = build_agent(channel);
                let local = tokio::task::LocalSet::new();
                rt.block_on(local.run_until(Self::drive(
                    agent,
                    handle,
                    cmd_rx,
                    tx_out_for_actor,
                    session_cancel_for_thread,
                )));
                let _ = done_tx.send(());
            });

        if let Err(e) = spawn_result {
            tracing::error!(error = %e, "failed to spawn dedicated session actor thread");
        }

        let name: Arc<str> = Arc::from(format!("serve.session.{id_str}"));
        let supervisor_cancel = supervisor.cancellation_token();
        let registry_for_coordinator = Arc::clone(registry);
        let session_id_for_coordinator = session_id.clone();
        let tx_for_coordinator = cmd_tx.clone();
        let blocking_handle = supervisor.spawn_oneshot(name, move || {
            Self::coordinate(
                done_rx,
                supervisor_cancel,
                session_cancel_for_coordinator,
                registry_for_coordinator,
                session_id_for_coordinator,
                tx_for_coordinator,
            )
        });

        (
            SessionActorHandle {
                tx: cmd_tx,
                tx_out,
                last_active: Instant::now(),
                cancel: session_cancel,
                resume_banner_sent: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                pending_resume_banner: resume_banner.map(Arc::from),
            },
            blocking_handle,
        )
    }

    /// Thin `Send`-safe coordinator handed to [`TaskSupervisor::spawn_oneshot`]: never touches
    /// the `!Send` `Agent` — only a thread-completion signal and cancellation tokens. Forwards a
    /// process-wide supervisor shutdown onto the session's own [`CancellationToken`] so
    /// [`Self::drive`] (running on the dedicated thread) only ever needs to select on one
    /// cancellation source.
    ///
    /// M1 (impl-critic finding): reaps `registry`'s entry for `session_id` unconditionally once
    /// the dedicated thread signals completion — regardless of *why* it ended (agent panic,
    /// normal exit, or supervisor shutdown). Before this, [`LiveSessionRegistry::idle_candidates`]
    /// was the *only* reap path, and it requires `receiver_count() == 0`; a lingering `GET
    /// /sessions/:id/events` SSE subscriber (or a crashed actor whose stream simply stops
    /// emitting, never closing) could pin a dead session in the registry forever, so
    /// `POST /sessions/:id/prompt` would return `410 Gone` for it permanently with no path back
    /// to a live actor even under D-12's reactivation. Uses
    /// [`LiveSessionRegistry::remove_if_current`] (not a plain key-based `remove`) so a
    /// concurrent reactivation that has already `insert`ed a fresh handle under the same
    /// `session_id` is never evicted by this now-dead coordinator.
    async fn coordinate(
        done_rx: tokio::sync::oneshot::Receiver<()>,
        supervisor_cancel: CancellationToken,
        session_cancel: CancellationToken,
        registry: Arc<LiveSessionRegistry>,
        session_id: SessionId,
        tx: mpsc::Sender<SessionCommand>,
    ) {
        let mut done_rx = done_rx;
        tokio::select! {
            _ = &mut done_rx => {}
            () = supervisor_cancel.cancelled() => {
                session_cancel.cancel();
                let _ = done_rx.await;
            }
        }
        registry.remove_if_current(&session_id, &tx);
    }

    /// Bridge `cmd_rx` into the agent's `LoopbackChannel` input and forward the channel's output
    /// as [`SessionOutput`] over `tx_out`, while concurrently driving `agent.run()` to
    /// completion. A single `tokio::select!` loop — no raw `tokio::spawn` — per the non-blocking
    /// contract (CLAUDE.md Async & Background Tasks).
    ///
    /// Must run inside a `tokio::task::LocalSet` (or be `.await`ed directly, never spawned via
    /// a `Send`-bound spawner) because `Agent<C>`'s futures are `!Send`.
    ///
    /// `cancel` cancelling (via `TaskSupervisor::shutdown_all`) is treated the same as
    /// [`SessionCommand::Shutdown`] — a graceful flush (drop the channel's input sender, let
    /// `Agent::run` observe closure and exit, drain buffered output) rather than the abrupt
    /// task-abort `shutdown_all` would otherwise apply. Abrupt abort is still safe (INV-SP-2
    /// torn-tail truncation covers a torn trailing write) but an explicit flush avoids leaving a
    /// truncated trailing event on every shutdown.
    async fn drive(
        mut agent: Agent<LoopbackChannel>,
        handle: LoopbackHandle,
        mut cmd_rx: mpsc::Receiver<SessionCommand>,
        tx_out: broadcast::Sender<SessionOutput>,
        cancel: CancellationToken,
    ) {
        let LoopbackHandle {
            input_tx,
            mut output_rx,
            cancel_signal,
        } = handle;
        let mut input_tx = Some(input_tx);

        let mut agent_run = std::pin::pin!(agent.run());
        let mut agent_done = false;

        while !agent_done {
            tokio::select! {
                biased;
                result = &mut agent_run => {
                    agent_done = true;
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "session actor: agent run ended with error");
                        let _ = tx_out.send(SessionOutput::Error(e.to_string()));
                    }
                }
                Some(event) = output_rx.recv() => {
                    if let Some(output) = translate_loopback_event(event) {
                        let _ = tx_out.send(output);
                    }
                }
                () = cancel.cancelled(), if input_tx.is_some() => {
                    tracing::info!("session actor: supervisor shutdown, flushing and exiting");
                    input_tx = None;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SessionCommand::Prompt { text }) => {
                            if let Some(tx) = &input_tx {
                                let msg = ChannelMessage {
                                    text,
                                    attachments: Vec::new(),
                                    is_guest_context: false,
                                    is_from_bot: false,
                                    owner_key: None,
                                };
                                tracing::debug!("session actor: forwarding prompt to agent channel");
                                let _ = tx.send(msg).await;
                                tracing::debug!("session actor: prompt forwarded");
                            }
                        }
                        Some(SessionCommand::Cancel) => cancel_signal.notify_one(),
                        Some(SessionCommand::Shutdown) | None => {
                            // Drop the sender: `Agent::run`'s `next_event()` observes the closed
                            // channel and returns `Ok(None)`, ending the loop gracefully (see
                            // `crates/zeph-core/src/agent/mod.rs::next_event` doc comment).
                            input_tx = None;
                        }
                    }
                }
            }
        }

        // Drain any output events buffered just before the agent loop ended.
        while let Ok(event) = output_rx.try_recv() {
            if let Some(output) = translate_loopback_event(event) {
                let _ = tx_out.send(output);
            }
        }
    }
}

/// Maps a [`LoopbackEvent`] to a [`SessionOutput`], or `None` for event kinds not yet part of
/// the spec §9.2 `SessionOutput` schema (`Status`/`ThinkingChunk`/`Usage`/`SessionTitle`/`Plan`/
/// `Stop`) — dropped rather than guessed at an ad hoc extension.
fn translate_loopback_event(event: LoopbackEvent) -> Option<SessionOutput> {
    match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => {
            Some(SessionOutput::Token(text))
        }
        LoopbackEvent::Flush => Some(SessionOutput::TurnComplete),
        LoopbackEvent::ToolStart(ev) => Some(SessionOutput::ToolCall {
            tool_name: ev.tool_name.to_string(),
            tool_call_id: ev.tool_call_id,
        }),
        LoopbackEvent::ToolOutput(ev) => Some(SessionOutput::ToolResult {
            tool_name: ev.tool_name.to_string(),
            display: ev.display,
        }),
        _ => None,
    }
}

/// Bookkeeping handle for one live session, held by [`LiveSessionRegistry`] (spec §9.3).
///
/// Cheap to clone — `tx`/`tx_out` are `Arc`-backed channel senders.
#[derive(Clone)]
pub struct SessionActorHandle {
    /// Mailbox for [`SessionCommand`]s; same-session prompts are serialized by this mpsc's FIFO
    /// ordering (spec §9.2 concurrency policy — no separate turn-lock needed).
    pub tx: mpsc::Sender<SessionCommand>,
    /// Broadcast source new subscribers (SSE connections, TUI attach) subscribe to.
    pub tx_out: broadcast::Sender<SessionOutput>,
    /// Updated by [`LiveSessionRegistry::get`] on every lookup; read by
    /// [`LiveSessionRegistry::idle_candidates`] for TTL eviction.
    pub last_active: Instant,
    /// Cancelling this token ends exactly this session's actor gracefully — the same effect as
    /// sending [`SessionCommand::Shutdown`], but usable without an owned mpsc permit. What
    /// `serve.evict` idle eviction (spec §9.3) calls to target one session without affecting any
    /// other live actor. Distinct from the process-wide `TaskSupervisor` cancellation token.
    pub cancel: CancellationToken,
    /// Resume-banner single-emission guard (spec-068 §13.5, AC-24): when more than one
    /// display-owning channel attaches to the same live session, exactly one attach must
    /// render the banner. `Arc`-shared across every `Clone` of this handle so all attach
    /// paths observe the same flag. Use [`Self::claim_resume_banner`] rather than reading
    /// this directly.
    pub resume_banner_sent: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Resume-visibility banner text, computed once at session build time (spec-068 §13.5,
    /// AC-24) from the session's replayed history. `None` when `[session.resume] show_banner
    /// = false` or the session had no prior history to resume. Rendered by exactly one
    /// attach path, gated by [`Self::claim_resume_banner`] — see `GET /sessions/:id/events`
    /// (`events_session_handler`, `src/serve/handlers.rs`) for the sole production consumer.
    pub pending_resume_banner: Option<std::sync::Arc<str>>,
}

impl SessionActorHandle {
    /// Atomically claim the right to render the resume banner for this session.
    ///
    /// Returns `true` for exactly one caller across all attach paths sharing this handle
    /// (via `Clone`) — that caller renders the banner; every other caller (this attach or
    /// any subsequent one) gets `false` and must render nothing (spec-068 §13.5, AC-24).
    #[must_use]
    pub fn claim_resume_banner(&self) -> bool {
        self.resume_banner_sent
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Registry of live [`SessionActor`]s for `zeph serve` (spec §9.3).
///
/// Distinct from the TUI's own `SessionRegistry`/`SlotId` tab-switching abstraction
/// (`zeph-tui/src/session.rs`) — this is `zeph serve`-specific bookkeeping only. The internal
/// `sessions` mutex is never held across `.await`; [`Self::get_or_reactivate`]'s
/// `reactivation_lock` is a separate `tokio::sync::Mutex` deliberately held across `.await` for
/// its entire critical section — see that method's doc comment.
#[derive(Default)]
pub struct LiveSessionRegistry {
    sessions: Mutex<HashMap<SessionId, SessionActorHandle>>,
    /// Serializes [`Self::get_or_reactivate`]'s check-build-insert critical section (N1,
    /// impl-critic re-verify finding): without it, two concurrent requests for the same
    /// evicted-but-durable session both miss the fast-path `get`, both independently replay and
    /// spawn a `SessionActor` over the *same* `SessionEventLog` file, and the second `insert`
    /// silently orphans the first — two live writers on one INV-D2 single-writer log, corrupting
    /// it (duplicate `seq`s, a torn line that isn't the trailing one). A single process-wide lock
    /// (not a per-session map) is deliberate: reactivation is a rare recovery path, not the hot
    /// `get()` path every prompt/events call takes first — serializing distinct sessions'
    /// reactivations against each other is an acceptable trade for not needing to manage a
    /// second, dynamically-growing lock table's own cleanup/eviction lifecycle.
    reactivation_lock: tokio::sync::Mutex<()>,
}

impl LiveSessionRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a live session by id, refreshing its `last_active` timestamp on hit.
    #[must_use]
    pub fn get(&self, id: &SessionId) -> Option<SessionActorHandle> {
        let mut sessions = self.sessions.lock();
        let handle = sessions.get_mut(id)?;
        handle.last_active = Instant::now();
        Some(handle.clone())
    }

    /// Register a freshly spawned actor's handle, replacing (and dropping, without aborting) any
    /// prior entry under the same id.
    pub fn insert(&self, id: SessionId, handle: SessionActorHandle) {
        self.sessions.lock().insert(id, handle);
    }

    /// Look up a live session, reactivating it via `reactivate` if it isn't currently live —
    /// atomically, so two concurrent callers for the same absent `id` can never both win and
    /// double-spawn a `SessionActor` over the same durable log (N1, impl-critic re-verify
    /// finding: `SessionEventLog` is single-writer per INV-D2, and a plain `get()`-miss-then-
    /// spawn-then-`insert()` sequence has no such guarantee under concurrency).
    ///
    /// Fast path (the common case — session already live): a plain `get()`, no lock contention
    /// with any in-flight reactivation elsewhere. Slow path (session absent): acquires
    /// `reactivation_lock`, then re-checks `get()` *under the lock* — if a concurrent caller won
    /// the race and already reactivated `id` while this caller was waiting for the lock, that
    /// caller's `insert` is visible here and `reactivate` is never invoked a second time. Only
    /// the loser of the race skips calling `reactivate`; the winner runs it exactly once.
    ///
    /// `reactivate` is expected to build+spawn the actor and `insert` it into `self` before
    /// resolving — it runs to completion holding `reactivation_lock`, so its own `insert` is
    /// safely serialized against any other concurrent `get_or_reactivate` call for the same
    /// (or a different) id.
    #[tracing::instrument(
        name = "core.serve.registry.get_or_reactivate",
        skip_all,
        level = "debug",
        fields(session_id = id.as_str())
    )]
    pub async fn get_or_reactivate<F, Fut>(
        &self,
        id: &SessionId,
        reactivate: F,
    ) -> Option<SessionActorHandle>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<SessionActorHandle>>,
    {
        if let Some(handle) = self.get(id) {
            return Some(handle);
        }
        let _guard = self.reactivation_lock.lock().await;
        if let Some(handle) = self.get(id) {
            return Some(handle);
        }
        reactivate().await
    }

    /// Remove `id`'s entry only if it is still the exact handle identified by `tx` (compared via
    /// [`mpsc::Sender::same_channel`], which is `true` iff both senders share the same underlying
    /// channel).
    ///
    /// Used by [`SessionActor`]'s coordinator (M1) to reap its own registry entry on completion
    /// without racing a concurrent reactivation (D-12) that has already `insert`ed a *fresh*
    /// handle under the same id — a plain `remove(id)` there would delete the new entry too, an
    /// unconditional key-based removal cannot tell "my own now-dead entry" from "a different,
    /// live entry that happens to share this id".
    pub fn remove_if_current(
        &self,
        id: &SessionId,
        tx: &mpsc::Sender<SessionCommand>,
    ) -> Option<SessionActorHandle> {
        let mut sessions = self.sessions.lock();
        if sessions.get(id).is_some_and(|h| h.tx.same_channel(tx)) {
            sessions.remove(id)
        } else {
            None
        }
    }

    /// Remove a session's handle (used by idle eviction and explicit shutdown), returning it if
    /// present.
    pub fn remove(&self, id: &SessionId) -> Option<SessionActorHandle> {
        self.sessions.lock().remove(id)
    }

    /// Session ids with no attached broadcast receivers (`receiver_count() == 0`) whose
    /// `last_active` is at least `ttl` old — eviction candidates for a `serve.evict` task
    /// (spec §9.3).
    #[must_use]
    pub fn idle_candidates(&self, ttl: Duration) -> Vec<SessionId> {
        let sessions = self.sessions.lock();
        let now = Instant::now();
        sessions
            .iter()
            .filter(|(_, handle)| {
                handle.tx_out.receiver_count() == 0 && now.duration_since(handle.last_active) >= ttl
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Ids of all live sessions currently tracked, in arbitrary order.
    #[must_use]
    pub fn ids(&self) -> Vec<SessionId> {
        self.sessions.lock().keys().cloned().collect()
    }

    /// Number of live sessions currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().len()
    }

    /// `true` when no sessions are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn make_handle() -> SessionActorHandle {
        let (tx, _rx) = mpsc::channel(4);
        let (tx_out, _sub) = broadcast::channel(4);
        SessionActorHandle {
            tx,
            tx_out,
            last_active: Instant::now(),
            cancel: CancellationToken::new(),
            resume_banner_sent: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_resume_banner: None,
        }
    }

    /// Regression test for AC-24 (spec-068 §13.5): when multiple display-owning channels
    /// attach to the same live session, exactly one attach must win the resume banner claim.
    #[test]
    fn claim_resume_banner_wins_exactly_once_across_clones() {
        let handle = make_handle();
        let attach_a = handle.clone();
        let attach_b = handle.clone();

        assert!(
            attach_a.claim_resume_banner(),
            "first attach must win the claim"
        );
        assert!(
            !attach_b.claim_resume_banner(),
            "second attach (sharing the same underlying flag via Clone) must not win"
        );
        assert!(
            !handle.claim_resume_banner(),
            "a third read via the original handle must also see the claim as already taken"
        );
    }

    #[test]
    fn registry_insert_and_get_round_trips() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        registry.insert(id.clone(), make_handle());
        assert!(registry.get(&id).is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_get_missing_returns_none() {
        let registry = LiveSessionRegistry::new();
        assert!(registry.get(&SessionId::new("nope")).is_none());
    }

    #[test]
    fn registry_remove_drops_entry() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        registry.insert(id.clone(), make_handle());
        assert!(registry.remove(&id).is_some());
        assert!(registry.get(&id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_remove_if_current_drops_matching_entry() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        let handle = make_handle();
        let tx = handle.tx.clone();
        registry.insert(id.clone(), handle);

        assert!(registry.remove_if_current(&id, &tx).is_some());
        assert!(registry.get(&id).is_none());
    }

    /// M1 regression: a stale coordinator's `remove_if_current` (its own now-dead handle's `tx`)
    /// must NOT evict a fresh entry a concurrent reactivation (D-12) already `insert`ed under the
    /// same id — the exact race a plain key-based `remove` would lose.
    #[test]
    fn registry_remove_if_current_ignores_stale_tx_after_reactivation() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        let stale_handle = make_handle();
        let stale_tx = stale_handle.tx.clone();
        registry.insert(id.clone(), stale_handle);

        // A concurrent reactivation replaces the entry with a fresh handle under the same id.
        registry.insert(id.clone(), make_handle());

        // The stale coordinator's reap call must be a no-op — the fresh entry survives.
        assert!(registry.remove_if_current(&id, &stale_tx).is_none());
        assert!(
            registry.get(&id).is_some(),
            "a stale coordinator must never evict a concurrently-reactivated entry"
        );
    }

    /// N1 regression (impl-critic re-verify finding): a genuine concurrency test, not a
    /// sequential table-driven one — real `tokio::spawn` tasks race `get_or_reactivate` for the
    /// same absent session id, each `.await`ing a `yield_now()` inside its `reactivate` closure
    /// so they actually interleave (without the yield, the first task could run its whole
    /// closure to completion before the second is even polled, defeating the point of the test).
    /// Before the `reactivation_lock`, every task's fast-path `get()` would miss and every task
    /// would run its `reactivate` closure — spawning N independent `SessionActorHandle`s that
    /// each `insert` under the same id, the exact double-spawn-over-one-log scenario N1
    /// describes. With the fix, exactly one task's closure must run to completion.
    #[tokio::test]
    async fn get_or_reactivate_serializes_concurrent_reactivation_for_the_same_id() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let registry = Arc::new(LiveSessionRegistry::new());
        let id = SessionId::new("race-test");
        let reactivate_calls = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let registry = Arc::clone(&registry);
            let id = id.clone();
            let reactivate_calls = Arc::clone(&reactivate_calls);
            tasks.push(tokio::spawn(async move {
                registry
                    .get_or_reactivate(&id, || {
                        let registry = Arc::clone(&registry);
                        let id = id.clone();
                        let reactivate_calls = Arc::clone(&reactivate_calls);
                        async move {
                            // Force a real interleaving window — without this, tasks could
                            // resolve strictly in spawn order without ever actually contending
                            // for `reactivation_lock`.
                            tokio::task::yield_now().await;
                            reactivate_calls.fetch_add(1, Ordering::SeqCst);
                            let handle = make_handle();
                            registry.insert(id, handle.clone());
                            Some(handle)
                        }
                    })
                    .await
            }));
        }

        for task in tasks {
            assert!(
                task.await.unwrap().is_some(),
                "every concurrent caller must resolve to a live handle, win or lose the race"
            );
        }

        assert_eq!(
            reactivate_calls.load(Ordering::SeqCst),
            1,
            "exactly one concurrent caller may run the reactivation closure — a second run means \
             two SessionActors would have been spawned over the same durable log (N1)"
        );
    }

    #[test]
    fn registry_ids_lists_all_tracked_sessions() {
        let registry = LiveSessionRegistry::new();
        assert!(registry.ids().is_empty());
        registry.insert(SessionId::new("s1"), make_handle());
        registry.insert(SessionId::new("s2"), make_handle());
        let mut ids: Vec<String> = registry
            .ids()
            .into_iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["s1".to_owned(), "s2".to_owned()]);
    }

    #[test]
    fn registry_idle_candidates_requires_no_subscribers_and_expired_ttl() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        let mut handle = make_handle();
        // No subscriber to tx_out, but last_active is "now" — not yet past a long TTL.
        handle.last_active = Instant::now();
        registry.insert(id.clone(), handle);
        assert!(registry.idle_candidates(Duration::from_hours(1)).is_empty());
    }

    #[test]
    fn registry_idle_candidates_skips_sessions_with_active_subscribers() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        let mut handle = make_handle();
        handle.last_active = Instant::now()
            .checked_sub(Duration::from_secs(9999))
            .unwrap();
        let _subscriber = handle.tx_out.subscribe();
        registry.insert(id, handle);
        assert!(registry.idle_candidates(Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn registry_idle_candidates_returns_expired_unattached_sessions() {
        let registry = LiveSessionRegistry::new();
        let id = SessionId::new("s1");
        let mut handle = make_handle();
        handle.last_active = Instant::now()
            .checked_sub(Duration::from_secs(9999))
            .unwrap();
        registry.insert(id.clone(), handle);
        let candidates = registry.idle_candidates(Duration::from_secs(1));
        assert_eq!(candidates, vec![id]);
    }

    #[tokio::test]
    async fn session_actor_drive_shuts_down_cleanly_on_command() {
        use crate::agent::Agent;
        use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};

        // `LoopbackChannel::pair` gives exactly the (channel, handle) split `SessionActor::drive`
        // bridges between — the same split every other headless channel consumer (A2A) uses.
        let (channel, handle) = LoopbackChannel::pair(8);
        let provider = mock_provider(vec!["ok".to_owned()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent: Agent<LoopbackChannel> =
            Agent::new(provider, channel, registry, None, 5, executor);

        let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>(4);
        let (tx_out, _sub) = broadcast::channel::<SessionOutput>(16);

        // `Agent<C>`'s futures are `!Send` (see the module doc), so `drive`'s future cannot be
        // handed to `tokio::spawn`. Pre-queue both commands into the bounded mpsc buffer, then
        // `.await` `drive` directly in this test task — no cross-thread Send requirement applies
        // to a future that is only ever polled in place, never spawned.
        cmd_tx
            .send(SessionCommand::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .unwrap();
        cmd_tx.send(SessionCommand::Shutdown).await.unwrap();
        drop(cmd_tx);

        // `drive`'s future embeds the whole `Agent` state (large) — box it per
        // `clippy::large_futures` rather than growing this test task's stack.
        tokio::time::timeout(
            Duration::from_secs(10),
            Box::pin(SessionActor::drive(
                agent,
                handle,
                cmd_rx,
                tx_out,
                CancellationToken::new(),
            )),
        )
        .await
        .expect("drive must finish within the timeout");
    }

    #[tokio::test]
    async fn session_actor_spawn_runs_on_dedicated_thread_and_shuts_down() {
        use crate::agent::Agent;
        use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};

        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let registry = Arc::new(LiveSessionRegistry::new());
        let session_id = SessionId::new("spawn-test");

        // `build_agent` is `Send` (captures only `Send`-safe test doubles); the `Agent` it
        // constructs is built *inside* the dedicated thread `spawn` creates from the
        // caller-supplied `LoopbackChannel`, never crossing a thread boundary itself.
        let (handle, blocking_handle) = SessionActor::spawn(
            &supervisor,
            &registry,
            &session_id,
            move |channel| {
                let provider = mock_provider(vec!["ok".to_owned()]);
                let registry = create_test_registry();
                let executor = MockToolExecutor::no_tools();
                let agent: Agent<LoopbackChannel> =
                    Agent::new(provider, channel, registry, None, 5, executor);
                agent
            },
            4,
            None,
        );

        handle
            .tx
            .send(SessionCommand::Prompt {
                text: "hello".to_owned(),
            })
            .await
            .unwrap();
        handle.tx.send(SessionCommand::Shutdown).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), blocking_handle.join())
            .await
            .expect("session actor must finish within the timeout")
            .expect("session actor task must not panic or be aborted");

        // M1 (impl-critic finding): the coordinator must reap its own registry entry once the
        // dedicated thread signals completion, not only via `idle_candidates`'s
        // `receiver_count() == 0` TTL path — otherwise a session whose actor died with no
        // eviction ever running stays permanently registered as "live" with a dead mailbox.
        assert!(
            registry.get(&session_id).is_none(),
            "registry entry must be reaped once the session actor's coordinator completes"
        );
    }

    #[tokio::test]
    async fn session_actor_handle_cancel_shuts_down_without_supervisor_shutdown() {
        use crate::agent::Agent;
        use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};

        // Regression test for the D-8 per-session cancellation path: `serve.evict` idle eviction
        // cancels one session's own `SessionActorHandle::cancel` — it must terminate that actor
        // without the supervisor's own token ever being cancelled (i.e. without a process-wide
        // shutdown), proving the two cancellation sources are genuinely independent.
        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let registry = Arc::new(LiveSessionRegistry::new());
        let session_id = SessionId::new("cancel-test");

        let (handle, blocking_handle) = SessionActor::spawn(
            &supervisor,
            &registry,
            &session_id,
            move |channel| {
                let provider = mock_provider(vec!["ok".to_owned()]);
                let registry = create_test_registry();
                let executor = MockToolExecutor::no_tools();
                Agent::new(provider, channel, registry, None, 5, executor)
            },
            4,
            None,
        );

        // No `SessionCommand::Shutdown` sent — cancel the per-session token directly, as idle
        // eviction would.
        handle.cancel.cancel();

        tokio::time::timeout(Duration::from_secs(10), blocking_handle.join())
            .await
            .expect("session actor must finish within the timeout after its own token cancels")
            .expect("session actor task must not panic or be aborted");
    }

    /// Samples this process's own RSS via `sysinfo` — the same technique
    /// `system_metrics::spawn_system_metrics_task` uses in production — so results are directly
    /// comparable to other in-process RSS measurements.
    fn sample_rss(sys: &mut sysinfo::System, pid: sysinfo::Pid) -> u64 {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).map_or(0, sysinfo::Process::memory)
    }

    // TODO(critic): decompose idle floor (stack-resident vs Agent heap vs skill registry) and
    // add a production-realistic (non-mock) upper-bound variant.
    //
    /// NFR-P7 follow-up (#5840): the #5445 standalone harness measured `SessionActor::spawn`'s
    /// structural housing cost in isolation (thread stack, runtime, channels — ~65-93 KiB/actor,
    /// see `specs/068-session-persistence/nfr.md`) but could not construct a real
    /// `Agent<LoopbackChannel>`, which is private to this crate. This test closes that gap: it
    /// spawns real `SessionActor`s wrapping real (mock-provider-backed, since no live LLM/Qdrant
    /// is needed to measure idle housing) `Agent<LoopbackChannel>` instances in-process, so the
    /// measured RSS also includes `Agent`'s own owned state — not just the actor's housing.
    ///
    /// **This is a zero-conversation-turn floor** (no prompts are ever sent), so the ~875-905 KiB
    /// measured here is *not* `Agent`'s message-history buffer, which is empty throughout. The
    /// dominant contributor is not yet decomposed (see the `TODO` above) but is more likely extra
    /// resident pages of the actor's 8 MiB thread stack ([`SESSION_ACTOR_STACK_SIZE`]) touched by
    /// the deeper `Agent::new`/`drive` call chain, and/or the shared `SkillRegistry` load —
    /// neither of which #5445's Agent-less harness ever touched.
    ///
    /// **Lower bound, not an upper bound**: `mock_provider` has no HTTP client/connection pool,
    /// [`MockToolExecutor::no_tools`] carries no tool definitions, and the shared registry below
    /// loads exactly one trivial skill with no embedding vectors. A production session's real
    /// `SkillRegistry` (many skills + embeddings), real provider (reqwest client + pool), and any
    /// `SemanticMemory` state will all measure higher than what this test reports — a pass here is
    /// not proof that a production idle session stays under NFR-P7's budget.
    ///
    /// **Composite vs. NFR-P7's own scope**: `specs/068-session-persistence/nfr.md`'s NFR-P7 is
    /// defined as housing-only (thread stack + runtime + channels); the composite this test
    /// measures (housing + `Agent`'s owned state) is a distinct, larger quantity the spec's #5445
    /// rationale explicitly separates out. The `assert!` below reuses NFR-P7's 1 MiB number as a
    /// convenience threshold for this composite measurement, not as a formal claim that NFR-P7
    /// (as specified) is satisfied — see the "NFR-P7 follow-up (#5840)" rationale note in
    /// `nfr.md` for the recorded distinction. This test is also `#[ignore]`d and matched by no CI
    /// workflow filter, so it never runs automatically — it is a manual tripwire for whoever
    /// invokes it by hand, not an automated regression gate.
    ///
    /// Mirrors the #5445 harness's approach of measuring marginal RSS across growing cumulative
    /// batches (10, 25, 50, 100 actors) so one-time fixed costs (allocator warmup, first-touch
    /// page faults) amortize out. The reported floor is the delta between the *last two*
    /// checkpoints (50→100), not an earlier window — the 50→100 and 25→50 deltas agree to within
    /// noise (confirming convergence), whereas the 10→25 window still carries first-batch fixed
    /// costs and reads noticeably higher; using the converged tail avoids attributing one-time
    /// warmup cost to the per-session marginal figure.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns up to 100 real OS threads; run explicitly to verify NFR-P7, e.g. \
                `cargo nextest run -p zeph-core -E 'test(nfr_p7)' --run-ignored ignored-only \
                --no-capture`"]
    async fn nfr_p7_real_agent_idle_session_memory_floor() {
        use crate::agent::Agent;
        use crate::agent::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};

        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let registry = Arc::new(LiveSessionRegistry::new());
        let pid = sysinfo::get_current_pid().expect("current pid must be resolvable");
        let mut sys = sysinfo::System::new();

        // Shared across every spawned Agent, mirroring production (`src/serve/agent_factory.rs`'s
        // `build_agent_factory`, which passes one `Clone`-shared `deps.registry` to
        // `Agent::new_with_registry_arc` for every session) rather than each actor building its
        // own private registry via the simpler `Agent::new`. A 101st real session costs one `Arc`
        // clone, not a whole new registry — this keeps the measured marginal cost representative
        // of production instead of overstating it with a per-actor registry that doesn't scale.
        let skill_registry = Arc::new(parking_lot::RwLock::new(create_test_registry()));

        let mut actors = Vec::new();
        let mut checkpoints: Vec<(usize, u64)> = Vec::new();

        for target in [10usize, 25, 50, 100] {
            while actors.len() < target {
                let session_id = SessionId::new(format!("nfr-p7-{}", actors.len()));
                let shared_registry = Arc::clone(&skill_registry);
                let (handle, blocking) = SessionActor::spawn(
                    &supervisor,
                    &registry,
                    &session_id,
                    move |channel| {
                        let provider = mock_provider(vec!["ok".to_owned()]);
                        let embedding_provider = provider.clone();
                        let executor = MockToolExecutor::no_tools();
                        Agent::new_with_registry_arc(
                            provider,
                            embedding_provider,
                            channel,
                            shared_registry,
                            None,
                            5,
                            executor,
                        )
                    },
                    4,
                    None,
                );
                actors.push((handle, blocking));
            }
            // Let newly spawned dedicated threads finish constructing their Agent and settle into
            // `drive`'s idle `select!` loop before sampling.
            tokio::time::sleep(Duration::from_millis(200)).await;
            checkpoints.push((target, sample_rss(&mut sys, pid)));
        }

        for (handle, _) in &actors {
            let _ = handle.tx.send(SessionCommand::Shutdown).await;
        }
        for (_, blocking) in actors {
            let _ = tokio::time::timeout(Duration::from_secs(10), blocking.join()).await;
        }

        let (n_prev, rss_prev) = checkpoints[checkpoints.len() - 2];
        let (n_last, rss_last) = checkpoints[checkpoints.len() - 1];
        let marginal_per_session = rss_last.saturating_sub(rss_prev) / (n_last - n_prev) as u64;

        eprintln!("NFR-P7 real-Agent memory floor checkpoints: {checkpoints:?}");
        eprintln!(
            "NFR-P7 real-Agent per-session marginal floor (housing + Agent owned state, \
             synthetic mock-backed lower bound): {marginal_per_session} bytes ({} KiB)",
            marginal_per_session / 1024
        );

        assert!(
            marginal_per_session < 1_048_576,
            "NFR-P7 composite budget (informational threshold, see nfr.md's #5840 rationale) \
             exceeded: measured {marginal_per_session} bytes/session >= 1 MiB \
             (checkpoints: {checkpoints:?})"
        );
    }
}
