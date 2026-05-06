// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Adapter types that bridge `Agent<C>` internals to the callback traits declared in
//! `zeph-agent-context`.
//!
//! [`CompactionAdapters`] is a bundle struct that owns all four adapters and exposes
//! a single `populate` method so the `compact_context` shim stays at ≤10 statements.

use std::pin::Pin;
use std::sync::Arc;

use zeph_agent_context::state::{
    CompactionPersistence, CompactionProbeCallback, MetricsCallback, ProbeOutcome,
    QdrantPersistFuture, ToolOutputArchive,
};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Message;
use zeph_memory::{CategoryScore, CompactionProbeConfig};

use crate::agent::Agent;
use crate::channel::Channel;
use crate::metrics::MetricsSnapshot;

// ── MetricsCollectorCallback ──────────────────────────────────────────────────

/// Implements [`MetricsCallback`] by delegating to the agent's `watch::Sender<MetricsSnapshot>`.
///
/// Constructed from a clone of `self.runtime.metrics.metrics_tx` — cheap, does not retain
/// a borrow on `Agent<C>`.
pub(in crate::agent) struct MetricsCollectorCallback {
    tx: Option<tokio::sync::watch::Sender<MetricsSnapshot>>,
}

impl MetricsCollectorCallback {
    /// Create a new callback wrapping the given metrics sender.
    pub(in crate::agent) fn new(tx: Option<tokio::sync::watch::Sender<MetricsSnapshot>>) -> Self {
        Self { tx }
    }

    fn update(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.tx {
            tx.send_modify(f);
        }
    }
}

impl MetricsCallback for MetricsCollectorCallback {
    fn record_hard_compaction(&self, turns_since_last: Option<u32>) {
        self.update(|m| {
            m.compaction_hard_count += 1;
            if let Some(turns) = turns_since_last {
                m.compaction_turns_after_hard.push(u64::from(turns));
            }
        });
    }

    fn record_tool_output_prune(&self, count: usize) {
        self.update(|m| {
            m.tool_output_prunes = m.tool_output_prunes.saturating_add(count as u64);
        });
    }

    fn record_compaction_probe_pass(
        &self,
        score: f32,
        category_scores: Vec<CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    ) {
        self.update(|m| {
            m.compaction_probe_passes += 1;
            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Pass);
            m.last_probe_score = Some(score);
            m.last_probe_category_scores = Some(category_scores);
            m.compaction_probe_threshold = threshold;
            m.compaction_probe_hard_fail_threshold = hard_fail_threshold;
        });
    }

    fn record_compaction_probe_soft_fail(
        &self,
        score: f32,
        category_scores: Vec<CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    ) {
        self.update(|m| {
            m.compaction_probe_soft_failures += 1;
            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::SoftFail);
            m.last_probe_score = Some(score);
            m.last_probe_category_scores = Some(category_scores);
            m.compaction_probe_threshold = threshold;
            m.compaction_probe_hard_fail_threshold = hard_fail_threshold;
        });
    }

    fn record_compaction_probe_hard_fail(
        &self,
        score: f32,
        category_scores: Vec<CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    ) {
        self.update(|m| {
            m.compaction_probe_failures += 1;
            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::HardFail);
            m.last_probe_score = Some(score);
            m.last_probe_category_scores = Some(category_scores);
            m.compaction_probe_threshold = threshold;
            m.compaction_probe_hard_fail_threshold = hard_fail_threshold;
        });
    }

    fn record_compaction_probe_error(&self) {
        self.update(|m| {
            m.compaction_probe_errors += 1;
            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Error);
            m.last_probe_score = None;
            m.last_probe_category_scores = None;
        });
    }
}

// ── AgentProbe ────────────────────────────────────────────────────────────────

/// Implements [`CompactionProbeCallback`] using `zeph_memory::validate_compaction`.
///
/// Owns cloned `Arc`s so it does not retain a borrow on `Agent<C>` after construction.
/// Per the probe contract: calls `dump_compaction_probe`, updates all four metric counters,
/// and returns only the routing verdict.
pub(in crate::agent) struct AgentProbe {
    probe_cfg: CompactionProbeConfig,
    probe_provider: AnyProvider,
    metrics: MetricsCollectorCallback,
    debug_dumper: Option<crate::debug_dump::DebugDumper>,
}

impl AgentProbe {
    /// Construct from cloned values extracted from `Agent<C>`.
    pub(in crate::agent) fn new<C: Channel>(agent: &Agent<C>) -> Self {
        let probe_cfg = agent.context_manager.compression.probe.clone();
        let probe_provider = agent.probe_or_summary_provider().clone();
        let metrics = MetricsCollectorCallback::new(agent.runtime.metrics.metrics_tx.clone());
        let debug_dumper = agent.runtime.debug.debug_dumper.clone();
        Self {
            probe_cfg,
            probe_provider,
            metrics,
            debug_dumper,
        }
    }
}

