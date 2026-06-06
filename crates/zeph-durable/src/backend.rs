// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The sealed execution-backend abstraction and its enum-dispatch front door.
//!
//! A backend is the persistence engine behind a durable execution: it journals control flow and,
//! for the local backend, owns the dedicated `durable.db` pool. The [`ExecutionBackend`] trait is
//! **sealed** (it requires [`crate::sealed::Sealed`]), so only backends declared inside this crate
//! can implement it. External crates never name a concrete backend; they hold a
//! [`DurableBackendEnum`] and dispatch through it.
//!
//! # Why enum dispatch instead of `Box<dyn ExecutionBackend>`
//!
//! The journal append path is hot. A trait object would force a virtual call and a heap allocation
//! per dispatch; [`DurableBackendEnum`] resolves the backend with a single `match` and no
//! allocation (the spec's NEVER list forbids `Box<dyn ExecutionBackend>` on the dispatch path).
//! Because the trait is sealed, adding methods to it later — when the `DurableContext`, promise,
//! and timer entry points land — is a non-breaking change.
//!
//! # Scope
//!
//! This module defines [`BackendCapabilities`], the sealed [`ExecutionBackend`] trait (with its
//! `capabilities` accessor), and the [`DurableBackendEnum`] dispatcher. The execution-open,
//! promise-resolution, and timer-scan methods named in the spec land alongside the
//! `DurableContext` (the trait can gain them without breaking callers).

use crate::config::RetentionPolicy;
use crate::error::DurableError;
use crate::ids::{ExecutionId, JournalSeq};
use crate::journal::{ExecutionStatus, Journal, JournalEntry};

pub mod local;

pub use local::LocalBackend;

/// The capabilities a backend advertises so callers can adapt their journaling strategy.
///
/// The replay cursor and the durable-step primitive read these flags to decide, for example,
/// whether parallel steps may journal concurrently or must be serialized into reserved-id order.
///
/// # Examples
///
/// ```
/// use zeph_durable::BackendCapabilities;
///
/// // The local backend journals parallel steps concurrently and stays in-process.
/// let caps = BackendCapabilities {
///     parallel_steps: true,
///     cross_process: false,
///     max_payload: 1_048_576,
/// };
/// assert!(caps.parallel_steps);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// Whether the backend may record parallel steps concurrently. `false` (e.g. Restate) requires
    /// the durable wrapper to serialize *recording* into reserved-`StepId` order.
    pub parallel_steps: bool,
    /// Whether the journal lives on a database shared across processes. Drives the INV-8 encryption
    /// gate and row-level HMAC requirement.
    pub cross_process: bool,
    /// The maximum payload size, in bytes, the backend accepts on append.
    pub max_payload: usize,
}

/// A durable-execution persistence backend.
///
/// `ExecutionBackend` is the closed set of journal engines Zeph ships. It is sealed via
/// [`crate::sealed::Sealed`]: external crates cannot implement it and must dispatch through
/// [`DurableBackendEnum`]. Every backend is also a [`Journal`], so the append/read/finalize/prune
/// surface is available uniformly.
///
/// # Contract for implementors
///
/// - [`capabilities`](ExecutionBackend::capabilities) MUST return a stable description of the
///   backend; callers cache it and adapt their journaling strategy to it.
/// - The [`Journal`] half MUST serialize writes through a single connection so appends receive a
///   monotonic [`JournalSeq`].
///
/// Additional entry points (execution open, promise resolution, timer scan) are added as the
/// higher layers land; because the trait is sealed, those additions do not break callers.
pub trait ExecutionBackend: Journal + Send + Sync + crate::sealed::Sealed {
    /// Return this backend's stable capability description.
    fn capabilities(&self) -> BackendCapabilities;
}

/// Closed enum dispatch over the compiled-in backends.
///
/// Construct it from a concrete backend and hand it across the crate boundary behind an `Arc`;
/// callers invoke the [`Journal`] and [`ExecutionBackend`] methods on the enum and the dispatch
/// resolves to the active variant with a single `match`. The enum is `#[non_exhaustive]`: the
/// feature-gated `Restate` variant joins it with the `restate` feature without breaking in-crate
/// matches.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zeph_durable::{BackendCapabilities, DurableBackendEnum};
///
/// fn max_payload(backend: &DurableBackendEnum) -> usize {
///     use zeph_durable::ExecutionBackend as _;
///     backend.capabilities().max_payload
/// }
/// # let _ = max_payload;
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum DurableBackendEnum {
    /// The always-compiled local backend journaling to a dedicated `durable.db`.
    Local(LocalBackend),
}

impl crate::sealed::Sealed for DurableBackendEnum {}

impl Journal for DurableBackendEnum {
    async fn append(&self, entry: JournalEntry) -> Result<JournalSeq, DurableError> {
        match self {
            Self::Local(backend) => backend.append(entry).await,
        }
    }

    async fn read_execution(&self, id: ExecutionId) -> Result<Vec<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.read_execution(id).await,
        }
    }

    async fn read_execution_range(
        &self,
        id: ExecutionId,
        from_step_id: u32,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.read_execution_range(id, from_step_id, limit).await,
        }
    }

    async fn finalize(&self, id: ExecutionId, status: ExecutionStatus) -> Result<(), DurableError> {
        match self {
            Self::Local(backend) => backend.finalize(id, status).await,
        }
    }

    async fn prune(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        match self {
            Self::Local(backend) => backend.prune(policy).await,
        }
    }
}

impl ExecutionBackend for DurableBackendEnum {
    fn capabilities(&self) -> BackendCapabilities {
        match self {
            Self::Local(backend) => backend.capabilities(),
        }
    }
}
