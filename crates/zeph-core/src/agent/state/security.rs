// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SecurityState` impl block: PII scrubbing and guardrail checking.
//!
//! These methods contain only security-state logic (no cross-cutting agent dependencies).
//! The Agent wrappers in `tool_execution/mod.rs` apply metrics and channel side-effects.

use std::sync::Arc;

use parking_lot::RwLock;

use super::SecurityState;

impl Default for SecurityState {
    fn default() -> Self {
        Self {
            sanitizer: zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
            quarantine_summarizer: None,
            is_acp_session: false,
            exfiltration_guard: zeph_sanitizer::exfiltration::ExfiltrationGuard::new(
                zeph_sanitizer::exfiltration::ExfiltrationGuardConfig::default(),
            ),
            flagged_urls: std::collections::HashSet::new(),
            user_provided_urls: Arc::new(RwLock::new(std::collections::HashSet::new())),
            pii_filter: zeph_sanitizer::pii::PiiFilter::new(
                zeph_sanitizer::pii::PiiFilterConfig::default(),
            ),
            #[cfg(feature = "classifiers")]
            pii_ner_backend: None,
            #[cfg(feature = "classifiers")]
            pii_ner_timeout_ms: 5000,
            #[cfg(feature = "classifiers")]
            pii_ner_max_chars: 8192,
            #[cfg(feature = "classifiers")]
            pii_ner_circuit_breaker_threshold: 2,
            #[cfg(feature = "classifiers")]
            pii_ner_consecutive_timeouts: 0,
            #[cfg(feature = "classifiers")]
            pii_ner_tripped: false,
            memory_validator: zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
                zeph_sanitizer::memory_validation::MemoryWriteValidationConfig::default(),
            ),
            guardrail: None,
            response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier::new(
                zeph_config::ResponseVerificationConfig::default(),
            ),
            causal_analyzer: None,
            vigil: None,
        }
    }
}

/// Result returned by [`SecurityState::scrub_pii`], carrying the scrubbed text and
/// side-effect descriptors that the Agent wrapper must apply to metrics.
pub(crate) struct PiiScrubResult {
    pub(crate) text: String,
    /// Whether PII was actually redacted (`scrub_count` increment + `push_classifier_metrics`).
    pub(crate) scrubbed: bool,
    /// Number of NER timeouts that occurred (metrics: `pii_ner_timeouts` increment).
    pub(crate) ner_timeouts: u32,
    /// Whether the circuit breaker tripped during this call (`pii_ner_circuit_breaker_trips` increment).
    pub(crate) circuit_breaker_tripped: bool,
}

impl SecurityState {
    /// Run regex PII filter and (optionally) NER classifier, merge spans, and redact in one pass.
    ///
    /// Returns a [`PiiScrubResult`] with the scrubbed text and metric side-effects.
    /// The caller is responsible for applying metrics updates from the result.
    #[cfg_attr(not(feature = "classifiers"), allow(clippy::unused_async))]
    pub(crate) async fn scrub_pii(&mut self, text: &str, tool_name: &str) -> PiiScrubResult {
        use zeph_sanitizer::pii::{merge_spans, redact_spans};

        if !self.pii_filter.is_enabled() {
            return PiiScrubResult {
                text: text.to_owned(),
                scrubbed: false,
                ner_timeouts: 0,
                circuit_breaker_tripped: false,
            };
        }

        #[cfg_attr(not(feature = "classifiers"), allow(unused_mut))]
        let mut spans = self.pii_filter.detect_spans(text);
        #[cfg_attr(not(feature = "classifiers"), allow(unused_mut))]
        let mut ner_timeouts: u32 = 0;
        #[cfg_attr(not(feature = "classifiers"), allow(unused_mut))]
        let mut circuit_breaker_tripped = false;

        #[cfg(feature = "classifiers")]
        self.run_ner_classifier(
            text,
            tool_name,
            &mut spans,
            &mut ner_timeouts,
            &mut circuit_breaker_tripped,
        )
        .await;

        let merged = merge_spans(spans);
        if merged.is_empty() {
            return PiiScrubResult {
                text: text.to_owned(),
                scrubbed: false,
                ner_timeouts,
                circuit_breaker_tripped,
            };
        }

        tracing::debug!(tool = %tool_name, span_count = merged.len(), "PII scrubbed from tool output");
        PiiScrubResult {
            text: redact_spans(text, &merged),
            scrubbed: true,
            ner_timeouts,
            circuit_breaker_tripped,
        }
    }

