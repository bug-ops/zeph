// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Round-robin endpoint pool for Gonka network nodes.
//!
//! The pool provides lock-free, atomic selection of the next healthy endpoint
//! and automatically skips nodes that have been marked failed within their cooldown
//! window. When all nodes are in cooldown, the least-recently-failed node is chosen
//! so callers never receive an error just from endpoint selection.
//!
//! # Examples
//!
//! ```rust
//! use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
//! use std::time::Duration;
//!
//! let nodes = vec![
//!     GonkaEndpoint { base_url: "https://node1.gonka.ai".into(), address: "addr1".into() },
//!     GonkaEndpoint { base_url: "https://node2.gonka.ai".into(), address: "addr2".into() },
//! ];
//! let pool = EndpointPool::new(nodes).expect("non-empty pool");
//! let ep = pool.next();
//! println!("selected: {}", ep.base_url);
//! ```

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use url::Url;

use crate::error::LlmError;

/// A single Gonka network node endpoint.
///
/// Holds the HTTP base URL used for API requests and the on-chain address
/// that identifies the signer node in the Gonka network.
#[derive(Debug, Clone)]
pub struct GonkaEndpoint {
    /// Base HTTP URL of the node (e.g. `https://node1.gonka.ai`).
    pub base_url: String,
    /// On-chain address of the signer node.
    pub address: String,
}

/// Round-robin pool of Gonka endpoints with fail-skip cooldown.
///
/// The pool is safe to share across threads (`Sync + Send`) — all internal
/// state uses atomic operations with `Relaxed` ordering, which is sufficient
/// because the worst consequence of a torn read is routing one extra request
/// to a recently-failed node.
///
/// # Fail-skip behaviour
///
/// When [`mark_failed`](Self::mark_failed) is called for an endpoint, that
/// endpoint is skipped by [`next`](Self::next) until its cooldown expires.
/// If *all* endpoints are in cooldown simultaneously, [`next`](Self::next)
/// returns the least-recently-failed node so callers always receive a valid
/// reference and never need to handle a missing-endpoint error.
pub struct EndpointPool {
    nodes: Vec<GonkaEndpoint>,
    cursor: AtomicUsize,
    /// Stores the absolute deadline (unix nanoseconds) after which the node
    /// is considered healthy again. Zero means the node is currently healthy.
    failed_until: Vec<AtomicU64>,
}

