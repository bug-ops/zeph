// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent transcript view: initiating background loads, polling for completion,
//! reloading on file change, and projecting cached entries into chat messages.

use tokio::sync::oneshot;

use super::{
    AgentViewTarget, App, ChatMessage, MessageRole, TRANSCRIPT_MAX_ENTRIES, TranscriptCache,
    TuiTranscriptEntry, load_transcript_file,
};

impl App {
    /// Switch the chat view target. Clears render cache and scroll offset.
    /// All view changes MUST go through this method (W5).
    pub fn set_view_target(&mut self, target: AgentViewTarget) {
        if self.sessions.current().view_target == target {
            return;
        }
        self.sessions.current_mut().view_target = target;
        self.sessions.current_mut().render_cache.clear();
        self.sessions.current_mut().scroll_offset = 0;
        self.sessions.current_mut().transcript_cache = None;
        if self
            .sessions
            .current_mut()
            .pending_transcript
            .take()
            .is_some()
        {
            // Only clear if a load was actually in flight — otherwise this could wipe an
            // unrelated status_label. Its "loading transcript..." label would otherwise
            // never be cleared, since poll_pending_transcript (the only other clearer)
            // never runs once pending_transcript is gone (e.g. Esc back to Main before
            // the load resolves).
            self.sessions.current_mut().status_label = None;
        }
        // Kick off transcript load if switching to a subagent.
        if let AgentViewTarget::SubAgent { ref id, .. } = self.sessions.current().view_target {
            let id = id.clone();
            self.start_transcript_load(&id);
        }
    }

    /// Initiates a background transcript load for the given agent ID.
    fn start_transcript_load(&mut self, agent_id: &str) {
        // Find transcript_dir from current metrics.
        let transcript_path = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == agent_id)
            .and_then(|sa| sa.transcript_dir.as_deref())
            .map(|dir| std::path::PathBuf::from(dir).join(format!("{agent_id}.jsonl")));

        let Some(path) = transcript_path else {
            return;
        };

