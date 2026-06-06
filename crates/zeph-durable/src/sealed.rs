// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealing token for the durable backend trait hierarchy.
//!
//! The execution-backend trait (landing in a later issue) is sealed so that only backends
//! declared inside `zeph-durable` — `LocalBackend` and the feature-gated `RestateBackend` — can
//! implement it. External crates select a backend through the `DurableBackendEnum` enum-dispatch
//! type, never by providing their own implementation. This keeps the backend surface closed and
//! makes adding backend methods a non-breaking change.

/// Sealing supertrait for the durable backend hierarchy.
///
/// # Stability
///
/// This trait is `#[doc(hidden)]`. External crates MUST NOT implement it. Because no external
/// implementations can exist, the sealed traits that depend on it may gain methods without a
/// breaking change.
#[doc(hidden)]
pub trait Sealed {}
