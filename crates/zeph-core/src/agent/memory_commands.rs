// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use zeph_memory::MessageId;

use super::{Agent, error::AgentError};
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Dispatch `/memory [subcommand]` slash command.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel send fails or database query fails.
    pub async fn handle_memory_command(&mut self, input: &str) -> Result<(), AgentError> {
        let args = input.strip_prefix("/memory").unwrap_or("").trim();

        if args.is_empty() || args == "tiers" {
            return self.handle_memory_tiers().await;
        }

        if args.starts_with("promote") {
            let rest = args.strip_prefix("promote").unwrap_or("").trim();
            return self.handle_memory_promote(rest).await;
        }

        self.channel
            .send("Unknown /memory subcommand. Available: /memory tiers, /memory promote <id>...")
            .await?;
        Ok(())
    }

    async fn handle_memory_tiers(&mut self) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.clone() else {
            self.channel.send("Memory not configured.").await?;
            return Ok(());
        };

        match memory.sqlite().count_messages_by_tier().await {
            Ok((episodic, semantic)) => {
                let mut out = String::new();
                let _ = writeln!(out, "Memory tiers:");
                let _ = writeln!(out, "  Working:  (current context window — virtual)");
                let _ = writeln!(out, "  Episodic: {episodic} messages");
                let _ = writeln!(out, "  Semantic: {semantic} facts");
                self.channel.send(out.trim_end()).await?;
            }
            Err(e) => {
                let msg = format!("Failed to query tier stats: {e}");
                self.channel.send(&msg).await?;
            }
        }

        Ok(())
    }

    async fn handle_memory_promote(&mut self, args: &str) -> Result<(), AgentError> {
        let Some(memory) = self.memory_state.memory.clone() else {
            self.channel.send("Memory not configured.").await?;
            return Ok(());
        };

        let ids: Vec<MessageId> = args
            .split_whitespace()
            .filter_map(|s| s.parse::<i64>().ok().map(MessageId))
            .collect();

        if ids.is_empty() {
            self.channel
                .send("Usage: /memory promote <id> [id...]\nExample: /memory promote 42 43 44")
                .await?;
            return Ok(());
        }

        match memory.sqlite().manual_promote(&ids).await {
            Ok(count) => {
                let msg = format!("Promoted {count} message(s) to semantic tier.");
                self.channel.send(&msg).await?;
            }
            Err(e) => {
                let msg = format!("Promotion failed: {e}");
                self.channel.send(&msg).await?;
            }
        }

        Ok(())
    }
}
