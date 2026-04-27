// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealed trait token for [`crate::channel::AgentChannel`].
//!
//! `Sealed` must be `pub` so `zeph-core` can implement it on `AgentChannelView`, but it is
//! placed in this module and re-exported with `#[doc(hidden)]` to discourage external use.
//! Downstream crates should not implement `AgentChannel` — the trait is intentionally sealed.

/// Sealing supertrait for [`crate::channel::AgentChannel`].
///
/// # Stability
///
/// This trait is `#[doc(hidden)]`. External crates MUST NOT implement it.
/// Adding methods to `AgentChannel` is a non-breaking change precisely because no external
/// impls exist.
#[doc(hidden)]
pub trait Sealed {}
