// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::{Arc, Mutex};

use crate::agent::Agent;
use crate::channel::Channel;

// Re-export for tests in this module (subgoal_extraction_tests) — test-only.
#[cfg(test)]
pub(super) use zeph_agent_context::summarization::scheduling::parse_subgoal_extraction_response;

impl<C: Channel> Agent<C> {
    /// Tiered compaction: delegates to [`ContextService::maybe_compact`].
    ///
    /// Tier logic (Soft/Hard/None), cooldown, and server-compaction guards are all
    /// handled by the service. After the service call:
    /// - Status messages collected during the service call are forwarded to `channel.send_status`.
    /// - `context_compactions` and `compaction_hard_count` metrics are updated — the service
    ///   cannot access these because `MetricsCallback` is not part of [`ContextSummarizationView`].
    pub(in crate::agent) async fn maybe_compact(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        let svc = zeph_agent_context::ContextService::new();
        let providers = self.providers();

        // Collect status messages from the service so they can be forwarded to the channel.
        // The service uses StatusSink (async trait) — CollectStatusSink gathers them synchronously.
        let status = CollectStatusSink::default();

        // Capture pre-call state to detect what the service did.
        let turns_before = self.context_manager.turns_since_last_hard_compaction;
        let msg_count_before = self.msg.messages.len();

        let mut summ = self.summarization_view();
        svc.maybe_compact(&mut summ, &providers, &status)
            .await
            .map_err(|e| crate::agent::error::AgentError::ContextError(format!("{e:#}")))?;

        // Forward collected statuses to the channel AND the TUI status sender.
        let collected = status.take();
        for msg in &collected {
            let _ = self.channel.send_status(msg).await;
        }
        if let Some(ref tx) = self.services.session.status_tx {
            for msg in &collected {
                let _ = tx.send(msg.clone());
            }
        }

        // Update metrics that the service cannot track (no MetricsCallback in ContextSummarizationView).
        let msg_count_after = self.msg.messages.len();
        let compacted = msg_count_after < msg_count_before;
        let hard_fired = self.context_manager.turns_since_last_hard_compaction == Some(0)
            && turns_before != Some(0);

        if compacted {
            self.update_metrics(|m| m.context_compactions += 1);
        }
        if hard_fired {
            let turns_segment = turns_before.unwrap_or(0);
            self.update_metrics(|m| {
                m.compaction_hard_count += 1;
                if turns_segment > 0 {
                    m.compaction_turns_after_hard.push(turns_segment);
                }
            });
        }

        Ok(())
    }

    /// Soft-only compaction for mid-iteration use inside tool execution loops.
    ///
    /// Delegates to [`ContextService::maybe_soft_compact_mid_iteration`].
    pub(in crate::agent) fn maybe_soft_compact_mid_iteration(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        let mut summ = self.summarization_view();
        svc.maybe_soft_compact_mid_iteration(&mut summ);
    }

    /// Proactive context compression: delegates to [`ContextService::maybe_proactive_compress`].
    ///
    /// The Focus-strategy auto-consolidation path is not active in this delegation
    /// (Focus fields are not in [`ContextSummarizationView`]). This is a known
    /// limitation tracked by `// TODO(review)` in service.rs.
    pub(in crate::agent) async fn maybe_proactive_compress(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        let svc = zeph_agent_context::ContextService::new();
        let providers = self.providers();
        let status = TxStatusSink(self.services.session.status_tx.clone());
        let mut summ = self.summarization_view();
        svc.maybe_proactive_compress(&mut summ, &providers, &status)
            .await;
        Ok(())
    }

    /// Emit a UX status signal when tokens were actually freed by compaction.
    pub(in crate::agent) async fn emit_compaction_status_signal(&mut self, tokens_before: u64) {
        let tokens_after = self.runtime.providers.cached_prompt_tokens;
        if tokens_after < tokens_before {
            let now_ms = u64::try_from(
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX);
            tracing::info!(
                tokens_before,
                tokens_after,
                saved = tokens_before.saturating_sub(tokens_after),
                "context compaction complete"
            );
            let _ = self
                .channel
                .send_status(&format!(
                    "Compacting: {tokens_before}→{tokens_after} tokens"
                ))
                .await;
            self.update_metrics(|m| {
                m.compaction_last_before = tokens_before;
                m.compaction_last_after = tokens_after;
                m.compaction_last_at_ms = now_ms;
            });
        }
    }
}

/// `StatusSink` adapter over an optional `UnboundedSender<String>`.
///
/// Sends status strings when the sender is present; silently drops them otherwise.
/// Mirrors the same adapter in `zeph-agent-context::service` for use in `Agent<C>`
/// delegation shims.
struct TxStatusSink(Option<tokio::sync::mpsc::UnboundedSender<String>>);

impl zeph_agent_context::StatusSink for TxStatusSink {
    fn send_status(&self, msg: &str) -> impl std::future::Future<Output = ()> + Send + '_ {
        if let Some(ref tx) = self.0 {
            let _ = tx.send(msg.to_owned());
        }
        std::future::ready(())
    }
}

/// `StatusSink` that collects status strings in-memory for later forwarding.
///
/// Used by [`Agent::maybe_compact`] to capture status messages emitted by the service
/// so they can be forwarded to `channel.send_status` (and optionally `status_tx`)
/// after the `&mut ContextSummarizationView` borrow is released.
#[derive(Default)]
struct CollectStatusSink(Arc<Mutex<Vec<String>>>);

impl CollectStatusSink {
    /// Drain and return all collected status messages.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl zeph_agent_context::StatusSink for CollectStatusSink {
    fn send_status(&self, msg: &str) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.0.lock().unwrap().push(msg.to_owned());
        std::future::ready(())
    }
}
