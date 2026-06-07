// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable promises: externally-completed handles that survive a crash-resume.
//!
//! A [`DurablePromise`] represents a value that an *out-of-band* party will supply later — a
//! human-in-the-loop approval, an async A2A reply, or a subagent result. The awaiting execution can
//! crash and resume while the promise is still pending; on resume it re-derives the same
//! [`PromiseId`] for its program position (see [`PromiseId::derive`]) and re-attaches to the pending
//! `durable_promises` row rather than minting a fresh one.
//!
//! # The resolver token is the capability (INV-9)
//!
//! A `PromiseId` is **not** a bearer capability — it is derivable from the execution journal and may
//! appear in traces. The authority to resolve a promise is a separate 32-byte high-entropy *resolver
//! token*, generated once when the promise is created and held only inside the [`DurablePromise`]
//! value (zeroized on drop). Only its BLAKE3 hash — domain-separated and bound to
//! `(promise_id, execution_id)` — is persisted. [`DurableHandle::resolve`] re-derives that hash from
//! a presented token and compares it in **constant time**; a wrong token is rejected without ever
//! revealing whether it was close.
//!
//! The consumer is responsible for the INV-9 channel rule: a [`DurableHandle`] is an operator/A2A
//! surface and MUST NOT be reachable from an LLM tool. The LLM never sees the resolver token (it is
//! handed out of band when the promise is created), so it cannot resolve its own pending promises.

use std::marker::PhantomData;
use std::sync::Arc;

use serde::Serialize;
use zeroize::Zeroizing;

use crate::backend::DurableBackendEnum;
use crate::backend::local::now_unix_millis;
use crate::error::DurableError;
use crate::ids::{ExecutionId, PromiseId};

/// Length of a resolver token in bytes.
pub(crate) const RESOLVER_TOKEN_LEN: usize = 32;

/// Domain-separation context for the resolver-token hash (BLAKE3 `derive_key` mode).
const RESOLVER_CONTEXT: &str = "zeph-durable v1 promise resolver-token 2026";

/// The persisted state of a promise, read back by the resolve and await paths.
///
/// The `payload` is the AEAD-sealed resolved value when `resolved` is `true`; the backend opens it
/// with the promise-bound AAD. The stored `resolver_token_hash` is compared against a presented
/// token during resolution.
#[derive(Debug, Clone)]
pub(crate) struct PromiseRecord {
    /// The execution that created the promise — half of the resolver-token binding.
    pub(crate) execution_id: ExecutionId,
    /// BLAKE3 hash of the bound resolver token (the token itself is never stored).
    pub(crate) resolver_token_hash: [u8; 32],
    /// Whether the promise has been resolved.
    pub(crate) resolved: bool,
    /// The AEAD-sealed resolved value, present once `resolved` is `true`.
    pub(crate) payload: Option<Vec<u8>>,
}

/// Compute the domain-separated, position-bound hash of a resolver token.
///
/// Binding `(promise_id, execution_id)` into the hash means a token is meaningless against any other
/// promise even if leaked, and the fixed `derive_key` context keeps these hashes disjoint from every
/// other BLAKE3 use in the workspace. The returned [`blake3::Hash`] compares in constant time.
pub(crate) fn resolver_token_hash(
    promise_id: PromiseId,
    execution_id: ExecutionId,
    token: &[u8; RESOLVER_TOKEN_LEN],
) -> blake3::Hash {
    let mut input = [0u8; 16 + 16 + RESOLVER_TOKEN_LEN];
    input[..16].copy_from_slice(promise_id.as_uuid().as_bytes());
    input[16..32].copy_from_slice(execution_id.as_bytes());
    input[32..].copy_from_slice(token);
    blake3::Hash::from(blake3::derive_key(RESOLVER_CONTEXT, &input))
}

/// A typed handle to a value that an out-of-band party will resolve later.
///
/// Created by [`DurableContext::promise`](crate::DurableContext::promise) and consumed by
/// [`DurableContext::await_promise`](crate::DurableContext::await_promise). The type parameter `T`
/// ties the awaited result type to the creation site; `T` is a phantom (`fn() -> T`, so the handle
/// is unconditionally `Send + Sync` and owns no `T`).
///
/// On a *fresh* creation the handle carries the resolver token; hand it to the resolving channel via
/// [`resolver_token`](DurablePromise::resolver_token). On *resume* the original token was already
/// delivered out of band before the crash and is unrecoverable, so a resumed handle carries no token
/// ([`is_resumed`](DurablePromise::is_resumed) is `true`) — it can still be awaited.
#[derive(Debug)]
pub struct DurablePromise<T> {
    id: PromiseId,
    resolver_token: Option<Zeroizing<[u8; RESOLVER_TOKEN_LEN]>>,
    _t: PhantomData<fn() -> T>,
}

impl<T> DurablePromise<T> {
    /// Construct a freshly-created promise holding its resolver token.
    pub(crate) fn fresh(
        id: PromiseId,
        resolver_token: Zeroizing<[u8; RESOLVER_TOKEN_LEN]>,
    ) -> Self {
        Self {
            id,
            resolver_token: Some(resolver_token),
            _t: PhantomData,
        }
    }

