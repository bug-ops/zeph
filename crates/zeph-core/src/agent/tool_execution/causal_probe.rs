// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Causal IPI pre/post probing (behavioral-deviation detection at the tool-return boundary).
//!
//! Split out of `tier_loop.rs` — see that module for the orchestration entry point that calls
//! into these probes.

use zeph_llm::provider::MessagePart;

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(name = "core.tool.run_causal_pre_probe", skip_all, level = "debug")]
    pub(super) async fn run_causal_pre_probe(&mut self) -> Option<(String, String)> {
        let analyzer = self.services.security.causal_analyzer.as_ref()?;
        let context_summary = self.build_causal_context_summary();
        match analyzer.probe(&context_summary).await {
            Ok(resp) => Some((resp, context_summary)),
            Err(e) => {
                tracing::warn!(error = %e, "causal IPI pre-probe failed, skipping analysis");
                None
            }
        }
    }

    #[tracing::instrument(
        name = "core.tool.run_causal_ipi_post_probe",
        skip_all,
        level = "debug"
    )]
    pub(super) async fn run_causal_ipi_post_probe(
        &mut self,
        causal_pre_response: Option<(String, String)>,
        result_parts: &[MessagePart],
    ) {
        let Some((pre_response, context_summary)) = causal_pre_response else {
            return;
        };
        let snippets: Vec<String> = result_parts
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolResult {
                    content, is_error, ..
                } = p
                {
                    if *is_error {
                        Some(zeph_sanitizer::causal_ipi::format_error_snippet(content))
                    } else {
                        Some(zeph_sanitizer::causal_ipi::format_tool_snippet(content))
                    }
                } else {
                    None
                }
            })
            .collect();
        let tool_snippets = if snippets.is_empty() {
            "[empty]".to_owned()
        } else {
            snippets.join("---")
        };
        let Some(ref analyzer) = self.services.security.causal_analyzer else {
            return;
        };
        match analyzer.post_probe(&context_summary, &tool_snippets).await {
            Ok(post_response) => {
                let analysis = analyzer.analyze(&pre_response, &post_response);
                if analysis.is_flagged {
                    let pre_excerpt = &pre_response[..pre_response.floor_char_boundary(100)];
                    let post_excerpt = &post_response[..post_response.floor_char_boundary(100)];
                    tracing::warn!(
                        deviation_score = analysis.deviation_score,
                        threshold = analyzer.threshold(),
                        pre = %pre_excerpt,
                        post = %post_excerpt,
                        "causal IPI: behavioral deviation detected at tool-return boundary"
                    );
                    self.update_metrics(|m| m.causal_ipi_flags += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::CausalIpiFlag,
                        "tool_batch",
                        format!("deviation={:.3}", analysis.deviation_score),
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "causal IPI post-probe failed, skipping analysis");
            }
        }
    }
}
