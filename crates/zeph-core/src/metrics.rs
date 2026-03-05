// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use tokio::sync::watch;

/// Category of a security event for TUI display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityEventCategory {
    InjectionFlag,
    ExfiltrationBlock,
    Quarantine,
    Truncation,
}

impl SecurityEventCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InjectionFlag => "injection",
            Self::ExfiltrationBlock => "exfil",
            Self::Quarantine => "quarantine",
            Self::Truncation => "truncation",
        }
    }
}

/// A single security event record for TUI display.
#[derive(Debug, Clone)]
pub struct SecurityEvent {
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    pub category: SecurityEventCategory,
    /// Source that triggered the event (e.g., `web_scrape`, `mcp_response`).
    pub source: String,
    /// Short description, capped at 128 chars.
    pub detail: String,
}

impl SecurityEvent {
    #[must_use]
    pub fn new(
        category: SecurityEventCategory,
        source: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        // IMP-1: cap source at 64 chars and strip ASCII control chars.
        let source: String = source
            .into()
            .chars()
            .filter(|c| !c.is_ascii_control())
            .take(64)
            .collect();
        // CR-1: UTF-8 safe truncation using floor_char_boundary (stable since Rust 1.82).
        let detail = detail.into();
        let detail = if detail.len() > 128 {
            let end = detail.floor_char_boundary(127);
            format!("{}…", &detail[..end])
        } else {
            detail
        };
        Self {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            category,
            source,
            detail,
        }
    }
}

/// Ring buffer capacity for security events.
pub const SECURITY_EVENT_CAP: usize = 100;

/// Bayesian confidence data for a single skill, used by TUI confidence bar.
#[derive(Debug, Clone, Default)]
pub struct SkillConfidence {
    pub name: String,
    pub posterior: f64,
    pub total_uses: u32,
}

/// Snapshot of a single sub-agent's runtime status.
#[derive(Debug, Clone, Default)]
pub struct SubAgentMetrics {
    pub id: String,
    pub name: String,
    /// Stringified `TaskState`: "working", "completed", "failed", "canceled", etc.
    pub state: String,
    pub turns_used: u32,
    pub max_turns: u32,
    pub background: bool,
    pub elapsed_secs: u64,
    /// Stringified `PermissionMode`: `"default"`, `"accept_edits"`, `"dont_ask"`,
    /// `"bypass_permissions"`, `"plan"`. Empty string when mode is `Default`.
    pub permission_mode: String,
}

#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub context_tokens: u64,
    pub api_calls: u64,
    pub active_skills: Vec<String>,
    pub total_skills: usize,
    pub mcp_server_count: usize,
    pub mcp_tool_count: usize,
    pub active_mcp_tools: Vec<String>,
    pub sqlite_message_count: u64,
    pub sqlite_conversation_id: Option<zeph_memory::ConversationId>,
    pub qdrant_available: bool,
    pub vector_backend: String,
    pub embeddings_generated: u64,
    pub last_llm_latency_ms: u64,
    pub uptime_seconds: u64,
    pub provider_name: String,
    pub model_name: String,
    pub summaries_count: u64,
    pub context_compactions: u64,
    pub compression_events: u64,
    pub compression_tokens_saved: u64,
    pub tool_output_prunes: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cost_spent_cents: f64,
    pub filter_raw_tokens: u64,
    pub filter_saved_tokens: u64,
    pub filter_applications: u64,
    pub filter_total_commands: u64,
    pub filter_filtered_commands: u64,
    pub filter_confidence_full: u64,
    pub filter_confidence_partial: u64,
    pub filter_confidence_fallback: u64,
    pub cancellations: u64,
    pub sanitizer_runs: u64,
    pub sanitizer_injection_flags: u64,
    pub sanitizer_truncations: u64,
    pub quarantine_invocations: u64,
    pub quarantine_failures: u64,
    pub exfiltration_images_blocked: u64,
    pub exfiltration_tool_urls_flagged: u64,
    pub exfiltration_memory_guards: u64,
    pub sub_agents: Vec<SubAgentMetrics>,
    pub skill_confidence: Vec<SkillConfidence>,
    /// Scheduled task summaries: `[name, kind, mode, next_run]`.
    pub scheduled_tasks: Vec<[String; 4]>,
    /// Thompson Sampling distribution snapshots: `(provider, alpha, beta)`.
    pub router_thompson_stats: Vec<(String, f64, f64)>,
    /// Ring buffer of recent security events (cap 100, FIFO eviction).
    pub security_events: VecDeque<SecurityEvent>,
}

pub struct MetricsCollector {
    tx: watch::Sender<MetricsSnapshot>,
}

impl MetricsCollector {
    #[must_use]
    pub fn new() -> (Self, watch::Receiver<MetricsSnapshot>) {
        let (tx, rx) = watch::channel(MetricsSnapshot::default());
        (Self { tx }, rx)
    }