    /// Construct a resumed promise whose token lives out of band (delivered before the crash).
    pub(crate) fn resumed(id: PromiseId) -> Self {
        Self {
            id,
            resolver_token: None,
            _t: PhantomData,
        }
    }

    /// The promise's identifier.
    #[must_use]
    pub fn id(&self) -> PromiseId {
        self.id
    }

    /// Borrow the resolver token to hand to the out-of-band resolving channel.
    ///
    /// Returns `None` for a resumed promise (the token was delivered before the crash and cannot be
    /// recovered). The token is secret: deliver it only over the operator/A2A channel, never to the
    /// LLM (INV-9).
    #[must_use]
    pub fn resolver_token(&self) -> Option<&[u8; RESOLVER_TOKEN_LEN]> {
        self.resolver_token.as_deref()
    }

    /// Whether this handle was reconstructed on resume (and therefore holds no token).
    #[must_use]
    pub fn is_resumed(&self) -> bool {
        self.resolver_token.is_none()
    }
}

/// The out-of-band entry point that resolves pending promises.
///
/// Cheap to clone (it holds only an `Arc` to the shared backend) and `Send + Sync`, so it can be
/// handed to an operator command handler or an A2A reply path. It deliberately exposes *only*
/// [`resolve`](DurableHandle::resolve): a holder can complete a promise given the matching token but
/// can neither create nor inspect executions.
#[derive(Clone, Debug)]
pub struct DurableHandle {
    backend: Arc<DurableBackendEnum>,
}

impl DurableHandle {
    /// Build a resolver handle over the shared backend.
    #[must_use]
    pub fn new(backend: Arc<DurableBackendEnum>) -> Self {
        Self { backend }
    }

    /// Resolve a promise by presenting its resolver token and the completion value (FR-DE-05).
    ///
    /// The token is hashed (domain-separated, bound to `(promise_id, execution_id)`) and compared in
    /// constant time against the stored hash. On a match the value is sealed and committed and any
    /// in-process waiter is woken; resolving an already-resolved promise with the correct token is a
    /// no-op success. A wrong token is rejected with [`DurableError::PromiseRejected`] and leaves the
    /// promise untouched.
    ///
    /// # Errors
    ///
    /// - [`DurableError::UnknownPromise`] if no promise with `id` exists.
    /// - [`DurableError::PromiseRejected`] if `resolver_token` does not authenticate.
    /// - [`DurableError::Serialize`] if `value` cannot be serialized, or a storage error from the
    ///   backend.
    pub async fn resolve<T: Serialize>(
        &self,
        id: PromiseId,
        resolver_token: &[u8; RESOLVER_TOKEN_LEN],
        value: T,
    ) -> Result<(), DurableError> {
        let span = tracing::info_span!("durable.promise.resolve", promise_id = %id.as_uuid());
        let _enter = span.enter();

        let record = self
            .backend
            .promise_state(id)
            .await?
            .ok_or(DurableError::UnknownPromise)?;

        // Constant-time authentication (INV-9): blake3::Hash equality is constant-time, so a wrong
        // token reveals no timing signal. Authenticate *before* the already-resolved short-circuit so
        // an attacker cannot use a resolved promise as an oracle.
        let presented = resolver_token_hash(id, record.execution_id, resolver_token);
        if presented != blake3::Hash::from(record.resolver_token_hash) {
            return Err(DurableError::PromiseRejected);
        }
        if record.resolved {
            return Ok(());
        }

        let payload = serde_json::to_vec(&value).map_err(|_| DurableError::Serialize {
            step: "promise.resolve",
        })?;
        self.backend
            .resolve_promise(id, record.execution_id, &payload, now_unix_millis())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::StepId;

    #[test]
    fn resolver_hash_binds_promise_and_execution() {
        let exec = ExecutionId::new();
        let promise = PromiseId::derive(exec, StepId::new(0));
        let token = [7u8; RESOLVER_TOKEN_LEN];

        let base = resolver_token_hash(promise, exec, &token);
        assert_eq!(
            base,
            resolver_token_hash(promise, exec, &token),
            "deterministic for fixed inputs"
        );
        // A different promise, execution, or token all change the hash.
        let other_promise = PromiseId::derive(exec, StepId::new(1));
        assert_ne!(base, resolver_token_hash(other_promise, exec, &token));
        assert_ne!(
            base,
            resolver_token_hash(promise, ExecutionId::new(), &token)
        );
        assert_ne!(
            base,
            resolver_token_hash(promise, exec, &[8u8; RESOLVER_TOKEN_LEN])
        );
    }

    #[test]
    fn fresh_promise_carries_token_resumed_does_not() {
        let fresh: DurablePromise<u32> =
            DurablePromise::fresh(PromiseId::new(), Zeroizing::new([1u8; RESOLVER_TOKEN_LEN]));
        assert!(fresh.resolver_token().is_some());
        assert!(!fresh.is_resumed());

        let resumed: DurablePromise<u32> = DurablePromise::resumed(PromiseId::new());
        assert!(resumed.resolver_token().is_none());
        assert!(resumed.is_resumed());
    }
}
