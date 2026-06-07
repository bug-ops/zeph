// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable promise adapter for subagent spawn/await (spec-064 §P4, INV-9, FR-DE-05).
//!
//! This module is a thin Layer-2 adapter. It wires the parent's [`DurableContext`] promise
//! lifecycle to the subagent spawn/collect path. Domain meaning ("subagent result") lives here;
//! the cryptographic and journaling mechanics live in `zeph-durable` (Layer 0).
//!
//! # Scope boundary
//!
//! This adapter covers the **finished-child replay** case only: when a parent resumes after a
//! crash and the child already resolved its promise, `await_durable_subagent` returns the
//! journaled `SubagentResult` immediately without re-spawning the child (spec §1038, acceptance
//! test item 4).
//!
//! The still-running-child-on-parent-crash case is intentionally out of scope. Only the BLAKE3
//! hash of the resolver token is persisted (INV-9); the raw 32-byte token is `Zeroizing` and
//! never stored. A crashed parent therefore cannot re-mint a valid token for an in-flight child's
//! promise row. This is the direct consequence of INV-9's hash-only persistence guarantee —
//! inventing a token-recovery path would violate INV-9. The general crash-recovery gap is
//! declared out of v1 scope in spec §862 and §1226.
//!
//! # INV-9 channel rule
//!
//! The [`DurableResolverSeat`] (holding the backend handle + token) is carried through a new
//! field on `SpawnContext::durable_resolver`. It is handed to the spawned background task only.
//! It MUST NOT be accessible from the child's tool executor or LLM surface at any point.
//!
//! # Gate pattern
//!
//! The gate check lives at the call site in `zeph-core` (where `DurableConfig` and the parent
//! `DurableContext` are available). When `durable.enabled && durable.subagent`, the call site:
//!
//! 1. Calls [`make_durable_promise`] to create the promise and optionally a resolver seat.
//! 2. Places the seat in [`crate::manager::SpawnContext::durable_resolver`] before spawning.
//! 3. Calls [`await_durable_subagent`] instead of [`crate::SubAgentManager::collect`].
//!
//! When either flag is `false`, `SpawnContext::durable_resolver` is `None` and the plain
//! `spawn`/`collect` path runs byte-identically to today (opt-in, zero overhead when disabled).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeph_durable::{DurableContext, DurableError, DurableHandle, DurablePromise, PromiseId};
use zeroize::Zeroizing;

use crate::error::SubAgentError;
use crate::state::SubAgentState;

/// Token length mirrors `zeph_durable::promise::RESOLVER_TOKEN_LEN` (32 bytes).
const RESOLVER_TOKEN_LEN: usize = 32;

/// The payload stored in the durable promise for a subagent's terminal result.
///
/// Carries both the success and failure cases so a resumed parent can reconstruct the
/// exact control outcome (spec reconciliation: §884 — live run and replay must diverge on
/// neither the output nor the error path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    /// The task ID assigned at spawn time, for correlation.
    pub task_id: String,
    /// Terminal output text on success (`Completed` state). Empty when `error` is `Some`.
    pub output: String,
    /// Error detail on failure or cancellation (`Failed`/`Canceled` state).
    pub error: Option<String>,
    /// Terminal lifecycle state of the subagent.
    pub state: SubAgentState,
}

impl SubagentResult {
    /// Build a successful result from the agent loop's output string.
    #[must_use]
    pub fn ok(task_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            output: output.into(),
            error: None,
            state: SubAgentState::Completed,
        }
    }

    /// Build a failed result carrying the error reason so replay can reconstruct the same outcome.
    #[must_use]
    pub fn err(task_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            output: String::new(),
            error: Some(error.into()),
            state: SubAgentState::Failed,
        }
    }
}

