// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cocoon confidential compute integration: sidecar HTTP client and LLM provider.
//!
//! [`CocoonClient`] communicates with the Cocoon C++ sidecar on `localhost`.
//! [`CocoonProvider`] implements [`crate::provider::LlmProvider`] by delegating body construction
//! to an inner [`crate::openai::OpenAiProvider`] and routing requests through [`CocoonClient`].
//!
//! All RA-TLS attestation, proxy selection, and TON payments are handled
//! transparently by the sidecar. Zeph never connects to the proxy or TEE workers
//! directly.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use zeph_llm::cocoon::{CocoonClient, CocoonProvider};
//!
//! let client = Arc::new(CocoonClient::new(
//!     "http://localhost:10000",
//!     None,
//!     Duration::from_secs(30),
//! ));
//! let provider = CocoonProvider::new("Qwen/Qwen3-0.6B", 4096, None, client);
//! ```

mod client;
mod provider;
mod stt;
#[cfg(test)]
mod tests;

pub use client::{CocoonClient, CocoonHealth};
pub use provider::CocoonProvider;
pub use stt::CocoonSttProvider;
