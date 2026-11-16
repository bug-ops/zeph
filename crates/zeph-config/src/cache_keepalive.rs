// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the prompt-cache keepalive service (`[llm.cache_keepalive]`).

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use zeph_common::ProviderName;

fn default_max_consecutive_pings() -> NonZeroU32 {
    NonZeroU32::new(6).expect("6 is non-zero")
}

/// `[llm.cache_keepalive]`: periodically re-reads the cached prompt prefix while the session is
/// idle so the provider's prompt cache does not expire between turns.
///
/// The service is opt-in. `keepalive_provider` names the `[[llm.providers]]` entry to ping;
/// empty means the main provider.
///
/// # Examples
///
/// ```
/// use zeph_config::CacheKeepaliveConfig;
///
/// let cfg = CacheKeepaliveConfig::default();
/// assert!(!cfg.enabled);
/// assert_eq!(cfg.max_consecutive_pings.get(), 6);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheKeepaliveConfig {
    /// Run the keepalive service. Default: `false`.
    pub enabled: bool,
    /// Provider to ping, by `[[llm.providers]]` name. Empty selects the main provider.
    pub keepalive_provider: ProviderName,
    /// Pings sent in a row without real activity before the service stops. Default: `6`.
    pub max_consecutive_pings: NonZeroU32,
}

impl Default for CacheKeepaliveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keepalive_provider: ProviderName::default(),
            max_consecutive_pings: default_max_consecutive_pings(),
        }
    }
}