    /// Run the NER classifier backend and append any detected spans.
    ///
    /// Updates `ner_timeouts` and `circuit_breaker_tripped` accumulators in place.
    /// No-op when the circuit breaker is already tripped or no backend is configured.
    #[cfg(feature = "classifiers")]
    async fn run_ner_classifier(
        &mut self,
        text: &str,
        tool_name: &str,
        spans: &mut Vec<zeph_sanitizer::pii::PiiSpan>,
        ner_timeouts: &mut u32,
        circuit_breaker_tripped: &mut bool,
    ) {
        use zeph_sanitizer::pii::build_char_to_byte_map;

        let Some(ref backend) = self.pii_ner_backend else {
            return;
        };

        if self.pii_ner_tripped {
            tracing::debug!(tool = %tool_name, "PII NER circuit breaker open, regex only");
            return;
        }

        let timeout_ms = self.pii_ner_timeout_ms;
        let ner_input = if text.len() > self.pii_ner_max_chars {
            let boundary = text.floor_char_boundary(self.pii_ner_max_chars);
            &text[..boundary]
        } else {
            text
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            backend.classify(ner_input),
        )
        .await
        {
            Ok(Ok(result)) if result.is_positive => {
                let char_to_byte = build_char_to_byte_map(ner_input);
                for ner_span in &result.spans {
                    let byte_start = char_to_byte
                        .get(ner_span.start)
                        .copied()
                        .unwrap_or(ner_input.len());
                    let byte_end = char_to_byte
                        .get(ner_span.end)
                        .copied()
                        .unwrap_or(ner_input.len());
                    if byte_end > byte_start {
                        spans.push(zeph_sanitizer::pii::PiiSpan {
                            label: ner_span.label.clone(),
                            start: byte_start,
                            end: byte_end,
                        });
                    }
                }
                self.pii_ner_consecutive_timeouts = 0;
            }
            Ok(Ok(_)) => {
                self.pii_ner_consecutive_timeouts = 0;
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, tool = %tool_name, "PII NER failed, regex only");
            }
            Err(_) => {
                *ner_timeouts += 1;
                self.pii_ner_consecutive_timeouts += 1;
                let threshold = self.pii_ner_circuit_breaker_threshold;
                if threshold > 0 && self.pii_ner_consecutive_timeouts >= threshold {
                    self.pii_ner_tripped = true;
                    *circuit_breaker_tripped = true;
                    tracing::warn!(
                        consecutive_timeouts = self.pii_ner_consecutive_timeouts,
                        threshold = threshold,
                        tool = %tool_name,
                        "PII NER circuit breaker tripped — NER disabled for this session, falling back to regex-only PII detection"
                    );
                } else {
                    tracing::warn!(
                        timeout_ms = timeout_ms,
                        tool = %tool_name,
                        consecutive = self.pii_ner_consecutive_timeouts,
                        "PII NER timed out, regex only"
                    );
                }
            }
        }
    }

    /// Run the guardrail filter against a tool output body. Returns the body (possibly replaced).
    ///
    /// Only reads `self.guardrail` — no metric side-effects. Pure security-state logic.
    pub(crate) async fn check_guardrail(&self, mut body: String, tool_name: &str) -> String {
        use zeph_sanitizer::guardrail::GuardrailVerdict;
        let Some(ref guardrail) = self.guardrail else {
            return body;
        };
        if !guardrail.scan_tool_output() {
            return body;
        }
        let verdict = if let Ok(v) =
            tokio::time::timeout(std::time::Duration::from_secs(10), guardrail.check(&body)).await
        {
            v
        } else {
            tracing::warn!(tool = %tool_name, "tool guardrail check timed out after 10s");
            zeph_sanitizer::guardrail::GuardrailVerdict::Error {
                error: "timeout".into(),
            }
        };
        if let GuardrailVerdict::Flagged { reason, .. } = &verdict {
            tracing::warn!(
                tool = %tool_name,
                reason = %reason,
                should_block = verdict.should_block(),
                "guardrail flagged tool output"
            );
            if verdict.should_block() {
                body = format!("[guardrail blocked] Tool output flagged: {reason}");
            }
        } else if let GuardrailVerdict::Error { error } = &verdict {
            if guardrail.error_should_block() {
                tracing::warn!(
                    tool = %tool_name,
                    %error,
                    "guardrail check failed (fail_strategy=closed), blocking tool output"
                );
                "[guardrail blocked] Tool output check failed (see logs)".clone_into(&mut body);
            } else {
                tracing::warn!(
                    tool = %tool_name,
                    %error,
                    "guardrail check failed (fail_strategy=open), allowing tool output"
                );
            }
        }
        body
    }
}
