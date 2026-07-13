// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent discovery via `/.well-known/agent.json` with TTL-based caching, plus optional
//! card-signature and URL-origin trust checks (A2A 1.0.0 §8.4, #5928).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::card_signing::{SignatureVerification, TrustedKey, verify_card_signatures};
use crate::error::A2aError;
use crate::types::AgentCard;

// TODO(critic): 1.0.0 serves /.well-known/agent-card.json — needs version-aware
// fetch/fallback before pure-1.0.0 peers are discoverable (#5928 follow-up).
const WELL_KNOWN_PATH: &str = "/.well-known/agent.json";

/// Trust policy for peer [`AgentCard`] verification during [`AgentRegistry::discover`]
/// (A2A 1.0.0 §8.4).
///
/// Mirrors `zeph_config::channels::CardTrustPolicy` (TOML-facing) as an independent
/// type, the same way `zeph_mcp::ToolDiscoveryStrategy` mirrors its `zeph-config`
/// counterpart: `zeph-config` must not depend on protocol crates, so config-side and
/// protocol-side enums are converted where both crates are in scope — the top-level
/// `zeph` binary crate (`src/tui_remote.rs::convert_card_trust_policy`), which wires
/// `[a2a_client].card_trust_policy`/`trusted_agent_keys` into the `AgentRegistry::discover`
/// call performed before `zeph --connect <URL>` establishes an A2A session (#6200).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CardTrustPolicy {
    /// Discover peer cards without checking signatures or URL origin. Default —
    /// byte-identical to pre-#5928 behavior.
    #[default]
    Ignore,
    /// Log a warning on an untrusted/unverifiable card or URL-origin mismatch, but still
    /// accept it. Reject only an actively **tampered** signature (a trusted key's
    /// signature that fails cryptographic verification).
    ///
    /// Recommended production setting once the S1 real-vector interop gate (see
    /// [`crate::card_signing`] module docs) has landed.
    Prefer,
    /// Reject any card with an unverifiable signature or a URL-origin mismatch.
    Require,
}

/// Severity of a trust-check outcome, used to combine the URL-origin and signature axes
/// (S2: evaluate both, take the most severe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Accept,
    Warn,
    Reject,
}

fn origin_severity(policy: CardTrustPolicy, mismatch: bool) -> Severity {
    if !mismatch {
        return Severity::Accept;
    }
    match policy {
        CardTrustPolicy::Ignore => Severity::Accept,
        CardTrustPolicy::Prefer => Severity::Warn,
        CardTrustPolicy::Require => Severity::Reject,
    }
}

/// Combine policy with a [`SignatureVerification`] outcome.
///
/// `Invalid` always rejects under `prefer`/`require` regardless of the URL-origin axis
/// (S2: a tampered signature is a stronger signal than an origin mismatch). `Unverifiable`
/// and `FeatureDisabled` are treated identically: under `require` they reject (an operator
/// who sets `require` without the `card-signing` feature compiled in gets a loud failure,
/// never a silent downgrade — see S3), under `prefer` they warn-and-accept.
fn signature_severity(policy: CardTrustPolicy, verification: &SignatureVerification) -> Severity {
    if policy == CardTrustPolicy::Ignore {
        return Severity::Accept;
    }
    match verification {
        SignatureVerification::Verified => Severity::Accept,
        SignatureVerification::Invalid { .. } => Severity::Reject,
        SignatureVerification::Unverifiable { .. } | SignatureVerification::FeatureDisabled => {
            match policy {
                CardTrustPolicy::Prefer => Severity::Warn,
                CardTrustPolicy::Require => Severity::Reject,
                CardTrustPolicy::Ignore => Severity::Accept,
            }
        }
    }
}

fn signature_reason(verification: &SignatureVerification) -> String {
    match verification {
        SignatureVerification::Verified => String::new(),
        SignatureVerification::Unverifiable { reason }
        | SignatureVerification::Invalid { reason } => reason.clone(),
        SignatureVerification::FeatureDisabled => "card-signing feature not compiled in".to_owned(),
    }
}