impl std::fmt::Debug for EndpointPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointPool")
            .field("nodes", &self.nodes)
            .field("cursor", &self.cursor.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl EndpointPool {
    /// Create a new pool from the given list of nodes.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Other`] if `nodes` is empty — a pool without
    /// endpoints cannot serve any requests.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    ///
    /// let result = EndpointPool::new(vec![]);
    /// assert!(result.is_err());
    ///
    /// let pool = EndpointPool::new(vec![
    ///     GonkaEndpoint { base_url: "https://n1.example".into(), address: "a1".into() },
    /// ]).unwrap();
    /// assert_eq!(pool.len(), 1);
    /// ```
    pub fn new(nodes: Vec<GonkaEndpoint>) -> Result<Self, LlmError> {
        if nodes.is_empty() {
            return Err(LlmError::Other(
                "EndpointPool requires at least one node".into(),
            ));
        }
        for node in &nodes {
            let parsed = Url::parse(&node.base_url).map_err(|e| {
                LlmError::Other(format!("invalid endpoint URL '{}': {e}", node.base_url))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(LlmError::Other(format!(
                    "endpoint URL '{}' must use http or https scheme",
                    node.base_url
                )));
            }
        }
        let n = nodes.len();
        let failed_until = (0..n).map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            nodes,
            cursor: AtomicUsize::new(0),
            failed_until,
        })
    }

    /// Return the next non-failed endpoint using round-robin selection.
    ///
    /// The cursor is advanced atomically on every call regardless of whether
    /// the selected node is healthy. Up to `nodes.len()` candidates are
    /// checked in order. If all candidates are still within their cooldown,
    /// the node with the smallest (earliest) `failed_until` deadline is
    /// returned as a fallback so the caller always receives a valid reference.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    ///
    /// let pool = EndpointPool::new(vec![
    ///     GonkaEndpoint { base_url: "https://n1".into(), address: "a1".into() },
    ///     GonkaEndpoint { base_url: "https://n2".into(), address: "a2".into() },
    /// ]).unwrap();
    ///
    /// // First two calls return different nodes.
    /// let ep1 = pool.next();
    /// let ep2 = pool.next();
    /// assert_ne!(ep1.base_url, ep2.base_url);
    /// ```
    pub fn next(&self) -> &GonkaEndpoint {
        let _span = tracing::trace_span!("llm.gonka.endpoint_next").entered();
        let n = self.nodes.len();
        let now_ns = now_ns();

        // Scan at most `n` candidates starting from the current cursor position.
        for _ in 0..n {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
            if self.failed_until[idx].load(Ordering::Relaxed) <= now_ns {
                return &self.nodes[idx];
            }
        }

        // All nodes are in cooldown — return the least-recently-failed one.
        let best = self
            .failed_until
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| a.load(Ordering::Relaxed))
            .map_or(0, |(i, _)| i);
        &self.nodes[best]
    }

    /// Mark the endpoint at `idx` as failed for the given `cooldown` duration.
    ///
    /// After the cooldown expires, the endpoint becomes eligible for selection
    /// again. Calling with `Duration::ZERO` immediately clears the failure.
    ///
    /// # Panics
    ///
    /// Does not panic — if `idx >= len()` the call is a no-op (the index
    /// simply does not match any slot).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    /// use std::time::Duration;
    ///
    /// let pool = EndpointPool::new(vec![
    ///     GonkaEndpoint { base_url: "https://n1".into(), address: "a1".into() },
    ///     GonkaEndpoint { base_url: "https://n2".into(), address: "a2".into() },
    /// ]).unwrap();
    ///
    /// pool.mark_failed(0, Duration::from_secs(60));
    /// // Subsequent calls will skip node 0 for the next 60 seconds.
    /// let ep = pool.next();
    /// assert_eq!(ep.base_url, "https://n2");
    /// ```
    pub fn mark_failed(&self, idx: usize, cooldown: Duration) {
        if idx >= self.nodes.len() {
            return;
        }
        let cooldown_ns = u64::try_from(cooldown.as_nanos()).unwrap_or(u64::MAX);
        let deadline = now_ns().saturating_add(cooldown_ns);
        self.failed_until[idx].store(deadline, Ordering::Relaxed);
    }

    /// Number of endpoints in the pool.
    ///
    /// Always at least 1 after successful construction.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the pool has no endpoints.
    ///
    /// This always returns `false` after a successful [`EndpointPool::new`]
    /// call, because the constructor rejects empty slices.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Like [`next`](Self::next) but also returns the internal node index.
    ///
    /// The index can be passed directly to [`mark_failed`](Self::mark_failed) so
    /// the correct pool slot is penalised when a request fails. Using [`next`](Self::next)
    /// alone does not expose the selected index, which makes it impossible to call
    /// `mark_failed` correctly in a retry loop.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    /// use std::time::Duration;
    ///
    /// let pool = EndpointPool::new(vec![
    ///     GonkaEndpoint { base_url: "https://n1".into(), address: "a1".into() },
    ///     GonkaEndpoint { base_url: "https://n2".into(), address: "a2".into() },
    /// ]).unwrap();
    ///
    /// let (idx, ep) = pool.next_indexed();
    /// assert!(idx < pool.len());
    /// assert!(!ep.base_url.is_empty());
    /// ```
    pub fn next_indexed(&self) -> (usize, &GonkaEndpoint) {
        let _span = tracing::trace_span!("llm.gonka.endpoint_next").entered();
        let n = self.nodes.len();
        let now_ns = now_ns();

        for _ in 0..n {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
            if self.failed_until[idx].load(Ordering::Relaxed) <= now_ns {
                return (idx, &self.nodes[idx]);
            }
        }

        let best = self
            .failed_until
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| a.load(Ordering::Relaxed))
            .map_or(0, |(i, _)| i);
        (best, &self.nodes[best])
    }
}

