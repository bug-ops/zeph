// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Interaction settings carried from configuration into every agent build site.

use zeph_config::{AskUserConfig, CacheKeepaliveConfig, Config};

/// `[tools.ask_user]` and `[llm.cache_keepalive]` as one value.
///
/// Deliberately has no `Default`: every deps struct literal must set it, so a build site that
/// forgets to wire it fails to compile instead of silently running with the features off.
#[derive(Debug, Clone, PartialEq)]
pub struct InteractionConfig {
    /// The `ask_user` tool settings.
    pub ask_user: AskUserConfig,
    /// The prompt-cache keepalive settings.
    pub cache_keepalive: CacheKeepaliveConfig,
}

impl InteractionConfig {
    /// Copy the two sections out of `config`, field for field.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            ask_user: config.tools.ask_user.clone(),
            cache_keepalive: config.llm.cache_keepalive.clone(),
        }
    }
}
