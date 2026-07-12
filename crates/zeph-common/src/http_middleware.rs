// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP middleware for Zeph HTTP server crates.
//!
//! This module provides re-usable [`axum`] middleware for bearer-token authentication
//! and per-IP rate limiting.  Both `zeph-gateway` and `zeph-a2a` activate this module
//! via the `http-middleware` feature flag — no implementation is duplicated.
//!
//! # Feature gate
//!
//! Enable with `zeph-common = { …, features = ["http-middleware"] }`.
//!
//! # Authentication
//!
//! [`AuthConfig`] stores a pre-computed BLAKE3 hash of the expected bearer token.
//! Per-request comparison operates on two 32-byte digests using [`subtle::ConstantTimeEq`],
//! preventing timing-oracle attacks.  Set [`AuthConfig::require_auth`] to `true` to reject
//! requests even when no token is configured (useful for strict enforcement in A2A servers).
//!
//! # Rate limiting
//!
//! [`RateLimitState`] implements a per-IP fixed-window counter with optional CIDR-based
//! trusted-proxy support.  When the TCP peer falls within a trusted CIDR the middleware
//! applies the XFF rightmost-untrusted algorithm to identify the real client IP.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

/// Maximum number of IP entries retained in the rate-limit map before a GC pass.
///
/// When the map reaches this size and a new, unseen IP arrives, expired entries
/// (older than [`RATE_WINDOW`]) are evicted before inserting the new one.  This
/// bounds memory usage to roughly `MAX_RATE_LIMIT_ENTRIES × ~56 bytes`.
pub const MAX_RATE_LIMIT_ENTRIES: usize = 10_000;

/// Fixed window duration for the per-IP request counter.
pub const RATE_WINDOW: Duration = Duration::from_mins(1);

/// Pre-computed authentication configuration for the bearer-token middleware.
///
/// The expected token is hashed once at startup so that the per-request comparison
/// always operates on two 32-byte BLAKE3 digests, keeping comparison both O(1) and
/// constant-time.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::http_middleware::AuthConfig;
///
/// let cfg = AuthConfig::new(Some("my-secret-token"), false);
/// // token_hash is pre-computed; raw string never stored
/// ```
#[derive(Clone)]
pub struct AuthConfig {
    /// BLAKE3 hash of the configured bearer token, or `None` when no token is set.
    pub token_hash: Option<blake3::Hash>,
    /// When `true`, requests are rejected even if no token is configured.
    ///
    /// Useful for A2A servers that must enforce authentication at the config level.
    pub require_auth: bool,
}

impl AuthConfig {
    /// Construct an [`AuthConfig`], hashing `token` once at startup.
    ///
    /// If `token` is `None` and `require_auth` is `false`, all requests pass through.
    /// If `token` is `None` and `require_auth` is `true`, all requests are rejected with 401.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_common::http_middleware::AuthConfig;
    ///
    /// // Auth enabled with a token
    /// let cfg = AuthConfig::new(Some("secret"), false);
    ///
    /// // Require auth even without a configured token (rejects all requests)
    /// let cfg_strict = AuthConfig::new(None, true);
    /// ```
    #[must_use]
    pub fn new(token: Option<&str>, require_auth: bool) -> Self {
        Self {
            token_hash: token.map(|t| blake3::hash(t.as_bytes())),
            require_auth,
        }
    }
}

/// Identity extracted from a validated bearer token, inserted into request extensions.
///
/// Downstream handlers may read this extension to check whether the request was
/// authenticated without re-examining the `Authorization` header.
///
/// # Examples
///
/// ```no_run
/// use axum::extract::Extension;
/// use zeph_common::http_middleware::AuthIdentity;
///
/// async fn my_handler(Extension(id): Extension<AuthIdentity>) {
///     if id.authenticated {
///         // handle authenticated request
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct AuthIdentity {
    /// `true` when the request carried a valid bearer token.
    pub authenticated: bool,
}

/// A parsed CIDR block used for trusted-proxy matching.
///
/// Used by [`RateLimitState`] to decide whether to trust `X-Forwarded-For` headers.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::http_middleware::Cidr;
///
/// let cidr = Cidr::parse("10.0.0.0/8").unwrap();
/// assert!(cidr.contains("10.1.2.3".parse().unwrap()));
/// assert!(!cidr.contains("11.0.0.0".parse().unwrap()));
/// ```
#[derive(Clone, Debug)]
pub struct Cidr {
    addr: IpAddr,
    prefix_len: u8,
}

