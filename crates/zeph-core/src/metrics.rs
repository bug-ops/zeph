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
    pub sqlite_conversation_id: Option<i64>,
    pub qdrant_available: bool,
    pub embeddings_generated: u64,
    pub last_llm_latency_ms: u64,
    pub uptime_seconds: u64,
    pub provider_name: String,
    pub model_name: String,
    pub summaries_count: u64,
    pub context_compactions: u64,
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
    fn summaries_count_tracks_summarizations() {
        let (collector, rx) = MetricsCollector::new();
        collector.update(|m| m.summaries_count += 1);
        collector.update(|m| m.summaries_count += 1);
        assert_eq!(rx.borrow().summaries_count, 2);
    }
}
