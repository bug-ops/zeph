// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Turn-level self-check hook for the MARCH quality pipeline.

use std::sync::Arc;
use std::time::Duration;

use zeph_llm::provider::Role;

use super::{Agent, Channel};
use crate::quality::SelfCheckPipeline;

impl<C: Channel> Agent<C> {
    /// Run the self-check pipeline for the current turn after `process_response()` completes.
    ///
    /// Finds the last assistant message, extracts retrieved context, and runs the pipeline
    /// synchronously within the configured latency budget.
    pub(super) async fn run_self_check_for_turn(
        &mut self,
        pipeline: Arc<SelfCheckPipeline>,
        turn_id: u64,
    ) {
        let cfg = pipeline.cfg_ref().clone();

        if cfg.async_run {
            tracing::warn!(
                turn_id,
                "self-check: async_run = true is not yet implemented; running synchronously"
            );
        }

        let Some(response_text) = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.content.clone())
            .filter(|t| !t.trim().is_empty())
        else {
            tracing::debug!(turn_id, "self-check skipped: no assistant text in turn");
            return;
        };

        let response_char_count = response_text.chars().count();
        if response_char_count > cfg.max_response_chars {
            tracing::debug!(
                turn_id,
                chars = response_char_count,
                limit = cfg.max_response_chars,
                "self-check skipped: response too long"
            );
            return;
        }

        let rc = crate::agent::context::retrieved::collect_retrieved_context(&self.msg.messages);

        let user_query = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        tracing::debug!(turn_id, "self-check: running pipeline");

        let budget = Duration::from_millis(cfg.latency_budget_ms);
        let Ok(report) = tokio::time::timeout(
            budget,
            pipeline.run(&response_text, rc, &user_query, turn_id),
        )
        .await
        else {
            tracing::warn!(
                turn_id,
                budget_ms = cfg.latency_budget_ms,
                "self-check timed out at outer budget"
            );
            let _ = self
                .channel
                .send_chunk(" [self-check: skipped (timeout)]")
                .await;
            return;
        };

        let flagged = report.flagged_ids.len();
        let total = report.assertions.len();
        tracing::debug!(
            turn_id,
            flagged,
            total,
            latency_ms = report.latency_ms,
            "self-check complete"
        );

        if flagged > 0 {
            let marker = format!(" {}", cfg.flag_marker);
            let _ = self.channel.send_chunk(&marker).await;
        }
    }
}