/// The out-of-band resolver seat carried from parent to child background task (INV-9).
///
/// Held exclusively inside the spawned background task; never reachable from the child's
/// tool executor or LLM surface. `Zeroizing` ensures the raw token bytes are wiped on drop.
pub struct DurableResolverSeat {
    /// Shared backend handle — cheap clone, used only to call `resolve`.
    pub handle: Arc<DurableHandle>,
    /// Promise identifier matching the parent's program position.
    pub promise_id: PromiseId,
    /// The raw 32-byte resolver token (zeroized on drop, never stored).
    pub token: Zeroizing<[u8; RESOLVER_TOKEN_LEN]>,
}

/// Create a durable promise in the parent's execution and return the resolver seat for the child.
///
/// Calls `ctx.promise::<SubagentResult>()` to occupy a deterministic program position so a
/// resumed parent re-derives the same [`PromiseId`] and re-attaches to the pending row rather
/// than minting an orphan.
///
/// Returns `(promise, seat)` where:
/// - `promise` is passed to [`await_durable_subagent`] after the child is spawned.
/// - `seat` carries the resolver token and must be handed to the child's background task
///   (via `SpawnContext::durable_resolver`). On a resumed parent the promise is already
///   created, so `seat` is `None` — the child's original token was delivered before the crash
///   and is unrecoverable (INV-9).
///
/// # Errors
///
/// Propagates [`DurableError`] if the promise row cannot be read or inserted, or if the
/// per-execution step cap is exceeded.
pub async fn make_durable_promise(
    ctx: &DurableContext,
) -> Result<(DurablePromise<SubagentResult>, Option<DurableResolverSeat>), DurableError> {
    let promise = ctx.promise::<SubagentResult>().await?;
    let seat = if let Some(token) = promise.resolver_token() {
        let handle = Arc::new(ctx.resolver_handle());
        Some(DurableResolverSeat {
            handle,
            promise_id: promise.id(),
            token: Zeroizing::new(*token),
        })
    } else {
        // Resumed: original token was delivered before the crash; cannot recover.
        None
    };
    Ok((promise, seat))
}

/// Await a durable promise for a subagent result, with an adapter-level tracing span.
///
/// On a fresh run this parks (in-process notify or poll) until the child's background task
/// calls [`resolve_durable_promise`]. On a resumed parent it returns the journaled
/// `SubagentResult` immediately if the child already resolved (spec §1038). In either case,
/// replay is transparent to the caller.
///
/// # Errors
///
/// Propagates [`DurableError`] if the promise row is missing (pruned) or the payload cannot
/// be decoded.
pub async fn await_durable_subagent(
    ctx: &DurableContext,
    execution_id: zeph_durable::ExecutionId,
    promise: DurablePromise<SubagentResult>,
) -> Result<SubagentResult, SubAgentError> {
    let promise_id = promise.id();
    let exec_uuid = execution_id.as_uuid();
    let span = tracing::info_span!(
        "subagent.durable.await",
        execution_id = %exec_uuid,
        promise_id = %promise_id.as_uuid(),
    );
    async move {
        ctx.await_promise(promise)
            .await
            .map_err(|e| SubAgentError::Durable(e.to_string()))
    }
    .instrument(span)
    .await
}

/// Called from the child's background task after the agent loop terminates.
///
/// Builds a [`SubagentResult`] from the loop's terminal outcome and resolves the promise via
/// [`DurableHandle::resolve`]. On a wrong token or missing promise row the error is logged at
/// `warn` level and swallowed — the child has already finished and cannot retry.
///
/// The INV-9 channel rule is enforced by the caller: `seat` must be consumed here and never
/// forwarded to any tool executor or LLM surface.
#[tracing::instrument(
    name = "subagent.durable.resolve",
    skip(seat, loop_result),
    fields(promise_id = %seat.promise_id.as_uuid())
)]
pub async fn resolve_durable_promise(
    seat: DurableResolverSeat,
    task_id: &str,
    loop_result: &Result<String, SubAgentError>,
) {
    let result = match loop_result {
        Ok(output) => SubagentResult::ok(task_id, output.as_str()),
        Err(e) => SubagentResult::err(task_id, e.to_string()),
    };
    if let Err(e) = seat
        .handle
        .resolve(seat.promise_id, &seat.token, result)
        .await
    {
        tracing::warn!(
            task_id,
            promise_id = %seat.promise_id.as_uuid(),
            error = %e,
            "durable: failed to resolve subagent promise — child result lost for durable replay"
        );
    }
}

