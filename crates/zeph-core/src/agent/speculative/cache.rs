// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-flight speculative handle cache.
//!
//! Keyed by `(ToolName, blake3::Hash)` where the hash covers the tool's argument map.
//! Bounded by `max_in_flight`; oldest handle (by `started_at`) is evicted and cancelled
//! when the bound is exceeded.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use zeph_common::ToolName;
use zeph_common::task_supervisor::{BlockingError, BlockingHandle};
use zeph_tools::{ToolError, ToolOutput};

/// Unique key for a speculative handle: tool name + BLAKE3 hash of normalized args.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HandleKey {
    pub tool_id: ToolName,
    pub args_hash: blake3::Hash,
}

/// An in-flight speculative execution handle.
///
/// Created when the engine dispatches a speculative tool call. Committed when the LLM
/// confirms the same call on `ToolUseStop`; cancelled on mismatch or TTL expiry.
pub struct SpeculativeHandle {
    pub key: HandleKey,
    pub join: BlockingHandle<Result<Option<ToolOutput>, ToolError>>,
    pub cancel: CancellationToken,
    /// Absolute wall-clock deadline; handle is cancelled by the sweeper when exceeded.
    pub ttl_deadline: tokio::time::Instant,
    pub started_at: Instant,
}

impl std::fmt::Debug for SpeculativeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeculativeHandle")
            .field("key", &self.key)
            .field("ttl_deadline", &self.ttl_deadline)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

impl SpeculativeHandle {
    /// Cancel the in-flight task.
    pub fn cancel(self) {
        self.cancel.cancel();
        self.join.abort();
    }

    /// Await the result; blocks until the task finishes or is cancelled.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] if the task was cancelled or panicked.
    pub async fn commit(self) -> Result<Option<ToolOutput>, ToolError> {
        match self.join.join().await {
            Ok(r) => r,
            Err(BlockingError::Panicked) => Err(ToolError::Execution(std::io::Error::other(
                "speculative task panicked",
            ))),
            Err(BlockingError::SupervisorDropped) => Err(ToolError::Execution(
                std::io::Error::other("speculative task cancelled"),
            )),
        }
    }
}

pub struct CacheInner {
    pub handles: HashMap<HandleKey, SpeculativeHandle>,
}

/// Cache for in-flight speculative handles, bounded by `max_in_flight`.
///
/// Thread-safe; all operations hold a short `parking_lot::Mutex` lock.
/// The inner `Arc<Mutex<CacheInner>>` is shared with the background TTL sweeper so
/// both operate on the same handle set (C2: no separate empty instance in the sweeper).
pub struct SpeculativeCache {
    pub(crate) inner: Arc<Mutex<CacheInner>>,
    max: usize,
}

