// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::Role;

use super::Agent;
use super::context;
use super::error;
use zeph_agent_feedback as feedback_detector;

/// Convert a `FeedbackVerdict` (from `LlmClassifier`) into a `CorrectionSignal`.
///
/// Mirrors `JudgeVerdict::into_signal` to keep both code paths symmetric.
pub(super) fn feedback_verdict_into_signal(
    verdict: &zeph_llm::classifier::llm::FeedbackVerdict,
    user_message: &str,
) -> Option<feedback_detector::CorrectionSignal> {
    if !verdict.is_correction {
        return None;
    }
    let confidence = verdict.confidence.clamp(0.0, 1.0);
    let kind_raw = verdict.kind.trim().to_lowercase().replace(' ', "_");
    let kind = match kind_raw.as_str() {
        "explicit_rejection" => feedback_detector::CorrectionKind::ExplicitRejection,
        "alternative_request" => feedback_detector::CorrectionKind::AlternativeRequest,
        "repetition" => feedback_detector::CorrectionKind::Repetition,
        "self_correction" => feedback_detector::CorrectionKind::SelfCorrection,
        other => {
            tracing::warn!(
                kind = other,
                "llm-classifier returned unknown correction kind, discarding"
            );
            return None;
        }
    };
    Some(feedback_detector::CorrectionSignal {
        confidence,
        kind,
        feedback_text: user_message.to_owned(),
    })
}

/// Store a correction record in memory (shared by judge and llm-classifier paths).
pub(super) async fn store_correction_in_memory(
    memory: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
    conv_id: Option<zeph_memory::ConversationId>,
    assistant_snippet: &str,
    user_msg: &str,
    skill_name: String,
    kind_str: &str,
) {
    let Some(mem) = memory else { return };
    let correction_text = context::truncate_chars(user_msg, 500);
    match mem
        .sqlite()
        .store_user_correction(
            conv_id.map(|c| c.0),
            assistant_snippet,
            &correction_text,
            if skill_name.is_empty() {
                None
            } else {
                Some(skill_name.as_str())
            },
            kind_str,
        )
        .await
    {
        Ok(correction_id) => {
            if let Err(e) = mem
                .store_correction_embedding(correction_id, &correction_text)
                .await
            {
                tracing::warn!("failed to store correction embedding: {e:#}");
            }
        }
        Err(e) => {
            tracing::warn!("failed to store judge correction: {e:#}");
        }
    }
}

impl<C: crate::channel::Channel> Agent<C> {
    /// Spawn a background task to evaluate the user message with the LLM judge (or `LlmClassifier`)
    /// and store the correction result. Non-blocking: the task runs independently of the response
    /// pipeline.
    ///
    /// # Notes
    ///
    /// Tasks are supervised via [`BackgroundSupervisor`] (`TaskClass::Enrichment`).
    /// If the concurrency limit is reached, the correction check is silently dropped —
    /// corrections are non-critical lossy data.
    pub(super) fn spawn_judge_correction_check(
        &mut self,
        trimmed: &str,
        conv_id: Option<zeph_memory::ConversationId>,
    ) {
        let assistant_snippet = self.last_assistant_response();
        let user_msg_owned = trimmed.to_owned();
        let memory_arc = self.memory_state.persistence.memory.clone();
        let skill_name = self
            .skill_state
            .active_skill_names
            .first()
            .cloned()
            .unwrap_or_default();
        let conv_id_bg = conv_id;
        let confidence_threshold = self
            .learning_engine
            .config
            .as_ref()
            .map_or(0.6_f32, |c| c.correction_confidence_threshold);

        if let Some(llm_classifier) = self.feedback.llm_classifier.clone() {
            let classifier_metrics_bg = self.metrics.classifier_metrics.clone();
            let metrics_tx_bg = self.metrics.metrics_tx.clone();
            self.lifecycle.supervisor.spawn(
                super::agent_supervisor::TaskClass::Enrichment,
                "llm_classifier_correction",
                evaluate_with_llm_classifier(
                    llm_classifier,
                    user_msg_owned,
                    assistant_snippet,
                    confidence_threshold,
                    classifier_metrics_bg,
                    metrics_tx_bg,
                    memory_arc,
                    conv_id_bg,
                    skill_name,
                ),
            );
        } else {
            let judge_provider = self
                .providers
                .judge_provider
                .clone()
                .unwrap_or_else(|| self.provider.clone());
            self.lifecycle.supervisor.spawn(
                super::agent_supervisor::TaskClass::Enrichment,
                "judge_correction",
                evaluate_with_judge(
                    judge_provider,
                    user_msg_owned,
                    assistant_snippet,
                    confidence_threshold,
                    memory_arc,
                    conv_id_bg,
                    skill_name,
                ),
            );
        }
    }