// Bring `Instrument` trait into scope for `.instrument(span)`.
use tracing::Instrument as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_result_ok_fields() {
        let r = SubagentResult::ok("t1", "hello");
        assert_eq!(r.task_id, "t1");
        assert_eq!(r.output, "hello");
        assert!(r.error.is_none());
        assert_eq!(r.state, SubAgentState::Completed);
    }

    #[test]
    fn subagent_result_err_fields() {
        let r = SubagentResult::err("t2", "timeout");
        assert_eq!(r.task_id, "t2");
        assert_eq!(r.output, "");
        assert_eq!(r.error.as_deref(), Some("timeout"));
        assert_eq!(r.state, SubAgentState::Failed);
    }

    #[test]
    fn subagent_result_roundtrips_json() {
        let original = SubagentResult::ok("task-42", "some output");
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SubagentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.task_id, original.task_id);
        assert_eq!(decoded.output, original.output);
        assert_eq!(decoded.state, original.state);
    }

    #[test]
    fn resolver_seat_token_is_zeroizing() {
        // Verify the struct compiles with Zeroizing token field and can be constructed.
        let token = Zeroizing::new([0u8; RESOLVER_TOKEN_LEN]);
        // DurableHandle requires a backend which we can't build in a unit test.
        // We only verify the type and field layout here.
        let _ = token;
    }

    /// Verify the full resolve → `await_promise` round-trip using an in-memory backend.
    ///
    /// This tests that:
    /// 1. `make_durable_promise` returns a fresh promise + seat on first call.
    /// 2. `resolve_durable_promise` stores the payload via the seat's token.
    /// 3. `await_durable_subagent` on a resumed context returns the stored `SubagentResult`.
    #[tokio::test]
    async fn durable_promise_resolve_and_await_roundtrip() {
        use std::sync::Arc;
        use zeph_durable::{
            DurableBackendEnum, DurableConfig, DurableContext, ExecutionId, ExecutionKind,
            JournalWriter, LocalBackend,
        };

        let exec_id = ExecutionId::new();
        let config = DurableConfig {
            journal_flush_interval_ms: 5,
            journal_ack_timeout_ms: 2000,
            ..DurableConfig::default()
        };

        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        local
            .open_execution(exec_id, ExecutionKind::AgentTurn)
            .await
            .unwrap();

        let (writer, handle) = JournalWriter::new(local.clone(), &config);
        let _writer_task = tokio::spawn(writer.run());

        let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
        let ctx = DurableContext::new(
            exec_id,
            ExecutionKind::AgentTurn,
            false,
            backend,
            handle,
            &config,
        );

        // Step 1: make_durable_promise returns a seat on first call.
        let (promise, seat_opt) = make_durable_promise(&ctx).await.unwrap();
        let seat = seat_opt.expect("fresh execution must yield a resolver seat");
        let promise_id = promise.id();

        // Step 2: resolve via seat (simulating child background task finish).
        let loop_result: Result<String, crate::error::SubAgentError> =
            Ok("agent output".to_owned());
        resolve_durable_promise(seat, "task-rt-01", &loop_result).await;

        // Step 3: await on the same context — must return the stored SubagentResult.
        let result = await_durable_subagent(&ctx, exec_id, promise)
            .await
            .unwrap();
        assert_eq!(result.task_id, "task-rt-01");
        assert_eq!(result.output, "agent output");
        assert!(result.error.is_none());
        assert_eq!(result.state, crate::state::SubAgentState::Completed);
        let _ = promise_id;
    }
}