/// Current time in unix nanoseconds, saturating to 0 on pre-epoch clocks.
#[inline]
pub(crate) fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    fn make_pool(n: usize) -> EndpointPool {
        let nodes = (0..n)
            .map(|i| GonkaEndpoint {
                base_url: format!("https://node{i}.example"),
                address: format!("addr{i}"),
            })
            .collect();
        EndpointPool::new(nodes).expect("non-empty pool")
    }

    /// Derive the index that `next()` will return without advancing the cursor,
    /// by reading the last-advanced position post-call via `base_url` comparison.
    fn url_to_idx(url: &str, n: usize) -> usize {
        for i in 0..n {
            if url == format!("https://node{i}.example") {
                return i;
            }
        }
        panic!("unrecognised url: {url}");
    }

    /// 1. Round-robin rotates through all 3 nodes and then repeats.
    #[test]
    fn gonka_endpoint_round_robin_three_nodes() {
        let pool = make_pool(3);
        let calls: Vec<usize> = (0..6)
            .map(|_| url_to_idx(pool.next().base_url.as_str(), 3))
            .collect();
        // First 3 must be a permutation of 0,1,2 (each exactly once).
        let mut first = calls[..3].to_vec();
        first.sort_unstable();
        assert_eq!(first, vec![0, 1, 2]);
        // Second 3 must be the same cycle.
        let mut second = calls[3..].to_vec();
        second.sort_unstable();
        assert_eq!(second, vec![0, 1, 2]);
    }

    /// 2. A node with a long cooldown is never returned when healthy alternatives exist.
    #[test]
    fn gonka_endpoint_failed_node_skipped_during_cooldown() {
        let pool = make_pool(3);
        // Mark node 0 failed for an hour.
        pool.mark_failed(0, Duration::from_hours(1));

        for _ in 0..9 {
            let idx = url_to_idx(pool.next().base_url.as_str(), 3);
            assert_ne!(idx, 0, "failed node 0 must not be returned");
        }
    }

    /// 3. A node recovers immediately when marked with `Duration::ZERO`.
    #[test]
    fn gonka_endpoint_failed_node_restored_after_cooldown() {
        let pool = make_pool(2);
        // Mark node 0 as immediately-expired (zero cooldown ⇒ deadline = now, already passed).
        pool.mark_failed(0, Duration::ZERO);

        // Node 0 must appear at some point in 6 calls.
        let seen: Vec<usize> = (0..6)
            .map(|_| url_to_idx(pool.next().base_url.as_str(), 2))
            .collect();
        assert!(
            seen.contains(&0),
            "recovered node 0 must be selectable; got: {seen:?}"
        );
    }

    /// 4. When all nodes are in cooldown, `next()` still returns a valid endpoint (no panic).
    #[test]
    fn gonka_endpoint_all_failed_fallback_no_panic() {
        let pool = make_pool(3);
        for i in 0..3 {
            pool.mark_failed(i, Duration::from_hours(1));
        }
        // Must not panic and must return one of the 3 valid indices.
        for _ in 0..6 {
            let idx = url_to_idx(pool.next().base_url.as_str(), 3);
            assert!(idx < 3, "index out of range: {idx}");
        }
    }

    /// 5a. Constructor rejects invalid URL scheme.
    #[test]
    fn gonka_endpoint_invalid_scheme_returns_err() {
        let result = EndpointPool::new(vec![GonkaEndpoint {
            base_url: "ftp://node.example".into(),
            address: "addr".into(),
        }]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("http or https scheme"),
            "unexpected error message: {msg}"
        );
    }

    /// 5b. Constructor rejects unparseable URL.
    #[test]
    fn gonka_endpoint_invalid_url_returns_err() {
        let result = EndpointPool::new(vec![GonkaEndpoint {
            base_url: "not a url".into(),
            address: "addr".into(),
        }]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid endpoint URL"),
            "unexpected error message: {msg}"
        );
    }

    /// 5. Constructor rejects an empty node list.
    #[test]
    fn gonka_endpoint_empty_constructor_returns_err() {
        let result = EndpointPool::new(vec![]);
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("EndpointPool requires at least one node"),
                    "unexpected error message: {msg}"
                );
            }
            Ok(_) => panic!("expected Err for empty pool, got Ok"),
        }
    }

    /// `len()` and `is_empty()` reflect pool size.
    #[test]
    fn gonka_endpoint_len_and_is_empty() {
        let pool = make_pool(4);
        assert_eq!(pool.len(), 4);
        assert!(!pool.is_empty());
    }

    /// `mark_failed` with out-of-range index is a no-op (must not panic).
    #[test]
    fn gonka_endpoint_mark_failed_out_of_range_noop() {
        let pool = make_pool(2);
        pool.mark_failed(99, Duration::from_secs(10)); // must not panic
    }

    /// Clearing a failure by writing a zero deadline makes the node immediately available.
    #[test]
    fn gonka_endpoint_clear_failure_via_atomic_store() {
        let pool = make_pool(2);
        pool.mark_failed(0, Duration::from_hours(1));
        // Manually clear via zero duration (deadline = now() + 0 which may still be in the future
        // by a nanosecond — use the internal atomic directly for a deterministic zero).
        pool.failed_until[0].store(0, Ordering::Relaxed);

        let seen: Vec<usize> = (0..6)
            .map(|_| url_to_idx(pool.next().base_url.as_str(), 2))
            .collect();
        assert!(
            seen.contains(&0),
            "node 0 must be selectable after atomic clear"
        );
    }
}