/// Origin (scheme + host + port) comparison result between the queried `base_url` and the
/// card's self-declared `url` field.
enum OriginCheck {
    Match,
    Mismatch { queried: String, advertised: String },
}

/// Compare `base_url` (what was queried) against `card_url` (what the card claims) by
/// scheme + host (case-insensitive) + `port_or_known_default()`, per RFC 6454 origin
/// semantics. A parse failure of `card_url` counts as a mismatch.
fn check_origin(base_url: &str, card_url: &str) -> OriginCheck {
    let queried = url::Url::parse(base_url);
    let advertised = url::Url::parse(card_url);
    let (Ok(queried), Ok(advertised)) = (queried, advertised) else {
        return OriginCheck::Mismatch {
            queried: base_url.to_owned(),
            advertised: card_url.to_owned(),
        };
    };
    let same_origin = queried.scheme().eq_ignore_ascii_case(advertised.scheme())
        && queried
            .host_str()
            .zip(advertised.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && queried.port_or_known_default() == advertised.port_or_known_default();
    if same_origin {
        OriginCheck::Match
    } else {
        OriginCheck::Mismatch {
            queried: format!(
                "{}://{}:{}",
                queried.scheme(),
                queried.host_str().unwrap_or(""),
                queried.port_or_known_default().unwrap_or(0)
            ),
            advertised: format!(
                "{}://{}:{}",
                advertised.scheme(),
                advertised.host_str().unwrap_or(""),
                advertised.port_or_known_default().unwrap_or(0)
            ),
        }
    }
}

#[derive(Default)]
struct TrustConfig {
    policy: CardTrustPolicy,
    trusted_keys: Vec<TrustedKey>,
}

struct CachedCard {
    card: AgentCard,
    fetched_at: Instant,
}

/// In-memory registry of peer agent capability cards with TTL-based cache invalidation.
///
/// `AgentRegistry` fetches [`AgentCard`] documents from `{base_url}/.well-known/agent.json`
/// and caches them for up to `ttl`. It supports three usage patterns:
///
/// 1. **Auto-discovery** via [`discover`](Self::discover): always fetches from the network.
/// 2. **Cache-first** via [`get_or_discover`](Self::get_or_discover): returns the cached card
///    if it is younger than `ttl`, otherwise re-fetches.
/// 3. **Manual registration** via [`register`](Self::register): populates the cache directly
///    without a network call (useful for known peers or test fixtures).
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_a2a::AgentRegistry;
/// use std::time::Duration;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
///
/// // Discover and cache a peer agent.
/// let card = registry.discover("http://peer.example.com").await?;
/// println!("Peer agent: {}", card.name);
///
/// // Next call returns the cached version (no network request).
/// let card = registry.get_or_discover("http://peer.example.com").await?;
/// # Ok(())
/// # }
/// ```
pub struct AgentRegistry {
    client: reqwest::Client,
    cache: RwLock<HashMap<String, CachedCard>>,
    ttl: Duration,
    /// Timeout for each network request in [`discover`](Self::discover).
    request_timeout: Duration,
    /// Card-signing + URL-origin trust policy applied in [`discover`](Self::discover).
    /// Defaults to [`CardTrustPolicy::Ignore`] with an empty key store when
    /// [`with_trust`](Self::with_trust) is never called — byte-identical to pre-#5928
    /// behavior for existing callers.
    trust: TrustConfig,
}

impl AgentRegistry {
    /// Create a new registry with the given HTTP client and cache TTL.
    ///
    /// All discovered or registered cards are evicted from the cache after `ttl` elapses.
    #[must_use]
    pub fn new(client: reqwest::Client, ttl: Duration) -> Self {
        Self {
            client,
            cache: RwLock::new(HashMap::new()),
            ttl,
            request_timeout: Duration::from_secs(10),
            trust: TrustConfig::default(),
        }
    }

    /// Set the per-request network timeout for [`discover`](Self::discover) calls (default: 10 seconds).
    ///
    /// Discovery is a lightweight GET request for the agent card; 10 seconds is generous.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Configure the card-signing + URL-origin trust policy applied by
    /// [`discover`](Self::discover).
    ///
    /// Not calling this method leaves the registry at [`CardTrustPolicy::Ignore`] with no
    /// trusted keys — existing callers see zero behavior change.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::{AgentRegistry, CardTrustPolicy};
    /// use std::time::Duration;
    ///
    /// let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5))
    ///     .with_trust(CardTrustPolicy::Prefer, vec![]);
    /// ```
    #[must_use]
    pub fn with_trust(mut self, policy: CardTrustPolicy, trusted_keys: Vec<TrustedKey>) -> Self {
        // Breadcrumb for operators enabling enforcement (S3, restored after #6200/#6201
        // review): card-signature interop with a real A2A peer is still unvalidated — the
        // JCS canonicalization (including #6201's proto3-default stripping) is implemented
        // per the A2A spec text only, never checked against a real `a2a-sdk`-produced
        // signed card (see `crate::card_signing` module docs). This condition doesn't
        // change based on which gap is currently open, so it's logged here rather than
        // only in a doc comment an operator flipping the knob at runtime will never read.
        if policy != CardTrustPolicy::Ignore {
            tracing::warn!(
                policy = ?policy,
                "a2a discovery: card signature interop is unvalidated against a real A2A peer \
                 (#5928/#6201) — canonicalization is implemented per spec text only; `require` \
                 may reject genuinely valid signed peers"
            );
        }
        self.trust = TrustConfig {
            policy,
            trusted_keys,
        };
        self
    }

    /// Fetch the [`AgentCard`] from `{base_url}/.well-known/agent.json` and update the cache.
    ///
    /// Always performs a network request regardless of the current cache state. The result
    /// is stored under `base_url` so subsequent [`get_or_discover`](Self::get_or_discover)
    /// calls can serve it without re-fetching until the TTL expires.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError`] wrapping an HTTP transport failure, or a [`A2aError`] discovery
    /// variant on non-2xx HTTP status or JSON parse failure.
    #[tracing::instrument(name = "a2a.discovery.discover", skip_all, err)]
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        let url = format!("{}{WELL_KNOWN_PATH}", base_url.trim_end_matches('/'));
        let (card, raw_value): (AgentCard, serde_json::Value) =
            tokio::time::timeout(self.request_timeout, async {
                let resp = self.client.get(&url).send().await?;

                if !resp.status().is_success() {
                    return Err(A2aError::Discovery {
                        url: url.clone(),
                        reason: format!("HTTP {}", resp.status()),
                    });
                }

                let bytes = resp.bytes().await.map_err(|e| A2aError::Discovery {
                    url: url.clone(),
                    reason: e.to_string(),
                })?;
                // Keep the raw JSON `Value` around (not just the typed `AgentCard`):
                // signature verification must canonicalize the bytes as received, never a
                // re-serialization of the typed struct — see `card_signing` module docs (S1).
                let raw_value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| A2aError::Discovery {
                        url: url.clone(),
                        reason: e.to_string(),
                    })?;
                let card: AgentCard =
                    serde_json::from_value(raw_value.clone()).map_err(|e| A2aError::Discovery {
                        url: url.clone(),
                        reason: e.to_string(),
                    })?;
                Ok((card, raw_value))
            })
            .await
            .map_err(|_| A2aError::Timeout(self.request_timeout))??;

        self.check_trust(base_url, &card, &raw_value)?;

        let mut cache = self.cache.write().await;
        cache.insert(
            base_url.to_owned(),
            CachedCard {
                card: card.clone(),
                fetched_at: Instant::now(),
            },
        );

        Ok(card)
    }

    /// Apply the URL-origin and signature trust checks (S2: combine both axes, most
    /// severe wins) and either accept (silently or with a `tracing::warn!`) or reject the
    /// discovered card.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError::UrlMismatch`] when only the URL-origin axis rejects, or
    /// [`A2aError::UntrustedCard`] when the signature axis (alone or combined with the
    /// URL axis) rejects.
    fn check_trust(
        &self,
        base_url: &str,
        card: &AgentCard,
        raw_value: &serde_json::Value,
    ) -> Result<(), A2aError> {
        let origin = check_origin(base_url, &card.url);
        let origin_mismatch = matches!(origin, OriginCheck::Mismatch { .. });
        let sig_verification =
            verify_card_signatures(raw_value, &card.signatures, &self.trust.trusted_keys);

        let policy = self.trust.policy;
        let o_sev = origin_severity(policy, origin_mismatch);
        let s_sev = signature_severity(policy, &sig_verification);

        match o_sev.max(s_sev) {
            Severity::Accept => Ok(()),
            Severity::Warn => {
                if o_sev == Severity::Warn
                    && let OriginCheck::Mismatch {
                        queried,
                        advertised,
                    } = &origin
                {
                    tracing::warn!(
                        queried,
                        advertised,
                        "a2a discovery: card.url origin mismatch (prefer policy, accepting)"
                    );
                }
                if s_sev == Severity::Warn {
                    tracing::warn!(
                        reason = %signature_reason(&sig_verification),
                        "a2a discovery: card signature unverifiable (prefer policy, accepting)"
                    );
                }
                Ok(())
            }
            Severity::Reject => {
                if o_sev == Severity::Reject
                    && s_sev != Severity::Reject
                    && let OriginCheck::Mismatch {
                        queried,
                        advertised,
                    } = origin
                {
                    return Err(A2aError::UrlMismatch {
                        queried,
                        advertised,
                    });
                }
                let mut reason = signature_reason(&sig_verification);
                if o_sev == Severity::Reject
                    && let OriginCheck::Mismatch {
                        queried,
                        advertised,
                    } = &origin
                {
                    reason = format!(
                        "{reason}; additionally url origin mismatch (queried '{queried}', advertised '{advertised}')"
                    );
                }
                Err(A2aError::UntrustedCard { reason })
            }
        }
    }

    /// Return a cached [`AgentCard`] if it is still within the TTL, otherwise re-fetch.
    ///
    /// This is the preferred call for high-frequency routing decisions — it avoids a
    /// network round-trip on every call while still refreshing stale cards automatically.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError`] if the cached entry is expired and the re-fetch via
    /// [`discover`](Self::discover) fails.
    #[tracing::instrument(name = "a2a.discovery.get_or_discover", skip_all, err)]
    pub async fn get_or_discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(base_url)
                && entry.fetched_at.elapsed() < self.ttl
            {
                return Ok(entry.card.clone());
            }
        }
        self.discover(base_url).await
    }

    /// Manually register an [`AgentCard`] under `base_url`, bypassing the network.
    ///
    /// Overwrites any existing entry for the same URL. The card is treated as freshly
    /// fetched and will not expire until `ttl` has elapsed from the time of this call.
    ///
    /// Useful when the card is already known (e.g., loaded from config) or in tests.
    ///
    /// Bypasses `card_trust_policy` entirely — no URL-origin or signature check runs for a
    /// manually registered card, regardless of policy. The caller vouches for the card.
    #[tracing::instrument(name = "a2a.discovery.register", skip_all)]
    pub async fn register(&self, base_url: String, card: AgentCard) {
        let mut cache = self.cache.write().await;
        cache.insert(
            base_url,
            CachedCard {
                card,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Return all currently cached [`AgentCard`]s, including stale entries.
    ///
    /// This does not trigger any eviction or re-fetch. Call [`evict_stale`](Self::evict_stale)
    /// first if you only want cards that are still within their TTL.
    #[tracing::instrument(name = "a2a.discovery.all", skip_all)]
    pub async fn all(&self) -> Vec<AgentCard> {
        let cache = self.cache.read().await;
        cache.values().map(|e| e.card.clone()).collect()
    }

    /// Remove all cache entries whose TTL has expired.
    ///
    /// Intended for periodic background cleanup. The A2A server does not call this
    /// automatically — callers should schedule it as needed (e.g., via a periodic task).
    #[tracing::instrument(name = "a2a.discovery.evict_stale", skip_all)]
    pub async fn evict_stale(&self) {
        let mut cache = self.cache.write().await;
        cache.retain(|_, entry| entry.fetched_at.elapsed() < self.ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::AgentCardBuilder;

    fn test_card(name: &str) -> AgentCard {
        AgentCardBuilder::new(name, "http://localhost", "0.1.0")
            .description("test")
            .build()
    }

    #[tokio::test]
    async fn register_and_retrieve() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        let card = test_card("agent-1");
        registry
            .register("http://localhost:8080".into(), card.clone())
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "agent-1");
    }

    #[tokio::test]
    async fn get_or_discover_returns_cached() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        let card = test_card("cached");
        registry.register("http://example.com".into(), card).await;

        let result = registry.get_or_discover("http://example.com").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "cached");
    }

    #[tokio::test]
    async fn evict_stale_removes_expired() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        let card = test_card("stale");
        registry
            .register("http://stale.example.com".into(), card)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;
        registry.evict_stale().await;

        let all = registry.all().await;
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn get_or_discover_refetches_after_ttl_expiry() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        let card = test_card("expiring");
        registry
            .register("http://no-server.invalid".into(), card)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = registry.get_or_discover("http://no-server.invalid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn discover_invalid_url_returns_error() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover("http://no-server.invalid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_registrations() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        registry
            .register("http://a.example.com".into(), test_card("a"))
            .await;
        registry
            .register("http://b.example.com".into(), test_card("b"))
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn register_overwrites_existing() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        registry
            .register("http://a.example.com".into(), test_card("v1"))
            .await;
        registry
            .register("http://a.example.com".into(), test_card("v2"))
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "v2");
    }

    #[test]
    fn check_origin_matches_same_origin_different_path() {
        let result = check_origin("http://example.com:8080", "http://example.com:8080/a2a");
        assert!(matches!(result, OriginCheck::Match));
    }

    #[test]
    fn check_origin_case_insensitive_host() {
        let result = check_origin("https://Example.COM", "https://example.com");
        assert!(matches!(result, OriginCheck::Match));
    }

    #[test]
    fn check_origin_default_port_equivalence() {
        let result = check_origin("https://example.com", "https://example.com:443");
        assert!(matches!(result, OriginCheck::Match));
    }

    #[test]
    fn check_origin_detects_scheme_mismatch() {
        let result = check_origin("http://example.com", "https://example.com");
        assert!(matches!(result, OriginCheck::Mismatch { .. }));
    }

    #[test]
    fn check_origin_detects_host_mismatch() {
        let result = check_origin("http://example.com", "http://evil.example.com");
        assert!(matches!(result, OriginCheck::Mismatch { .. }));
    }

    #[test]
    fn check_origin_detects_port_mismatch() {
        let result = check_origin("http://example.com:8080", "http://example.com:9090");
        assert!(matches!(result, OriginCheck::Mismatch { .. }));
    }

    #[test]
    fn check_origin_unparsable_card_url_is_mismatch() {
        let result = check_origin("http://example.com", "not a url");
        assert!(matches!(result, OriginCheck::Mismatch { .. }));
    }

    #[test]
    fn origin_severity_table() {
        assert_eq!(
            origin_severity(CardTrustPolicy::Ignore, true),
            Severity::Accept
        );
        assert_eq!(
            origin_severity(CardTrustPolicy::Ignore, false),
            Severity::Accept
        );
        assert_eq!(
            origin_severity(CardTrustPolicy::Prefer, true),
            Severity::Warn
        );
        assert_eq!(
            origin_severity(CardTrustPolicy::Prefer, false),
            Severity::Accept
        );
        assert_eq!(
            origin_severity(CardTrustPolicy::Require, true),
            Severity::Reject
        );
        assert_eq!(
            origin_severity(CardTrustPolicy::Require, false),
            Severity::Accept
        );
    }

    #[test]
    fn signature_severity_table() {
        let verified = SignatureVerification::Verified;
        let invalid = SignatureVerification::Invalid {
            reason: "bad".into(),
        };
        let unverifiable = SignatureVerification::Unverifiable {
            reason: "unsigned".into(),
        };
        let disabled = SignatureVerification::FeatureDisabled;

        for policy in [
            CardTrustPolicy::Ignore,
            CardTrustPolicy::Prefer,
            CardTrustPolicy::Require,
        ] {
            assert_eq!(
                signature_severity(policy, &verified),
                Severity::Accept,
                "Verified must always accept under {policy:?}"
            );
        }

        assert_eq!(
            signature_severity(CardTrustPolicy::Ignore, &invalid),
            Severity::Accept
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Prefer, &invalid),
            Severity::Reject,
            "Invalid must reject under prefer even though other Unverifiable cases only warn"
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Require, &invalid),
            Severity::Reject
        );

        assert_eq!(
            signature_severity(CardTrustPolicy::Ignore, &unverifiable),
            Severity::Accept
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Prefer, &unverifiable),
            Severity::Warn
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Require, &unverifiable),
            Severity::Reject
        );

        // FeatureDisabled is treated identically to Unverifiable (S2/S3): require rejects
        // loudly rather than silently downgrading.
        assert_eq!(
            signature_severity(CardTrustPolicy::Ignore, &disabled),
            Severity::Accept
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Prefer, &disabled),
            Severity::Warn
        );
        assert_eq!(
            signature_severity(CardTrustPolicy::Require, &disabled),
            Severity::Reject
        );
    }

    #[test]
    fn check_trust_default_ignore_accepts_everything() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let mut card = test_card("peer");
        card.url = "http://totally-different.example.com".into();
        let raw = serde_json::to_value(&card).unwrap();
        assert!(
            registry
                .check_trust("http://localhost", &card, &raw)
                .is_ok()
        );
    }

    #[test]
    fn check_trust_require_rejects_url_mismatch_and_unsigned_card_combined() {
        // An unsigned card fails the signature axis too under `require`, so both axes
        // reject and the combined `UntrustedCard` (not `UrlMismatch`) error is returned —
        // see `check_trust_require_rejects_url_mismatch_alone_when_signature_verifies`
        // below for the URL-axis-only rejection case.
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(CardTrustPolicy::Require, vec![]);
        let mut card = test_card("peer");
        card.url = "http://totally-different.example.com".into();
        let raw = serde_json::to_value(&card).unwrap();
        let err = registry
            .check_trust("http://localhost", &card, &raw)
            .unwrap_err();
        let A2aError::UntrustedCard { reason } = err else {
            panic!("expected UntrustedCard, got {err:?}");
        };
        assert!(reason.contains("url origin mismatch"), "reason: {reason}");
    }

    #[cfg(feature = "card-signing")]
    #[test]
    fn check_trust_require_rejects_url_mismatch_alone_when_signature_verifies() {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::EncodePublicKey;

        let signing_key = SigningKey::from_bytes(&[3u8; 32].into()).unwrap();
        let pem = signing_key
            .verifying_key()
            .to_public_key_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();
        let trusted = vec![TrustedKey {
            kid: "key-1".into(),
            alg: crate::card_signing::SigAlg::Es256,
            key_material: pem,
        }];

        let mut card = test_card("peer");
        card.url = "http://totally-different.example.com".into();
        let raw_unsigned = serde_json::to_value(&card).unwrap();
        let sig = crate::card_signing::sign_card(&raw_unsigned, "key-1", &signing_key).unwrap();
        card.signatures = vec![sig.clone()];
        let mut raw = raw_unsigned;
        raw["signatures"] = serde_json::json!([sig]);

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(CardTrustPolicy::Require, trusted);
        let err = registry
            .check_trust("http://localhost", &card, &raw)
            .unwrap_err();
        assert!(matches!(err, A2aError::UrlMismatch { .. }));
    }

    #[test]
    fn check_trust_prefer_warns_but_accepts_unsigned_peer() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(CardTrustPolicy::Prefer, vec![]);
        let card = test_card("peer"); // url == base_url passed below; card is unsigned
        let raw = serde_json::to_value(&card).unwrap();
        assert!(
            registry
                .check_trust("http://localhost", &card, &raw)
                .is_ok()
        );
    }

    #[test]
    fn check_trust_require_rejects_unsigned_peer() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(CardTrustPolicy::Require, vec![]);
        let card = test_card("peer");
        let raw = serde_json::to_value(&card).unwrap();
        let err = registry
            .check_trust("http://localhost", &card, &raw)
            .unwrap_err();
        assert!(matches!(err, A2aError::UntrustedCard { .. }));
    }
}

