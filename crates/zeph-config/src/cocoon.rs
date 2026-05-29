// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::default_true;

/// Cocoon-level display and behaviour settings.
///
/// These options govern how Cocoon-specific information is presented in the TUI.
/// They are independent of the Cocoon LLM provider entry in `[[llm.providers]]`.
///
/// # Examples
///
/// ```toml
/// [cocoon]
/// show_balance = false  # redact TON balance in the TUI status bar
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CocoonConfig {
    /// Show the Cocoon TON balance in the TUI status bar.
    ///
    /// When `false`, the balance is rendered as `*** TON` instead of the real
    /// value, implementing the redaction option described in spec §15.2.
    /// Default is `true` (balance visible), matching the current behaviour.
    #[serde(default = "default_true")]
    pub show_balance: bool,
}

impl Default for CocoonConfig {
    fn default() -> Self {
        Self { show_balance: true }
    }
}