impl Cidr {
    /// Parse `"a.b.c.d/n"` or `"addr/n"`.  Returns `None` on any parse error.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_common::http_middleware::Cidr;
    ///
    /// assert!(Cidr::parse("192.168.0.0/16").is_some());
    /// assert!(Cidr::parse("not-a-cidr").is_none());
    /// assert!(Cidr::parse("10.0.0.0/33").is_none());
    /// ```
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (addr_str, prefix_str) = s.split_once('/')?;
        let addr: IpAddr = addr_str.parse().ok()?;
        let prefix_len: u8 = prefix_str.parse().ok()?;
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if prefix_len > max {
            return None;
        }
        Some(Self { addr, prefix_len })
    }

    /// Returns `true` when `ip` falls within this CIDR block.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_common::http_middleware::Cidr;
    ///
    /// let cidr = Cidr::parse("10.0.0.0/8").unwrap();
    /// assert!(cidr.contains("10.255.255.255".parse().unwrap()));
    /// assert!(!cidr.contains("11.0.0.0".parse().unwrap()));
    /// ```
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(candidate)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let shift = 32 - u32::from(self.prefix_len);
                u32::from(net) >> shift == u32::from(candidate) >> shift
            }
            (IpAddr::V6(net), IpAddr::V6(candidate)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                let shift = 128 - u32::from(self.prefix_len);
                u128::from(net) >> shift == u128::from(candidate) >> shift
            }
            _ => false,
        }
    }
}

/// Shared state threaded through the rate-limiting middleware.
///
/// Construct via [`RateLimitState::new`].  Pass to axum via
/// `middleware::from_fn_with_state(rate_state, rate_limit_middleware)`.
///
/// # Examples
///
/// ```no_run
/// use zeph_common::http_middleware::RateLimitState;
///
/// // 100 req/min, trust the loopback as a proxy
/// let state = RateLimitState::new(100, &["127.0.0.1/32".to_string()]);
/// ```
#[derive(Clone)]
pub struct RateLimitState {
    /// Maximum requests per IP per [`RATE_WINDOW`].  `0` disables rate limiting.
    pub limit: u32,
    /// Map from remote IP to `(request_count, window_start)`.
    pub counters: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
    /// Parsed CIDR blocks for trusted reverse proxies.
    pub trusted_cidrs: Arc<Vec<Cidr>>,
}

impl RateLimitState {
    /// Construct a [`RateLimitState`], parsing `trusted_proxy_cidrs` at startup.
    ///
    /// Invalid CIDR strings are logged as warnings and silently ignored.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_common::http_middleware::RateLimitState;
    ///
    /// let state = RateLimitState::new(60, &["10.0.0.0/8".to_string()]);
    /// ```
    #[must_use]
    pub fn new(limit: u32, trusted_proxy_cidrs: &[String]) -> Self {
        let parsed_cidrs: Vec<Cidr> = trusted_proxy_cidrs
            .iter()
            .filter_map(|s| {
                let c = Cidr::parse(s);
                if c.is_none() {
                    tracing::warn!(cidr = %s, "http-middleware: invalid trusted_proxy_cidr, ignoring");
                }
                c
            })
            .collect();
        Self {
            limit,
            counters: Arc::new(Mutex::new(HashMap::new())),
            trusted_cidrs: Arc::new(parsed_cidrs),
        }
    }
}