impl CompactionProbeCallback for AgentProbe {
    fn validate<'a>(
        &'a mut self,
        to_compact: &'a [Message],
        summary: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>> {
        Box::pin(async move {
            if !self.probe_cfg.enabled {
                return ProbeOutcome::Pass;
            }

            let result = zeph_memory::validate_compaction(
                self.probe_provider.clone(),
                to_compact.to_vec(),
                summary.to_owned(),
                &self.probe_cfg,
            )
            .await;

            match result {
                Err(e) => {
                    tracing::warn!("compaction probe error (non-blocking): {e:#}");
                    self.metrics.record_compaction_probe_error();
                    ProbeOutcome::Pass
                }
                Ok(None) => ProbeOutcome::Pass,
                Ok(Some(ref probe_result)) => {
                    if let Some(ref dumper) = self.debug_dumper {
                        dumper.dump_compaction_probe(probe_result);
                    }

                    let score = probe_result.score;
                    let cats = probe_result.category_scores.clone();
                    let threshold = probe_result.threshold;
                    let hard_fail = probe_result.hard_fail_threshold;

                    match probe_result.verdict {
                        zeph_memory::ProbeVerdict::Pass => {
                            tracing::info!(score, "compaction probe passed");
                            self.metrics
                                .record_compaction_probe_pass(score, cats, threshold, hard_fail);
                            ProbeOutcome::Pass
                        }
                        zeph_memory::ProbeVerdict::SoftFail => {
                            tracing::warn!(
                                score,
                                threshold,
                                "compaction probe SOFT FAIL — proceeding with warning"
                            );
                            self.metrics.record_compaction_probe_soft_fail(
                                score, cats, threshold, hard_fail,
                            );
                            ProbeOutcome::SoftFail
                        }
                        zeph_memory::ProbeVerdict::HardFail => {
                            tracing::warn!(
                                score,
                                threshold = hard_fail,
                                "compaction probe HARD FAIL — keeping original messages"
                            );
                            self.metrics.record_compaction_probe_hard_fail(
                                score, cats, threshold, hard_fail,
                            );
                            ProbeOutcome::HardFail
                        }
                        zeph_memory::ProbeVerdict::Error => {
                            // validate_compaction returns Err on errors, not Ok(Error).
                            debug_assert!(false, "ProbeVerdict::Error reached inside Ok path");
                            self.metrics.record_compaction_probe_error();
                            ProbeOutcome::Pass
                        }
                    }
                }
            }
        })
    }
}

// ── AgentArchive ──────────────────────────────────────────────────────────────

/// Implements [`ToolOutputArchive`] using the agent's `SQLite` memory store (Memex #2432).
///
/// Saves non-empty, non-archived tool output bodies to `tool_overflow` and returns
/// reference strings for injection as a postfix after LLM summarization.
pub(in crate::agent) struct AgentArchive {
    archive_enabled: bool,
    memory: Option<Arc<zeph_memory::semantic::SemanticMemory>>,
    conversation_id: Option<zeph_memory::ConversationId>,
}

impl AgentArchive {
    /// Construct from values extracted from `Agent<C>`.
    pub(in crate::agent) fn new<C: Channel>(agent: &Agent<C>) -> Self {
        Self {
            archive_enabled: agent.context_manager.compression.archive_tool_outputs,
            memory: agent.services.memory.persistence.memory.clone(),
            conversation_id: agent.services.memory.persistence.conversation_id,
        }
    }
}

impl ToolOutputArchive for AgentArchive {
    fn archive<'a>(
        &'a self,
        to_compact: &'a [Message],
    ) -> Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            if !self.archive_enabled {
                return Vec::new();
            }
            let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
                return Vec::new();
            };

            let mut refs = Vec::new();
            let sqlite = memory.sqlite().clone();

            for msg in to_compact {
                for part in &msg.parts {
                    if let zeph_llm::provider::MessagePart::ToolOutput {
                        body, tool_name, ..
                    } = part
                    {
                        if body.is_empty()
                            || body.starts_with("[archived:")
                            || body.starts_with("[full output stored")
                            || body.starts_with("[tool output pruned")
                        {
                            continue;
                        }
                        match sqlite.save_archive(cid.0, body.as_bytes()).await {
                            Ok(uuid) => {
                                let bytes = body.len();
                                refs.push(format!(
                                    "[archived:{uuid} — tool: {tool_name} — {bytes} bytes]"
                                ));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "Memex: failed to archive tool output (non-fatal)"
                                );
                            }
                        }
                    }
                }
            }

            if !refs.is_empty() {
                tracing::debug!(
                    archived = refs.len(),
                    "Memex: archived tool outputs before compaction"
                );
            }
            refs
        })
    }
}