        let (tx, rx) = oneshot::channel();
        self.sessions.current_mut().pending_transcript = Some(rx);
        self.sessions.current_mut().status_label = Some("loading transcript...".to_owned());
        // Determine if the agent is still active (for C2: skip warning on partial last line).
        let is_active = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == agent_id)
            .is_some_and(|sa| matches!(sa.state.as_str(), "working" | "submitted"));

        tokio::task::spawn_blocking(move || {
            // EXEMPT: short one-shot load; result delivered via oneshot and polled every tick
            let result = load_transcript_file(&path, is_active);
            let _ = tx.send(result);
        });
    }

    /// Poll the pending transcript load and install result if ready.
    pub fn poll_pending_transcript(&mut self) {
        let Some(rx) = self.sessions.current_mut().pending_transcript.as_mut() else {
            return;
        };
        match rx.try_recv() {
            Ok((entries, total)) => {
                self.sessions.current_mut().pending_transcript = None;
                self.sessions.current_mut().status_label = None;
                let turns_at_load = self
                    .sessions
                    .current()
                    .view_target
                    .subagent_id()
                    .and_then(|id| self.metrics.sub_agents.iter().find(|sa| sa.id == id))
                    .map_or(0, |sa| sa.turns_used);
                if let AgentViewTarget::SubAgent { ref id, .. } =
                    self.sessions.current().view_target.clone()
                {
                    self.sessions.current_mut().transcript_cache = Some(TranscriptCache {
                        agent_id: id.clone(),
                        entries,
                        turns_at_load,
                        total_in_file: total,
                    });
                }
                self.sessions.current_mut().render_cache.clear();
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.sessions.current_mut().pending_transcript = None;
                self.sessions.current_mut().status_label = None;
            }
        }
    }

    /// Check if the transcript needs reloading (turns count increased).
    pub(super) fn maybe_reload_transcript(&mut self) {
        let AgentViewTarget::SubAgent { ref id, .. } = self.sessions.current().view_target.clone()
        else {
            return;
        };
        // Don't start a new load while one is already in flight.
        if self.sessions.current().pending_transcript.is_some() {
            return;
        }
        let current_turns = self
            .metrics
            .sub_agents
            .iter()
            .find(|sa| sa.id == *id)
            .map_or(0, |sa| sa.turns_used);
        let cached_turns = self
            .sessions
            .current()
            .transcript_cache
            .as_ref()
            .map_or(0, |c| c.turns_at_load);
        if current_turns > cached_turns {
            let agent_id = id.to_owned();
            self.start_transcript_load(&agent_id);
        }
    }

    /// Returns the messages to display in the chat area.
    ///
    /// Always returns an owned `Vec` — the cost is one clone of at most
    /// `MAX_TUI_MESSAGES` (2000) ref-counted strings inside `ChatMessage`.
    /// When viewing a subagent, returns transcript entries converted to [`ChatMessage`].
    /// When no transcript is loaded yet, returns a loading placeholder.
    #[must_use]
    pub fn visible_messages(&self) -> Vec<ChatMessage> {
        let slot = self.sessions.current();
        if slot.view_target.is_main() {
            return slot.messages.clone();
        }
        if let Some(ref cache) = slot.transcript_cache {
            return cache
                .entries
                .iter()
                .map(TuiTranscriptEntry::to_chat_message)
                .collect();
        }
        if slot.pending_transcript.is_some() {
            return vec![ChatMessage::new(
                MessageRole::System,
                "Loading transcript...".to_owned(),
            )];
        }
        let name = slot.view_target.subagent_name().unwrap_or("unknown");
        vec![ChatMessage::new(
            MessageRole::System,
            format!("Transcript not available for {name}."),
        )]
    }

    /// Returns the truncation info string if the transcript was truncated.
    #[must_use]
    pub fn transcript_truncation_info(&self) -> Option<String> {
        let cache = self.sessions.current().transcript_cache.as_ref()?;
        if cache.total_in_file > TRANSCRIPT_MAX_ENTRIES {
            Some(format!(
                "[showing last {TRANSCRIPT_MAX_ENTRIES} of {} messages]",
                cache.total_in_file
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, oneshot};
    use zeph_core::metrics::SubAgentMetrics;

    use super::*;

    fn make_app() -> App {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    fn sub_agent(id: &str, transcript_dir: Option<&str>) -> SubAgentMetrics {
        SubAgentMetrics {
            id: id.to_owned(),
            name: "test-agent".to_owned(),
            state: "completed".to_owned(),
            transcript_dir: transcript_dir.map(str::to_owned),
            ..Default::default()
        }
    }

    // ── #5984 status_label lifecycle around transcript load ────────────────────

    #[tokio::test]
    async fn start_transcript_load_sets_status_label_before_dispatch() {
        // start_transcript_load offloads to spawn_blocking, which requires a Tokio runtime.
        let mut app = make_app();
        app.metrics.sub_agents = vec![sub_agent("sa-1", Some("/tmp/zeph-test-nonexistent-dir"))];
        app.start_transcript_load("sa-1");
        assert_eq!(
            app.status_label(),
            Some("loading transcript..."),
            "status_label must be set synchronously before the spawn_blocking dispatch"
        );
        assert!(app.sessions.current().pending_transcript.is_some());
    }

    #[test]
    fn start_transcript_load_noop_when_no_transcript_dir() {
        // No transcript_dir → transcript_path is None → early return, no dispatch at all.
        let mut app = make_app();
        app.metrics.sub_agents = vec![sub_agent("sa-1", None)];
        app.start_transcript_load("sa-1");
        assert_eq!(app.status_label(), None);
        assert!(app.sessions.current().pending_transcript.is_none());
    }

    #[test]
    fn poll_pending_transcript_clears_status_label_on_success() {
        let mut app = make_app();
        app.sessions.current_mut().status_label = Some("loading transcript...".to_owned());
        app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
            id: "sa-1".to_owned(),
            name: "Planner".to_owned(),
        };
        let (tx, rx) = oneshot::channel();
        app.sessions.current_mut().pending_transcript = Some(rx);
        tx.send((Vec::new(), 0)).expect("receiver still open");

        app.poll_pending_transcript();

        assert_eq!(app.status_label(), None);
        assert!(app.sessions.current().pending_transcript.is_none());
    }

    #[test]
    fn poll_pending_transcript_clears_status_label_when_task_panics() {
        // Closed branch: the spawn_blocking task dropped its sender without sending
        // (e.g. panicked) — status_label must not be left stuck on "loading transcript...".
        let mut app = make_app();
        app.sessions.current_mut().status_label = Some("loading transcript...".to_owned());
        let (tx, rx) = oneshot::channel::<(Vec<TuiTranscriptEntry>, usize)>();
        app.sessions.current_mut().pending_transcript = Some(rx);
        drop(tx);

        app.poll_pending_transcript();

        assert_eq!(app.status_label(), None);
        assert!(app.sessions.current().pending_transcript.is_none());
    }

    #[test]
    fn poll_pending_transcript_is_noop_while_still_pending() {
        let mut app = make_app();
        app.sessions.current_mut().status_label = Some("loading transcript...".to_owned());
        let (_tx, rx) = oneshot::channel();
        app.sessions.current_mut().pending_transcript = Some(rx);

        app.poll_pending_transcript();

        // Not ready yet (TryRecvError::Empty) — status_label must remain set.
        assert_eq!(app.status_label(), Some("loading transcript..."));
        assert!(app.sessions.current().pending_transcript.is_some());
    }

    // ── #5984 set_view_target cancellation must not strand status_label ────────

    #[test]
    fn set_view_target_cancels_pending_load_and_clears_status_label() {
        // Reproduces the stuck-spinner bug: a transcript load is in flight
        // (status_label = "loading transcript..."), then the user navigates away
        // (e.g. Esc back to Main) before it resolves. The switch cancels
        // pending_transcript, but poll_pending_transcript (the only other clearer)
        // will now never run again — set_view_target itself must clear the label.
        let mut app = make_app();
        app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
            id: "sa-1".to_owned(),
            name: "Planner".to_owned(),
        };
        let (_tx, rx) = oneshot::channel();
        app.sessions.current_mut().pending_transcript = Some(rx);
        app.sessions.current_mut().status_label = Some("loading transcript...".to_owned());

        app.set_view_target(AgentViewTarget::Main);

        assert!(
            app.sessions.current().pending_transcript.is_none(),
            "pending load must be cancelled"
        );
        assert_eq!(
            app.status_label(),
            None,
            "cancelling the in-flight transcript load must clear its status_label"
        );
    }

    #[test]
    fn set_view_target_preserves_unrelated_status_label_when_nothing_pending() {
        // Negative case for the same fix: switching view targets with no pending_transcript
        // in flight (e.g. no transcript_dir on record for the target agent, so
        // start_transcript_load returns early) must not wipe an unrelated status_label.
        let mut app = make_app();
        assert!(app.metrics.sub_agents.is_empty());
        app.sessions.current_mut().status_label = Some("indexing files...".to_owned());

        app.set_view_target(AgentViewTarget::SubAgent {
            id: "sa-1".to_owned(),
            name: "Planner".to_owned(),
        });

        assert!(app.sessions.current().pending_transcript.is_none());
        assert_eq!(
            app.status_label(),
            Some("indexing files..."),
            "unrelated status_label must not be wiped when there was no in-flight \
             transcript load to cancel"
        );
    }
}