    pub fn update(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        self.tx.send_modify(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metrics_snapshot() {
        let m = MetricsSnapshot::default();
        assert_eq!(m.total_tokens, 0);
        assert_eq!(m.api_calls, 0);
        assert!(m.active_skills.is_empty());
        assert!(m.active_mcp_tools.is_empty());
        assert_eq!(m.mcp_tool_count, 0);
        assert_eq!(m.mcp_server_count, 0);
        assert!(m.provider_name.is_empty());
        assert_eq!(m.summaries_count, 0);
    }

    #[test]
    fn metrics_collector_update() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.api_calls = 5;
            m.total_tokens = 1000;
        });
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.api_calls, 5);
        assert_eq!(snapshot.total_tokens, 1000);
    }

    #[test]
    fn metrics_collector_multiple_updates() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| m.api_calls = 1);
        collector.update(|m| m.api_calls += 1);
        assert_eq!(rx.borrow().api_calls, 2);
    }

    #[test]
    fn metrics_snapshot_clone() {
        let mut m = MetricsSnapshot::default();
        m.provider_name = "ollama".into();
        let cloned = m.clone();
        assert_eq!(cloned.provider_name, "ollama");
    }

    #[test]
    fn filter_metrics_tracking() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.filter_raw_tokens += 250;
            m.filter_saved_tokens += 200;
            m.filter_applications += 1;
        });
        collector.update(|m| {
            m.filter_raw_tokens += 100;
            m.filter_saved_tokens += 80;
            m.filter_applications += 1;
        });
        let s = rx.borrow();
        assert_eq!(s.filter_raw_tokens, 350);
        assert_eq!(s.filter_saved_tokens, 280);
        assert_eq!(s.filter_applications, 2);
    }

    #[test]
    fn filter_confidence_and_command_metrics() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| {
            m.filter_total_commands += 1;
            m.filter_filtered_commands += 1;
            m.filter_confidence_full += 1;
        });
        collector.update(|m| {
            m.filter_total_commands += 1;
            m.filter_confidence_partial += 1;
        });
        let s = rx.borrow();
        assert_eq!(s.filter_total_commands, 2);
        assert_eq!(s.filter_filtered_commands, 1);
        assert_eq!(s.filter_confidence_full, 1);
        assert_eq!(s.filter_confidence_partial, 1);
        assert_eq!(s.filter_confidence_fallback, 0);
    }

    #[test]
    fn summaries_count_tracks_summarizations() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| m.summaries_count += 1);
        collector.update(|m| m.summaries_count += 1);
        assert_eq!(rx.borrow().summaries_count, 2);
    }

    #[test]
    fn cancellations_counter_increments() {
        let (collector, rx) = MetricsCollector::new();
        assert_eq!(rx.borrow().cancellations, 0);
        collector.update(|m| m.cancellations += 1);
        collector.update(|m| m.cancellations += 1);
        assert_eq!(rx.borrow().cancellations, 2);
    }

    #[test]
    fn security_event_detail_exact_128_not_truncated() {
        let s = "a".repeat(128);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s.clone());
        assert_eq!(ev.detail, s, "128-char string must not be truncated");
    }

    #[test]
    fn security_event_detail_129_is_truncated() {
        let s = "a".repeat(129);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s);
        assert!(
            ev.detail.ends_with('…'),
            "129-char string must end with ellipsis"
        );
        assert!(
            ev.detail.len() <= 130,
            "truncated detail must be at most 130 bytes"
        );
    }

    #[test]
    fn security_event_detail_multibyte_utf8_no_panic() {
        // Each '中' is 3 bytes. 43 chars = 129 bytes — triggers truncation at a multi-byte boundary.
        let s = "中".repeat(43);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, "src", s);
        assert!(ev.detail.ends_with('…'));
    }

    #[test]
    fn security_event_source_capped_at_64_chars() {
        let long_source = "x".repeat(200);
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, long_source, "detail");
        assert_eq!(ev.source.len(), 64);
    }

    #[test]
    fn security_event_source_strips_control_chars() {
        let source = "tool\x00name\x1b[31m";
        let ev = SecurityEvent::new(SecurityEventCategory::InjectionFlag, source, "detail");
        assert!(!ev.source.contains('\x00'));
        assert!(!ev.source.contains('\x1b'));
    }

    #[test]
    fn security_event_category_as_str() {
        assert_eq!(SecurityEventCategory::InjectionFlag.as_str(), "injection");
        assert_eq!(SecurityEventCategory::ExfiltrationBlock.as_str(), "exfil");
        assert_eq!(SecurityEventCategory::Quarantine.as_str(), "quarantine");
        assert_eq!(SecurityEventCategory::Truncation.as_str(), "truncation");
    }

    #[test]
    fn ring_buffer_respects_cap_via_update() {
        let (collector, rx) = MetricsCollector::new();
        for i in 0..110u64 {
            let event = SecurityEvent::new(
                SecurityEventCategory::InjectionFlag,
                "src",
                format!("event {i}"),
            );
            collector.update(|m| {
                if m.security_events.len() >= SECURITY_EVENT_CAP {
                    m.security_events.pop_front();
                }
                m.security_events.push_back(event);
            });
        }
        let snap = rx.borrow();
        assert_eq!(snap.security_events.len(), SECURITY_EVENT_CAP);
        // FIFO: earliest events evicted, last one present
        assert!(snap.security_events.back().unwrap().detail.contains("109"));
    }

    #[test]
    fn security_events_empty_by_default() {
        let m = MetricsSnapshot::default();
        assert!(m.security_events.is_empty());
    }
}
