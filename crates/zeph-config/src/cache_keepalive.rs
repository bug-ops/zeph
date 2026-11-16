// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the prompt-cache keepalive service (`[llm.cache_keepalive]`).

use serde::{Deserialize, Serialize};
use zeph_common::ProviderName;

/// Error returned when a keepalive ping cap is outside `1..=50`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("keepalive ping cap {0} is outside 1..=50")]
pub struct KeepalivePingCapError(u32);

/// Number of consecutive idle pings the keepalive service may send, in `1..=50`.
///
/// Zero would disable the service silently and an unbounded value removes the only spend limit
/// when nothing can be priced, so both are rejected at construction and at deserialization.
///
/// # Examples
///
/// ```
/// use zeph_config::KeepalivePingCap;
///
/// assert_eq!(KeepalivePingCap::new(6).unwrap().get(), 6);
/// assert!(KeepalivePingCap::new(0).is_err());
/// assert!(KeepalivePingCap::new(51).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct KeepalivePingCap(u32);

impl KeepalivePingCap {
    /// Smallest accepted cap.
    pub const MIN: u32 = 1;
    /// Largest accepted cap.
    pub const MAX: u32 = 50;

    /// Build a cap from a ping count.
    ///
    /// # Errors
    ///
    /// Returns [`KeepalivePingCapError`] when `pings` is outside `1..=50`.
    pub const fn new(pings: u32) -> Result<Self, KeepalivePingCapError> {
        if pings < Self::MIN || pings > Self::MAX {
            Err(KeepalivePingCapError(pings))
        } else {
            Ok(Self(pings))
        }
    }

    /// The cap as a ping count.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for KeepalivePingCap {
    type Error = KeepalivePingCapError;

    fn try_from(pings: u32) -> Result<Self, Self::Error> {
        Self::new(pings)
    }
}

impl From<KeepalivePingCap> for u32 {
    fn from(cap: KeepalivePingCap) -> Self {
        cap.0
    }
}

impl Default for KeepalivePingCap {
    fn default() -> Self {
        Self(6)
    }
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheKeepaliveConfig {
    /// Run the keepalive service. Default: `false`.
    pub enabled: bool,
    /// Provider to ping, by `[[llm.providers]]` name. Empty selects the main provider.
    pub keepalive_provider: ProviderName,
    /// Pings sent in a row without real activity before the service stops, `1..=50`. Default: `6`.
    pub max_consecutive_pings: KeepalivePingCap,
}
