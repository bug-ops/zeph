// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for the `/guidelines` slash command.

use crate::channel::Channel;

use super::Agent;
use super::error::AgentError;

/// Maximum characters of guidelines text shown in TUI chat to avoid flooding.
const MAX_DISPLAY_CHARS: usize = 4096;

impl<C: Channel> Agent<C> {
    /// Display the current compression guidelines or a "no guidelines" notice.
    ///
    /// # Errors
    ///
    /// Returns an error if sending the message to the channel fails.
    pub(super) async fn handle_guidelines_command(&mut self) -> Result<(), AgentError> {
        let Some(memory) = &self.memory_state.memory else {
            return self
                .channel
                .send("No memory backend initialised.")
                .await
                .map_err(AgentError::from);
        };

        let cid = self.memory_state.conversation_id;
        let sqlite = memory.sqlite();

        let (version, text) = sqlite.load_compression_guidelines(cid).await.map_err(
            |e: zeph_memory::MemoryError| {
                tracing::warn!("failed to load compression guidelines: {e:#}");
                AgentError::from(e)
            },
        )?;

        if version == 0 || text.is_empty() {
            return self
                .channel
                .send("No compression guidelines generated yet.")
                .await
                .map_err(AgentError::from);
        }

        let (_, created_at) = sqlite
            .load_compression_guidelines_meta(cid)
            .await
            .unwrap_or((0, String::new()));

        let (body, truncated) = if text.len() > MAX_DISPLAY_CHARS {
            let end = text.floor_char_boundary(MAX_DISPLAY_CHARS);
            (&text[..end], true)
        } else {
            (text.as_str(), false)
        };

        let mut output =
            format!("Compression Guidelines (v{version}, updated {created_at}):\n\n{body}");
        if truncated {
            output.push_str("\n\n[truncated]");
        }

        self.channel.send(&output).await.map_err(AgentError::from)
    }
}
