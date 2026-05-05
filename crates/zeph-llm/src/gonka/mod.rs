// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Gonka AI gateway integration: endpoint pool and request signer.
//!
//! The [`RequestSigner`] authenticates HTTP requests using a secp256k1 key,
//! and the [`endpoints`] module manages a rotating pool of gateway nodes.

pub mod endpoints;
#[cfg(feature = "gonka")]
mod provider;
#[cfg(feature = "gonka")]
pub mod signer;
#[cfg(all(test, feature = "gonka"))]
mod tests;

#[cfg(feature = "gonka")]
pub use provider::GonkaProvider;
#[cfg(feature = "gonka")]
pub use signer::RequestSigner;
