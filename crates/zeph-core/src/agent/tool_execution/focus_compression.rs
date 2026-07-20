// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Focus/compress internal-tool preprocessing and Acon tool-result compression.
//!
//! Covers the `start_focus`/`complete_focus`/`compress_context`/`request_compaction`
//! internal-tool preprocessing pass (`preprocess_focus_compress_calls`) and the Acon
//! tool-result compression pass applied to result parts before they enter message history
//! (`apply_acon_compression`). Split out of `tier_loop.rs` — see that module for the
//! orchestration entry point that calls into these passes.

use zeph_llm::provider::MessagePart;

use super::tier_loop::skipped_output;
use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(
        name = "core.tool.preprocess_focus_compress",
        skip_all,
        level = "debug"
    )]
    pub(super) async fn preprocess_focus_compress_calls(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
    ) -> Option<zeph_llm::provider::Message> {
        let mut pending_focus_checkpoint: Option<zeph_llm::provider::Message> = None;
        for (idx, tc) in tool_calls.iter().enumerate() {
            let is_focus_tool = self.services.focus.config.enabled
                && (tc.name == "start_focus" || tc.name == "complete_focus");
            let is_compress = tc.name == "compress_context";
            let is_request_compaction = tc.name == "request_compaction"
                && self
                    .services
                    .memory
                    .subsystems
                    .arc_config
                    .allow_agent_compaction;
            if is_focus_tool || is_compress || is_request_compaction {
                let result = if is_compress {
                    self.handle_compress_context().await
                } else if is_request_compaction {
                    self.handle_request_compaction(&tc.input).await
                } else {
                    let (text, maybe_checkpoint) =
                        self.handle_focus_tool(tc.name.as_str(), &tc.input);
                    if let Some(cp) = maybe_checkpoint {
                        pending_focus_checkpoint = Some(cp);
                    }
                    text
                };
                tool_results[idx] = Ok(Some(skipped_output(tc.name.clone(), result)));
            }
        }
        pending_focus_checkpoint
    }

    /// Apply Acon tool-result compression to `result_parts` in-place before the parts enter
    /// message history. No-op when `acon_config.enabled` is false or the batch is empty.
    #[tracing::instrument(name = "context.tool_result_compress", skip_all, level = "debug")]
    pub(super) fn apply_acon_compression(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        result_parts: &mut [MessagePart],
    ) {
        use zeph_context::tool_result_compress::{
            CompressionMethod, ToolResultCompressionConfig, ToolResultCompressor, ToolResultEntry,
        };

        let acon = &self.services.memory.subsystems.acon_config;
        if !acon.enabled {
            return;
        }

        let cfg = ToolResultCompressionConfig::from(acon);
        let tc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);

        // Build a lookup from tool_use_id → tool_name so we can populate the trace field
        // without relying on positional correspondence between result_parts and tool_calls.
        // This is robust to future changes where process_one_tool_result emits zero or
        // multiple ToolResult parts per call.
        let id_to_name: std::collections::HashMap<&str, &str> = tool_calls
            .iter()
            .map(|tc| (tc.id.as_str(), tc.name.as_str()))
            .collect();

        // Collect (part_index, tool_name, owned_text) for each ToolResult part. Text is cloned
        // to avoid borrow conflicts when we later mutate result_parts.
        let indexed_texts: Vec<(usize, String, String)> = result_parts
            .iter()
            .enumerate()
            .filter_map(|(i, part)| {
                if let MessagePart::ToolResult {
                    content,
                    tool_use_id,
                    ..
                } = part
                {
                    let name = id_to_name
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("")
                        .to_owned();
                    Some((i, name, content.clone()))
                } else {
                    None
                }
            })
            .collect();

        if indexed_texts.is_empty() {
            return;
        }

        let entries: Vec<ToolResultEntry<'_>> = indexed_texts
            .iter()
            .map(|(part_idx, name, text)| ToolResultEntry {
                tool_name: name.as_str(),
                text: text.as_str(),
                index: *part_idx,
            })
            .collect();

        let compressed = ToolResultCompressor::compress_batch(&entries, tc.as_ref(), &cfg);

        let mut tokens_saved: usize = 0;
        let mut results_compressed: u32 = 0;

        for (result, (part_idx, _, _)) in compressed.iter().zip(indexed_texts.iter()) {
            if result.method != CompressionMethod::PassThrough
                && let MessagePart::ToolResult { content, .. } = &mut result_parts[*part_idx]
            {
                content.clone_from(&result.text);
                tokens_saved = tokens_saved.saturating_add(
                    result
                        .original_tokens
                        .saturating_sub(result.compressed_tokens),
                );
                results_compressed += 1;
            }
        }

        if results_compressed > 0 {
            tracing::debug!(
                tokens_saved,
                results_compressed,
                "acon: tool result compression applied"
            );
            self.update_metrics(|m| {
                m.acon_tokens_saved = m
                    .acon_tokens_saved
                    .saturating_add(u64::try_from(tokens_saved).unwrap_or(u64::MAX));
                m.acon_results_compressed = m
                    .acon_results_compressed
                    .saturating_add(u64::from(results_compressed));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_req(name: &str) -> zeph_llm::provider::ToolUseRequest {
        zeph_llm::provider::ToolUseRequest {
            id: format!("id_{name}"),
            name: name.into(),
            input: serde_json::Value::Null,
        }
    }

    // Gap 5: apply_acon_compression must be a no-op when acon_config.enabled = false.
    #[test]
    fn apply_acon_compression_noop_when_disabled() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_llm::provider::MessagePart;
        use zeph_skills::registry::SkillRegistry;

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![] as Vec<String>),
            SkillRegistry::empty(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable Acon.
        agent.services.memory.subsystems.acon_config.enabled = false;

        // Build a result part with text that would be truncated if Acon were active.
        let big_content = "word ".repeat(5000);
        let mut parts = vec![MessagePart::ToolResult {
            tool_use_id: "id_shell".to_owned(),
            content: big_content.clone(),
            is_error: false,
        }];
        let calls = vec![make_tool_req("shell")];

        agent.apply_acon_compression(&calls, &mut parts);

        // Content must be unchanged.
        if let MessagePart::ToolResult { content, .. } = &parts[0] {
            assert_eq!(
                content.len(),
                big_content.len(),
                "content must not be modified when acon is disabled"
            );
        } else {
            panic!("expected ToolResult part");
        }
    }

    // spec-072 C5/AC-15 (T-213): pre-assembly pass safety with an interleaved Image sibling.
    // `run_causal_ipi_post_probe` and `record_shadow_event` take `result_parts`/`tool_calls`
    // by shared reference and never touch `MessagePart::Image` at all, so they cannot
    // mutate/drop it by construction. `apply_acon_compression` is the only pass that mutates
    // `result_parts` in place — this test proves its `tool_use_id`-based `ToolResult`
    // targeting is unaffected by the presence/position of a non-`ToolResult` sibling, and
    // that the `Image` part itself survives all three passes byte-for-byte.
    #[test]
    #[allow(clippy::too_many_lines)] // control + interleaved runs, both passes asserted
    fn pre_assembly_passes_preserve_image_sibling() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_llm::provider::{ImageData, ToolUseRequest};
        use zeph_skills::registry::SkillRegistry;

        fn make_agent() -> Agent<MockChannel> {
            let mut agent = Agent::new(
                mock_provider(vec![]),
                MockChannel::new(vec![] as Vec<String>),
                SkillRegistry::empty(),
                None,
                5,
                MockToolExecutor::no_tools(),
            );
            // Default passthrough_threshold (2000 tokens) is well below the ~9000-token
            // bodies below, so compression actually runs (not a PassThrough no-op).
            agent.services.memory.subsystems.acon_config.enabled = true;
            agent
        }

        fn tool_result(id: &str, content: String) -> MessagePart {
            MessagePart::ToolResult {
                tool_use_id: id.to_owned(),
                content,
                is_error: false,
            }
        }

        let big_a = "alpha ".repeat(3000);
        let big_b = "bravo ".repeat(3000);
        let calls = vec![
            ToolUseRequest {
                id: "id_a".to_owned(),
                name: "read".into(),
                input: serde_json::Value::Null,
            },
            ToolUseRequest {
                id: "id_b".to_owned(),
                name: "read".into(),
                input: serde_json::Value::Null,
            },
        ];
        let image_bytes = vec![1u8, 2, 3, 4, 5];
        let image_mime = "image/png".to_owned();
        let image = MessagePart::Image(Box::new(ImageData {
            data: image_bytes.clone(),
            mime_type: image_mime.clone(),
        }));

        // Control run: no Image sibling at all.
        let big_a_original_len = big_a.len();
        let mut control_parts = vec![
            tool_result("id_a", big_a.clone()),
            tool_result("id_b", big_b.clone()),
        ];
        let mut control_agent = make_agent();
        control_agent.apply_acon_compression(&calls, &mut control_parts);

        // Interleaved run: Image positioned between the two ToolResult parts.
        let mut interleaved_parts = vec![
            tool_result("id_a", big_a),
            image,
            tool_result("id_b", big_b),
        ];
        let mut agent = make_agent();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            agent
                .run_causal_ipi_post_probe(None, &interleaved_parts)
                .await;
        });
        agent.record_shadow_event(&calls, "goal summary".into());
        agent.apply_acon_compression(&calls, &mut interleaved_parts);

        // (a) Compression output for both ToolResult parts is unaffected by the interleaved
        // Image sibling: identical to the control run without it.
        let MessagePart::ToolResult {
            content: control_a, ..
        } = &control_parts[0]
        else {
            panic!("expected ToolResult part in control run");
        };
        let MessagePart::ToolResult {
            content: control_b, ..
        } = &control_parts[1]
        else {
            panic!("expected ToolResult part in control run");
        };
        let MessagePart::ToolResult {
            content: interleaved_a,
            ..
        } = &interleaved_parts[0]
        else {
            panic!("expected ToolResult part at index 0");
        };
        let MessagePart::ToolResult {
            content: interleaved_b,
            ..
        } = &interleaved_parts[2]
        else {
            panic!("expected ToolResult part at index 2");
        };
        assert!(
            control_a.len() < big_a_original_len,
            "sanity: compression must actually run (control_a shorter than original)"
        );
        assert_eq!(
            control_a, interleaved_a,
            "id_a compression must be identical with/without the interleaved Image sibling"
        );
        assert_eq!(
            control_b, interleaved_b,
            "id_b compression must be identical with/without the interleaved Image sibling"
        );

        // (b) The Image part itself survives all three passes byte-for-byte.
        match &interleaved_parts[1] {
            MessagePart::Image(img) => {
                assert_eq!(img.data, image_bytes, "Image bytes must be unchanged");
                assert_eq!(
                    img.mime_type, image_mime,
                    "Image mime_type must be unchanged"
                );
            }
            other => panic!("expected Image part at index 1, got {other:?}"),
        }
    }
}