#[cfg(test)]
mod wiremock_tests {
    use std::assert_matches;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::discovery::AgentRegistry;
    use crate::error::A2aError;
    use crate::testing::agent_card_response;

    #[tokio::test]
    async fn discover_success() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("mock-agent", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let card = registry.discover(&base_url).await.unwrap();
        assert_eq!(card.name, "mock-agent");
    }

    #[tokio::test]
    async fn discover_404_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover(&server.uri()).await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Discovery { .. });
    }

    #[tokio::test]
    async fn discover_invalid_json_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover(&server.uri()).await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Discovery { .. });
    }

    #[tokio::test]
    async fn get_or_discover_refetches_after_ttl() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("fresh-agent", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        // Register stale card
        let stale = crate::card::AgentCardBuilder::new("stale", &base_url, "0.0.1").build();
        registry.register(base_url.clone(), stale).await;
        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Should re-fetch from mock server
        let card = registry.get_or_discover(&base_url).await.unwrap();
        assert_eq!(card.name, "fresh-agent");
    }

    #[tokio::test]
    async fn discover_times_out() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(serde_json::json!({
                        "name": "slow-agent",
                        "description": "test",
                        "url": base_url,
                        "version": "0.1.0",
                        "protocolVersion": crate::A2A_PROTOCOL_VERSION,
                        "capabilities": {"streaming": false, "pushNotifications": false, "stateTransitionHistory": false},
                        "defaultInputModes": [],
                        "defaultOutputModes": [],
                        "skills": []
                    })),
            )
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_request_timeout(Duration::from_millis(100));
        let result = registry.discover(&server.uri()).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), A2aError::Timeout(_)),
            "expected Timeout error from slow discovery"
        );
    }

    #[tokio::test]
    async fn discover_unsigned_peer_accepted_under_default_ignore() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("unsigned-peer", &base_url))
            .mount(&server)
            .await;

        // Default policy (no `with_trust` call) — must behave exactly as before #5928.
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let card = registry.discover(&base_url).await.unwrap();
        assert_eq!(card.name, "unsigned-peer");
    }

    #[tokio::test]
    async fn discover_unsigned_peer_accepted_under_prefer() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("unsigned-peer", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(crate::discovery::CardTrustPolicy::Prefer, vec![]);
        let card = registry.discover(&base_url).await.unwrap();
        assert_eq!(card.name, "unsigned-peer");
    }

    #[tokio::test]
    async fn discover_unsigned_peer_rejected_under_require() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("unsigned-peer", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(crate::discovery::CardTrustPolicy::Require, vec![]);
        let result = registry.discover(&base_url).await;
        assert_matches!(result.unwrap_err(), A2aError::UntrustedCard { .. });
    }

    #[tokio::test]
    async fn discover_url_mismatch_rejected_under_require() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            // Card advertises a different origin than the one queried.
            .respond_with(agent_card_response(
                "spoofed-peer",
                "http://attacker.example.com",
            ))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(crate::discovery::CardTrustPolicy::Require, vec![]);
        let result = registry.discover(&base_url).await;
        // The mock card is unsigned, so the signature axis also rejects under `require`
        // — the combined `UntrustedCard` error is returned (see
        // `check_trust_require_rejects_url_mismatch_alone_when_signature_verifies` in the
        // `tests` module above for the URL-axis-only case).
        assert_matches!(result.unwrap_err(), A2aError::UntrustedCard { .. });
    }

    #[tokio::test]
    async fn discover_url_mismatch_warns_but_accepts_under_prefer() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response(
                "spoofed-peer",
                "http://attacker.example.com",
            ))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1))
            .with_trust(crate::discovery::CardTrustPolicy::Prefer, vec![]);
        let card = registry.discover(&base_url).await.unwrap();
        assert_eq!(card.name, "spoofed-peer");
    }
}