    /// Detect implicit corrections in the user's message and record them in the learning engine.
    ///
    /// Uses regex-based `FeedbackDetector` first. If a `JudgeDetector` is configured and the
    /// regex result is borderline, the LLM judge runs in a background task (non-blocking).
    /// When `DetectorMode::Model` and an `LlmClassifier` is attached, the LLM classifier is
    /// used instead of `JudgeDetector`, sharing the same adaptive thresholds and rate limiter.
    pub(super) async fn detect_and_record_corrections(
        &mut self,
        trimmed: &str,
        conv_id: Option<zeph_memory::ConversationId>,
    ) {
        let correction_detection_enabled = self
            .learning_engine
            .config
            .as_ref()
            .is_none_or(|c| c.correction_detection);
        if !correction_detection_enabled {
            return;
        }

        let previous_user_messages = self.collect_previous_user_messages();
        let regex_signal = self
            .feedback
            .detector
            .detect(trimmed, &previous_user_messages);

        let judge_should_run = self.should_run_judge(regex_signal.as_ref());

        let (signal, signal_source) = if judge_should_run {
            self.spawn_judge_correction_check(trimmed, conv_id);
            (None, "judge")
        } else {
            (regex_signal, "regex")
        };

        let Some(signal) = signal else { return };
        tracing::info!(
            kind = signal.kind.as_str(),
            confidence = signal.confidence,
            source = signal_source,
            "implicit correction detected"
        );
        let feedback_text = context::truncate_chars(&signal.feedback_text, 500);
        if self.is_learning_enabled()
            && signal.kind != feedback_detector::CorrectionKind::SelfCorrection
        {
            self.record_skill_outcomes(
                "user_rejection",
                Some(&feedback_text),
                Some(signal.kind.as_str()),
            )
            .await;
        }
        self.store_user_correction_inline(trimmed, conv_id, signal.kind.as_str())
            .await;
    }

    fn collect_previous_user_messages(&self) -> Vec<&str> {
        self.msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect()
    }

    fn should_run_judge(
        &mut self,
        regex_signal: Option<&feedback_detector::CorrectionSignal>,
    ) -> bool {
        if self.feedback.llm_classifier.is_some() {
            let adaptive_low = self
                .learning_engine
                .config
                .as_ref()
                .map_or(0.5, |c| c.judge_adaptive_low);
            let adaptive_high = self
                .learning_engine
                .config
                .as_ref()
                .map_or(0.8, |c| c.judge_adaptive_high);
            let should_invoke = self
                .feedback
                .judge
                .get_or_insert_with(|| {
                    feedback_detector::JudgeDetector::new(adaptive_low, adaptive_high)
                })
                .should_invoke(regex_signal);
            should_invoke
                && self
                    .feedback
                    .judge
                    .as_mut()
                    .is_some_and(feedback_detector::JudgeDetector::check_rate_limit)
        } else {
            self.feedback
                .judge
                .as_ref()
                .is_some_and(|jd| jd.should_invoke(regex_signal))
                && self
                    .feedback
                    .judge
                    .as_mut()
                    .is_some_and(feedback_detector::JudgeDetector::check_rate_limit)
        }
    }

    async fn store_user_correction_inline(
        &self,
        trimmed: &str,
        conv_id: Option<zeph_memory::ConversationId>,
        kind_str: &str,
    ) {
        let Some(memory) = &self.memory_state.persistence.memory else {
            return;
        };
        let correction_text = context::truncate_chars(trimmed, 500);
        match memory
            .sqlite()
            .store_user_correction(
                conv_id.map(|c| c.0),
                "",
                &correction_text,
                self.skill_state
                    .active_skill_names
                    .first()
                    .map(String::as_str),
                kind_str,
            )
            .await
        {
            Ok(correction_id) => {
                if let Err(e) = memory
                    .store_correction_embedding(correction_id, &correction_text)
                    .await
                {
                    tracing::warn!("failed to store correction embedding: {e:#}");
                }
            }
            Err(e) => tracing::warn!("failed to store user correction: {e:#}"),
        }
    }