impl SpeculativeCache {
    /// Create a new cache with the given capacity.
    #[must_use]
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner {
                handles: HashMap::new(),
            })),
            max: max_in_flight.clamp(1, 16),
        }
    }

    /// Return a cloned `Arc` to the inner mutex so it can be shared with a sweeper task.
    ///
    /// The sweeper calls [`SpeculativeCache::sweep_expired_inner`] on the shared `Arc`
    /// instead of constructing a second `SpeculativeCache` that would have separate storage.
    #[must_use]
    pub fn shared_inner(&self) -> Arc<Mutex<CacheInner>> {
        Arc::clone(&self.inner)
    }

    /// Cancel and remove all handles whose TTL deadline has passed, operating on a raw `Arc`.
    ///
    /// Intended for use by the sweeper task, which holds only the `Arc` (not a full
    /// `SpeculativeCache` wrapper).
    pub fn sweep_expired_inner(inner: &Mutex<CacheInner>) {
        let now = tokio::time::Instant::now();
        let mut g = inner.lock();
        let expired: Vec<HandleKey> = g
            .handles
            .iter()
            .filter(|(_, h)| h.ttl_deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(h) = g.handles.remove(&key) {
                h.cancel();
            }
        }
    }

    /// Insert a new handle. If at capacity, evicts and cancels the oldest.
    ///
    /// If a handle with the same key already exists it is replaced and explicitly cancelled
    /// so the underlying tokio task does not keep running (C4: no silent drop).
    pub fn insert(&self, handle: SpeculativeHandle) {
        let mut g = self.inner.lock();
        if g.handles.len() >= self.max {
            let oldest_key = g
                .handles
                .values()
                .min_by_key(|h| h.started_at)
                .map(|h| h.key.clone());
            if let Some(key) = oldest_key
                && let Some(evicted) = g.handles.remove(&key)
            {
                evicted.cancel();
            }
        }
        if let Some(displaced) = g.handles.insert(handle.key.clone(), handle) {
            displaced.cancel();
        }
    }

    /// Find and remove a handle matching `tool_id` + `args_hash`.
    #[must_use]
    pub fn take_match(
        &self,
        tool_id: &ToolName,
        args_hash: &blake3::Hash,
    ) -> Option<SpeculativeHandle> {
        let key = HandleKey {
            tool_id: tool_id.clone(),
            args_hash: *args_hash,
        };
        self.inner.lock().handles.remove(&key)
    }

    /// Remove and cancel the first handle whose `tool_id` matches, if any.
    ///
    /// Used when the args hash is not known (e.g., on tool-id mismatch at dispatch time).
    pub fn cancel_by_tool_id(&self, tool_id: &ToolName) {
        let mut g = self.inner.lock();
        let key = g.handles.keys().find(|k| &k.tool_id == tool_id).cloned();
        if let Some(key) = key
            && let Some(h) = g.handles.remove(&key)
        {
            h.cancel();
        }
    }

    /// Cancel and remove all handles whose TTL deadline has passed.
    ///
    /// Called by the sweeper task every 5 s.
    pub fn sweep_expired(&self) {
        Self::sweep_expired_inner(&self.inner);
    }

    /// Cancel and remove all remaining handles (called at turn boundary).
    pub fn cancel_all(&self) {
        let mut g = self.inner.lock();
        for (_, h) in g.handles.drain() {
            h.cancel();
        }
    }

    /// Number of in-flight handles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().handles.len()
    }

    /// True when the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Compute a BLAKE3 hash over a normalized JSON args map.
///
/// Keys are sorted lexicographically before hashing to ensure arg-order independence.
#[must_use]
pub fn hash_args(args: &serde_json::Map<String, serde_json::Value>) -> blake3::Hash {
    let mut keys: Vec<&str> = args.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    for k in keys {
        hasher.update(k.as_bytes());
        hasher.update(b"\x00");
        let v = args[k].to_string();
        hasher.update(v.as_bytes());
        hasher.update(b"\x00");
    }
    hasher.finalize()
}

/// Produce a normalized args template: top-level keys with their JSON type as placeholder value.
///
/// Used by `PatternStore` to store a template that is stable across observations with varying
/// argument values. Example: `{"command":"<string>","timeout":"<number>"}`.
#[must_use]
pub fn args_template(args: &serde_json::Map<String, serde_json::Value>) -> String {
    let template: serde_json::Map<String, serde_json::Value> = args
        .iter()
        .map(|(k, v)| {
            let placeholder = match v {
                serde_json::Value::String(_) => serde_json::json!("<string>"),
                serde_json::Value::Number(_) => serde_json::json!("<number>"),
                serde_json::Value::Bool(_) => serde_json::json!("<bool>"),
                serde_json::Value::Array(_) => serde_json::json!("<array>"),
                serde_json::Value::Object(_) => serde_json::json!("<object>"),
                serde_json::Value::Null => serde_json::json!(null),
            };
            (k.clone(), placeholder)
        })
        .collect();
    serde_json::Value::Object(template).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_args_order_independent() {
        let mut a = serde_json::Map::new();
        a.insert("z".into(), serde_json::json!(1));
        a.insert("a".into(), serde_json::json!(2));

        let mut b = serde_json::Map::new();
        b.insert("a".into(), serde_json::json!(2));
        b.insert("z".into(), serde_json::json!(1));

        assert_eq!(hash_args(&a), hash_args(&b));
    }

    #[test]
    fn hash_args_different_values() {
        let mut a = serde_json::Map::new();
        a.insert("x".into(), serde_json::json!(1));
        let mut b = serde_json::Map::new();
        b.insert("x".into(), serde_json::json!(2));
        assert_ne!(hash_args(&a), hash_args(&b));
    }

    #[test]
    fn args_template_replaces_values_with_type_placeholders() {
        let mut m = serde_json::Map::new();
        m.insert("cmd".into(), serde_json::json!("ls -la"));
        m.insert("timeout".into(), serde_json::json!(30));
        m.insert("flag".into(), serde_json::json!(true));
        let t = args_template(&m);
        assert!(t.contains("<string>"));
        assert!(t.contains("<number>"));
        assert!(t.contains("<bool>"));
    }
}
