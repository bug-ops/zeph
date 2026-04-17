// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error lineage tracking for DAG cascade abort defense (arXiv:2603.04474).
//!
//! Tracks consecutive failure chains across `depends_on` paths. When N consecutive
//! nodes in a dependency chain all fail, the DAG is aborted to prevent silent
//! propagation of a root failure through the entire graph.
//!
//! Lineage is stored as a **side-table on [`DagScheduler`]** — not on [`TaskNode`] —
//! to avoid database blob bloat and to keep lineage as a derived, runtime-only signal.
//!
//! [`DagScheduler`]: crate::scheduler::DagScheduler
//! [`TaskNode`]: crate::graph::TaskNode

use std::time::{SystemTime, UNIX_EPOCH};

use super::graph::TaskId;

/// Returns the current time as milliseconds since UNIX epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Classifies the nature of an entry in the error lineage chain.
///
/// Only `Failed` is constructible in v1. `WeakOutput` is reserved for
/// future use once predicate confidence distribution is calibrated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageKind {
    /// Task failed with the given error class (e.g., `"timeout"`, `"llm_error"`).
    Failed { error_class: String },
}

/// A single entry in an [`ErrorLineage`] chain.
///
/// Records which task failed, why, and when.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::lineage::{LineageEntry, LineageKind};
/// use zeph_orchestration::graph::TaskId;
///
/// let entry = LineageEntry {
///     task_id: TaskId(1),
///     kind: LineageKind::Failed { error_class: "timeout".to_string() },
///     ts_ms: 1_000_000,
/// };
/// assert_eq!(entry.task_id, TaskId(1));
/// ```
#[derive(Debug, Clone)]
pub struct LineageEntry {
    /// The task that failed.
    pub task_id: TaskId,
    /// Why this entry was added to the chain.
    pub kind: LineageKind,
    /// Timestamp in milliseconds since UNIX epoch when the entry was recorded.
    pub ts_ms: u64,
}

/// Classifies an error string into a short error class label for lineage entries.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::lineage::classify_error;
///
/// assert_eq!(classify_error("task timed out after 30s"), "timeout");
/// assert_eq!(classify_error("LLM returned 429"), "rate_limit");
/// assert_eq!(classify_error("unknown issue"), "unknown");
/// ```
#[must_use]
pub fn classify_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".to_string()
    } else if lower.contains("429") || lower.contains("rate limit") || lower.contains("rate_limit")
    {
        "rate_limit".to_string()
    } else if lower.contains("canceled") || lower.contains("cancelled") {
        "canceled".to_string()
    } else if lower.contains("llm") || lower.contains("provider") || lower.contains("inference") {
        "llm_error".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Tracks the consecutive error chain for a single dependency path in the DAG.
///
/// Entries are stored in order from earliest to latest. New entries are appended
/// with [`ErrorLineage::push`]; parent chains are merged with [`ErrorLineage::merge`].
///
/// # Expiry
///
/// Entries older than the configured TTL are pruned during [`ErrorLineage::merge`]
/// via [`ErrorLineage::is_recent`].
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::lineage::{ErrorLineage, LineageEntry, LineageKind};
/// use zeph_orchestration::graph::TaskId;
///
/// let mut chain = ErrorLineage::default();
/// chain.push(LineageEntry {
///     task_id: TaskId(0),
///     kind: LineageKind::Failed { error_class: "timeout".to_string() },
///     ts_ms: 1_000,
/// });
/// assert_eq!(chain.consecutive_failed_len(), 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct ErrorLineage {
    /// Ordered list of lineage entries; earliest first.
    entries: Vec<LineageEntry>,
}

impl ErrorLineage {
    /// Append a new entry to the chain.
    pub fn push(&mut self, entry: LineageEntry) {
        self.entries.push(entry);
    }

    /// Returns true when the oldest entry in this chain is within `ttl_secs` of now.
    ///
    /// An empty chain is always considered recent (no entries to expire).
    #[must_use]
    pub fn is_recent(&self, ttl_secs: u64) -> bool {
        match self.entries.first() {
            None => true,
            Some(entry) => now_ms().saturating_sub(entry.ts_ms) <= ttl_secs * 1000,
        }
    }