/// Axum middleware that enforces bearer-token authentication.
///
/// When [`AuthConfig::token_hash`] is `Some`, the request must carry
/// `Authorization: Bearer <token>` whose BLAKE3 hash matches the pre-computed digest.
/// Comparison uses [`ConstantTimeEq`] on two 32-byte arrays — constant time regardless
/// of token content, preventing timing-oracle attacks.
///
/// The middleware always inserts an [`AuthIdentity`] extension into the request so that
/// downstream handlers can inspect authentication status without re-reading the header.
///
/// - Token present and valid → passes through, `AuthIdentity { authenticated: true }`
/// - Token present but wrong → 401, `AuthIdentity { authenticated: false }`
/// - No token, `require_auth = true` → 401 with a warning log
/// - No token, `require_auth = false` → passes through, `AuthIdentity { authenticated: false }`
///
/// On failure this returns `401` directly, without calling `next.run` — so this middleware
/// must be layered *inside* [`rate_limit_middleware`] (i.e. `rate_limit_middleware` wraps
/// it). Otherwise failed-auth requests would never reach the rate limiter, letting an
/// attacker brute-force the bearer token with no per-IP throttling.
///
/// # Errors
///
/// Returns `401 Unauthorized` on authentication failure.
#[tracing::instrument(skip_all, name = "common.http_middleware.auth")]
pub async fn auth_middleware(
    axum::extract::State(cfg): axum::extract::State<AuthConfig>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(expected_hash) = cfg.token_hash {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());

        let token = auth_header
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");

        // Hash the submitted token to a fixed-length digest before comparing.
        // Expected hash is pre-computed at startup (stored in AuthConfig).
        // ct_eq operates on two 32-byte arrays — constant time regardless of content.
        let token_hash = blake3::hash(token.as_bytes());
        if !bool::from(token_hash.as_bytes().ct_eq(expected_hash.as_bytes())) {
            req.extensions_mut().insert(AuthIdentity {
                authenticated: false,
            });
            return StatusCode::UNAUTHORIZED.into_response();
        }
        req.extensions_mut().insert(AuthIdentity {
            authenticated: true,
        });
    } else {
        if cfg.require_auth {
            tracing::warn!(
                "http-middleware: require_auth=true but no auth_token configured, rejecting request"
            );
            req.extensions_mut().insert(AuthIdentity {
                authenticated: false,
            });
            return StatusCode::UNAUTHORIZED.into_response();
        }
        req.extensions_mut().insert(AuthIdentity {
            authenticated: false,
        });
    }

    next.run(req).await
}