    /// Post-dispatch learning hook called from the agent loop after a registry command sends its
    /// `Message` response. Triggers `generate_improved_skill` for `/skill reject` and `/feedback`
    /// commands — these require `&mut self` and cannot run inside the `Send` future in
    /// `agent_access_impl.rs`.
    pub(super) async fn maybe_trigger_post_command_learning(&mut self, trimmed: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let rest = if let Some(r) = trimmed.strip_prefix("/feedback ") {
            // "/feedback <skill_name> <message>" — pass "<skill_name> <message>" to split
            let r = r.trim();
            if let Some((name, feedback_rest)) = r.split_once(' ') {
                let feedback = feedback_rest.trim().trim_matches('"');
                if self.feedback.detector.detect(feedback, &[]).is_some() {
                    self.generate_improved_skill(name.trim(), feedback, "", Some(feedback))
                        .await
                        .ok();
                }
            }
            return;
        } else if let Some(r) = trimmed.strip_prefix("/skill reject ") {
            r.trim()
        } else {
            return;
        };
        // "/skill reject <name> <reason>" path
        let mut parts = rest.splitn(2, ' ');
        let Some(name) = parts.next() else { return };
        let reason = parts.next().unwrap_or("").trim();
        if !reason.is_empty() {
            self.generate_improved_skill(name, reason, "", Some(reason))
                .await
                .ok();
        }
    }

    /// Return the `/feedback` command output as a `String` without sending via channel.
    ///
    /// Used by the `AgentAccess::handle_feedback_command` implementation to satisfy the
    /// `Send` bound on the returned future.
    pub(super) async fn handle_feedback_as_string(
        &mut self,
        input: &str,
    ) -> Result<String, error::AgentError> {
        let Some((name, rest)) = input.split_once(' ') else {
            return Ok("Usage: /feedback <skill_name> <message>".to_owned());
        };
        let (skill_name, feedback) = (name.trim(), rest.trim().trim_matches('"'));

        if feedback.is_empty() {
            return Ok("Usage: /feedback <skill_name> <message>".to_owned());
        }

        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };
        let conversation_id = self.memory_state.persistence.conversation_id;

        let outcome_type = if self.feedback.detector.detect(feedback, &[]).is_some() {
            "user_rejection"
        } else {
            "user_approval"
        };

        memory
            .sqlite()
            .record_skill_outcome(
                skill_name,
                None,
                conversation_id,
                outcome_type,
                None,
                Some(feedback),
            )
            .await?;

        Ok(format!("Feedback recorded for \"{skill_name}\"."))
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_with_llm_classifier(
    llm_classifier: zeph_llm::classifier::llm::LlmClassifier,
    user_msg: String,
    assistant: String,
    confidence_threshold: f32,
    classifier_metrics_bg: Option<std::sync::Arc<zeph_llm::ClassifierMetrics>>,
    metrics_tx_bg: Option<tokio::sync::watch::Sender<crate::metrics::MetricsSnapshot>>,
    memory_arc: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
    conv_id: Option<zeph_memory::ConversationId>,
    skill_name: String,
) {
    match llm_classifier
        .classify_feedback(&user_msg, &assistant, confidence_threshold)
        .await
    {
        Ok(verdict) => {
            if let (Some(ref cm), Some(ref tx)) = (classifier_metrics_bg, metrics_tx_bg) {
                let snap = cm.snapshot();
                tx.send_modify(|ms| ms.classifier = snap);
            }
            if let Some(signal) = feedback_verdict_into_signal(&verdict, &user_msg) {
                let is_self_correction =
                    signal.kind == feedback_detector::CorrectionKind::SelfCorrection;
                tracing::info!(
                    kind = signal.kind.as_str(),
                    confidence = signal.confidence,
                    source = "llm-classifier",
                    is_self_correction,
                    "correction signal detected"
                );
                store_correction_in_memory(
                    memory_arc,
                    conv_id,
                    &assistant,
                    &user_msg,
                    skill_name,
                    signal.kind.as_str(),
                )
                .await;
            }
        }
        Err(e) => {
            tracing::warn!("llm-classifier failed: {e:#}");
        }
    }
}

async fn evaluate_with_judge(
    judge_provider: zeph_llm::any::AnyProvider,
    user_msg: String,
    assistant: String,
    confidence_threshold: f32,
    memory_arc: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
    conv_id: Option<zeph_memory::ConversationId>,
    skill_name: String,
) {
    match feedback_detector::JudgeDetector::evaluate(
        &judge_provider,
        &user_msg,
        &assistant,
        confidence_threshold,
    )
    .await
    {
        Ok(verdict) => {
            if let Some(signal) = verdict.into_signal(&user_msg) {
                let is_self_correction =
                    signal.kind == feedback_detector::CorrectionKind::SelfCorrection;
                tracing::info!(
                    kind = signal.kind.as_str(),
                    confidence = signal.confidence,
                    source = "judge",
                    is_self_correction,
                    "correction signal detected"
                );
                store_correction_in_memory(
                    memory_arc,
                    conv_id,
                    &assistant,
                    &user_msg,
                    skill_name,
                    signal.kind.as_str(),
                )
                .await;
            }
        }
        Err(e) => {
            tracing::warn!("judge detector failed: {e:#}");
        }
    }
}
