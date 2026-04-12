// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Policy LLM client trait and minimal message types.
//!
//! Defines the interface used by `zeph-tools` adversarial policy validation,
//! moved here to keep `zeph-tools` decoupled from `zeph-llm`.

use std::future::Future;
use std::pin::Pin;

/// Minimal message type for policy LLM calls.
///
/// Uses a dedicated type rather than importing `zeph-llm::Message` to keep
/// `zeph-common` free of `zeph-*` dependencies.
#[derive(Debug, Clone)]
pub struct PolicyMessage {
    /// Role of the message sender.
    pub role: PolicyRole,
    /// Message content.
    pub content: String,
}

/// Role for a [`PolicyMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRole {
    /// System-level instruction.
    System,
    /// User-level prompt.
    User,
}

/// Trait for sending chat messages to the policy LLM.
///
/// Implemented externally (e.g. in `runner.rs` on a newtype wrapping `Arc<AnyProvider>`).
/// `zeph-tools` defines the usage; `zeph-common` defines the contract, keeping both
/// crates decoupled from `zeph-llm`.
pub trait PolicyLlmClient: Send + Sync {
    /// Send a sequence of messages and return the assistant's text response.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if the LLM call fails (network error, timeout, etc.).
    fn chat<'a>(
        &'a self,
        messages: &'a [PolicyMessage],
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}