// ── AgentPersistence ──────────────────────────────────────────────────────────

/// Implements [`CompactionPersistence`] by persisting to `SQLite` synchronously and
/// returning a `'static` Qdrant future for off-thread dispatch.
///
/// The `SQLite` path runs inline (it is fast and failure is non-fatal).
/// The Qdrant path is returned as a boxed future to be dispatched through
/// `BackgroundSupervisor::spawn_summarization`.
pub(in crate::agent) struct AgentPersistence {
    memory: Option<Arc<zeph_memory::semantic::SemanticMemory>>,
    conversation_id: Option<zeph_memory::ConversationId>,
}

impl AgentPersistence {
    /// Construct from values extracted from `Agent<C>`.
    pub(in crate::agent) fn new<C: Channel>(agent: &Agent<C>) -> Self {
        Self {
            memory: agent.services.memory.persistence.memory.clone(),
            conversation_id: agent.services.memory.persistence.conversation_id,
        }
    }
}

impl CompactionPersistence for AgentPersistence {
    fn after_compaction<'a>(
        &'a self,
        compacted_count: usize,
        summary_content: &'a str,
        summary: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = (bool, Option<QdrantPersistFuture>)> + Send + 'a>>
    {
        Box::pin(async move {
            let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
                return (false, None);
            };

            // Synchronous SQLite persist — clone DbStore so no &SemanticMemory survives .await.
            let sqlite = memory.sqlite().clone();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            let sqlite_failed = match ids {
                Ok(ids) if ids.len() >= 2 => {
                    let start = ids[1];
                    let end = ids[compacted_count.min(ids.len() - 1)];
                    if let Err(e) = sqlite
                        .replace_conversation(cid, start..=end, "system", summary_content)
                        .await
                    {
                        tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                        true
                    } else {
                        false
                    }
                }
                Ok(_) => false,
                Err(e) => {
                    tracing::warn!("failed to get message ids for compaction: {e:#}");
                    true
                }
            };

            // Build the Qdrant future as a 'static boxed future (clone Arc, own String).
            let memory_arc = Arc::clone(memory);
            let summary_owned = summary.to_owned();
            let qdrant_fut: QdrantPersistFuture = Box::pin(async move {
                if let Err(e) = memory_arc.store_session_summary(cid, &summary_owned).await {
                    tracing::warn!("failed to store session summary: {e:#}");
                }
                false
            });

            (sqlite_failed, Some(qdrant_fut))
        })
    }
}

// ── CompactionAdapters bundle ─────────────────────────────────────────────────

/// Bundle of all compaction adapters for `Agent<C>`.
///
/// Constructed once from `&mut Agent<C>` in the `compact_context` shim, then wired
/// into the [`zeph_agent_context::state::ContextSummarizationView`] via [`Self::populate`].
/// This collapses adapter constructions into one shim statement.
pub(in crate::agent) struct CompactionAdapters {
    probe: AgentProbe,
    archive: AgentArchive,
    persistence: AgentPersistence,
    metrics: MetricsCollectorCallback,
    typed_pages: Option<std::sync::Arc<zeph_context::typed_page::TypedPagesState>>,
}

impl CompactionAdapters {
    /// Build all adapters from the agent. Only reads fields — does not retain a borrow.
    pub(in crate::agent) fn new<C: Channel>(agent: &Agent<C>) -> Self {
        let probe = AgentProbe::new(agent);
        let archive = AgentArchive::new(agent);
        let persistence = AgentPersistence::new(agent);
        let metrics = MetricsCollectorCallback::new(agent.runtime.metrics.metrics_tx.clone());
        let typed_pages = agent.services.compression.typed_pages_state.clone();
        Self {
            probe,
            archive,
            persistence,
            metrics,
            typed_pages,
        }
    }

    /// Wire all adapters into `summ` in a single call.
    ///
    /// The shim calls this immediately after `summarization_view()` and
    /// `with_compression_guidelines`.
    pub(in crate::agent) fn populate<'a>(
        &'a mut self,
        summ: &mut zeph_agent_context::state::ContextSummarizationView<'a>,
    ) {
        summ.probe = Some(&mut self.probe);
        summ.archive = Some(&self.archive);
        summ.persistence = Some(&self.persistence);
        summ.metrics = Some(&self.metrics);
        summ.typed_pages.clone_from(&self.typed_pages);
    }
}
