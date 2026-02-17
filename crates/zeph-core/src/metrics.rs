use tokio::sync::watch;

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
    pub embeddings_generated: u64,
    pub last_llm_latency_ms: u64,
    pub uptime_seconds: u64,
    pub provider_name: String,
    pub model_name: String,
    pub summaries_count: u64,
    pub context_compactions: u64,
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
}