    /// Merge another chain's entries into this one, preserving temporal order.
    ///
    /// Only entries that are `is_recent(ttl_secs)` are included. Duplicate task
    /// IDs are allowed — they represent separate failure events in the lineage.
    pub fn merge(&mut self, other: &ErrorLineage, ttl_secs: u64) {
        if other.is_recent(ttl_secs) {
            for entry in &other.entries {
                self.entries.push(entry.clone());
            }
        }
    }

    /// Returns the number of consecutive `Failed` entries at the tail of the chain.
    ///
    /// Used to detect linear-waterfall cascades: if this value reaches the configured
    /// `cascade_chain_threshold`, the DAG is aborted.
    #[must_use]
    pub fn consecutive_failed_len(&self) -> usize {
        // All current v1 entries are Failed — count from tail until a non-Failed entry.
        let mut count = 0;
        for entry in self.entries.iter().rev() {
            match &entry.kind {
                LineageKind::Failed { .. } => count += 1,
            }
        }
        count
    }

    /// Returns the first entry in the chain, or `None` if the chain is empty.
    #[must_use]
    pub fn first_entry(&self) -> Option<&LineageEntry> {
        self.entries.first()
    }

    /// Returns all entries as a slice.
    #[must_use]
    pub fn entries(&self) -> &[LineageEntry] {
        &self.entries
    }

    /// Returns true if the chain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(task_id: u32, ts_ms: u64) -> LineageEntry {
        LineageEntry {
            task_id: TaskId(task_id),
            kind: LineageKind::Failed {
                error_class: "timeout".to_string(),
            },
            ts_ms,
        }
    }

    #[test]
    fn empty_chain_is_recent() {
        let chain = ErrorLineage::default();
        assert!(chain.is_recent(300));
    }

    #[test]
    fn recent_entry_within_ttl() {
        let mut chain = ErrorLineage::default();
        chain.push(entry(0, now_ms()));
        assert!(chain.is_recent(300));
    }

    #[test]
    fn old_entry_outside_ttl() {
        let mut chain = ErrorLineage::default();
        // 10 minutes ago
        chain.push(entry(0, now_ms().saturating_sub(600_001)));
        assert!(!chain.is_recent(300));
    }

    #[test]
    fn consecutive_failed_len_counts_all() {
        let mut chain = ErrorLineage::default();
        chain.push(entry(0, 1000));
        chain.push(entry(1, 2000));
        chain.push(entry(2, 3000));
        assert_eq!(chain.consecutive_failed_len(), 3);
    }

    #[test]
    fn merge_appends_recent_entries() {
        let mut parent = ErrorLineage::default();
        parent.push(entry(0, now_ms()));

        let mut child = ErrorLineage::default();
        child.merge(&parent, 300);
        child.push(entry(1, now_ms()));

        assert_eq!(child.entries().len(), 2);
        assert_eq!(child.consecutive_failed_len(), 2);
    }

    #[test]
    fn merge_skips_stale_parent() {
        let mut parent = ErrorLineage::default();
        // Very old entry
        parent.push(entry(0, now_ms().saturating_sub(700_000)));

        let mut child = ErrorLineage::default();
        child.merge(&parent, 300);
        child.push(entry(1, now_ms()));

        // stale parent not merged
        assert_eq!(child.entries().len(), 1);
    }

    #[test]
    fn classify_error_timeout() {
        assert_eq!(classify_error("task timed out after 30s"), "timeout");
        assert_eq!(classify_error("Timeout exceeded"), "timeout");
    }

    #[test]
    fn classify_error_rate_limit() {
        assert_eq!(
            classify_error("LLM returned 429 Too Many Requests"),
            "rate_limit"
        );
    }

    #[test]
    fn classify_error_unknown() {
        assert_eq!(classify_error("something weird happened"), "unknown");
    }
}