/// Axum middleware that enforces a per-IP fixed-window rate limit.
///
/// Each remote IP is tracked independently.  A counter is incremented on every request
/// within the current window.  When the counter exceeds [`RateLimitState::limit`] the
/// request receives `429 Too Many Requests`.
///
/// When [`RateLimitState::trusted_cidrs`] is non-empty and the TCP peer falls within
/// a trusted CIDR, the middleware applies the XFF rightmost-untrusted algorithm on
/// `X-Forwarded-For` to identify the real client IP.
///
/// To prevent unbounded memory growth, expired entries are evicted via inline GC when
/// the counters map reaches [`MAX_RATE_LIMIT_ENTRIES`] and a new IP is encountered.
/// When the map is still at capacity after eviction (all entries fresh), the request
/// receives `429 Too Many Requests`.
///
/// Setting [`RateLimitState::limit`] to `0` disables rate limiting entirely.
///
/// This middleware must be layered *outside* [`auth_middleware`] (i.e. it wraps
/// `auth_middleware`, not the other way around), so that its counter is incremented for
/// every request — including ones that fail authentication — before `auth_middleware`
/// gets a chance to short-circuit with `401`. Layering auth outermost would let a
/// failed-auth request skip the counter entirely, allowing unthrottled bearer-token
/// brute-forcing.
///
/// # Errors
///
/// Returns `429 Too Many Requests` when the rate limit is exceeded or the rate-limit
/// table is full and cannot accommodate a new IP after eviction.
#[tracing::instrument(skip_all, name = "common.http_middleware.rate_limit")]
pub async fn rate_limit_middleware(
    axum::extract::State(state): axum::extract::State<RateLimitState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if state.limit == 0 {
        return next.run(req).await;
    }

    let peer_ip = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |ci| ci.0.ip());

    // When trusted proxy CIDRs are configured and the TCP peer falls within one of them,
    // apply the rightmost-untrusted algorithm on X-Forwarded-For: walk the header values
    // from right to left and pick the first IP that is not in any trusted CIDR.
    let ip = if !state.trusted_cidrs.is_empty()
        && state.trusted_cidrs.iter().any(|c| c.contains(peer_ip))
    {
        let xff_ip = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter_map(|s| s.parse::<IpAddr>().ok())
                    .rev()
                    .find(|ip| !state.trusted_cidrs.iter().any(|c| c.contains(*ip)))
            });
        xff_ip.unwrap_or(peer_ip)
    } else {
        peer_ip
    };

    let now = Instant::now();
    let mut counters = state.counters.lock().await;

    if counters.len() >= MAX_RATE_LIMIT_ENTRIES && !counters.contains_key(&ip) {
        let before_eviction = counters.len();
        counters.retain(|_, (_, ts)| now.duration_since(*ts) < RATE_WINDOW);
        let after_eviction = counters.len();

        if after_eviction >= MAX_RATE_LIMIT_ENTRIES {
            tracing::warn!(
                before = before_eviction,
                after = after_eviction,
                limit = MAX_RATE_LIMIT_ENTRIES,
                "rate limiter at capacity after stale entry eviction, rejecting new IP"
            );
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }

    let entry = counters.entry(ip).or_insert((0, now));
    if now.duration_since(entry.1) >= RATE_WINDOW {
        *entry = (1, now);
    } else {
        entry.0 += 1;
        if entry.0 > state.limit {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    drop(counters);

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_ipv4_contains_in_range() {
        let cidr = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(cidr.contains("10.1.2.3".parse().unwrap()));
        assert!(cidr.contains("10.255.255.255".parse().unwrap()));
        assert!(!cidr.contains("11.0.0.0".parse().unwrap()));
        assert!(!cidr.contains("9.255.255.255".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv4_slash32_exact_host() {
        let cidr = Cidr::parse("192.168.1.100/32").unwrap();
        assert!(cidr.contains("192.168.1.100".parse().unwrap()));
        assert!(!cidr.contains("192.168.1.101".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv4_slash0_matches_all() {
        let cidr = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(cidr.contains("1.2.3.4".parse().unwrap()));
        assert!(cidr.contains("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv6_contains_in_range() {
        let cidr = Cidr::parse("::1/128").unwrap();
        assert!(cidr.contains("::1".parse().unwrap()));
        assert!(!cidr.contains("::2".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv4_v6_mismatch_returns_false() {
        let cidr = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(!cidr.contains("::1".parse().unwrap()));
    }

    #[test]
    fn cidr_parse_rejects_invalid() {
        assert!(Cidr::parse("not-a-cidr").is_none());
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("::1/129").is_none());
        assert!(Cidr::parse("10.0.0.0/").is_none());
    }

    #[test]
    fn auth_config_new_hashes_token() {
        let cfg = AuthConfig::new(Some("secret"), false);
        assert!(cfg.token_hash.is_some());
        assert!(!cfg.require_auth);
        let expected = blake3::hash(b"secret");
        assert_eq!(cfg.token_hash.unwrap(), expected);
    }

    #[test]
    fn auth_config_new_none_token() {
        let cfg = AuthConfig::new(None, true);
        assert!(cfg.token_hash.is_none());
        assert!(cfg.require_auth);
    }

    #[test]
    fn rate_limit_state_new_parses_cidrs() {
        let state = RateLimitState::new(10, &["10.0.0.0/8".to_string(), "invalid".to_string()]);
        assert_eq!(state.limit, 10);
        assert_eq!(state.trusted_cidrs.len(), 1);
    }

    #[test]
    fn bearer_ct_eq_is_constant_time() {
        use std::time::Instant;

        const ITERS: u32 = 100_000;
        const MAX_RATIO: u128 = 10;

        let expected_hash = blake3::hash(b"super-secret-gateway-token");
        let candidates: &[&[u8]] = &[b"x", b"wrong_token_123", &[b'z'; 512]];
        let mut times_ns: Vec<u128> = Vec::with_capacity(candidates.len());

        for candidate in candidates {
            let h = blake3::hash(candidate);
            for _ in 0..1_000 {
                let _ = h.as_bytes().ct_eq(expected_hash.as_bytes());
            }
            let start = Instant::now();
            for _ in 0..ITERS {
                let _ = h.as_bytes().ct_eq(expected_hash.as_bytes());
            }
            times_ns.push(start.elapsed().as_nanos() / u128::from(ITERS));
        }

        let min = *times_ns.iter().min().unwrap();
        let max = *times_ns.iter().max().unwrap();
        assert!(
            min > 0 && max / min < MAX_RATIO,
            "ct_eq timing ratio {max}/{min} exceeds {MAX_RATIO}×; times per iter: {times_ns:?} ns"
        );
    }
}
