// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Credential scrub of the unsent tail in the `append` layout (spec 088).
//!
//! A sent message is never rewritten, so each message is scrubbed exactly once, right before the
//! first request that carries it. Progress is an index plus a fingerprint of the last scrubbed
//! message; if that message is no longer at the index (trim, compaction, pop) the whole window is
//! scrubbed again, which is idempotent.

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};

use zeph_llm::provider::{Message, MessagePart, Role};

use crate::agent::Agent;
use crate::channel::Channel;

/// Position and fingerprint of the last message that was scrubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrubProgress {
    index: usize,
    fingerprint: u64,
}

/// Fingerprint over everything a scrub can change: role, content and the text-bearing parts.
fn fingerprint(msg: &Message) -> u64 {
    let mut hasher = DefaultHasher::new();
    std::mem::discriminant(&msg.role).hash(&mut hasher);
    msg.content.hash(&mut hasher);
    msg.parts.len().hash(&mut hasher);
    for part in &msg.parts {
        match part {
            MessagePart::Text { text } => text.hash(&mut hasher),
            MessagePart::ToolResult { content, .. } => content.hash(&mut hasher),
            MessagePart::ToolOutput { body, .. } => body.hash(&mut hasher),
            _ => {}
        }
    }
    hasher.finish()
}

/// Replace `text` with the scrubbed form when `scrub` changed it.
fn scrub_in_place(text: &mut String, scrub: for<'a> fn(&'a str) -> Cow<'a, str>) -> bool {
    let owned = match scrub(text) {
        Cow::Owned(s) => Some(s),
        Cow::Borrowed(_) => None,
    };
    owned.is_some_and(|s| {
        *text = s;
        true
    })
}

/// Scrub one message in place and report whether anything changed.
///
/// Wire builders render from `parts` when present, so the part texts are scrubbed and `content`
/// is rebuilt from them. User text gets the full scrub (secrets, URL credentials, paths) like the
/// `replace` layout; tool results only lose secrets, because a path in a tool result is data the
/// model needs.
fn scrub_message(msg: &mut Message) -> bool {
    if msg.parts.is_empty() {
        return scrub_in_place(&mut msg.content, crate::redact::scrub_content);
    }
    let mut changed = false;
    for part in &mut msg.parts {
        changed |= match part {
            MessagePart::Text { text } => scrub_in_place(text, crate::redact::scrub_content),
            MessagePart::ToolResult { content, .. } => {
                scrub_in_place(content, crate::redact::redact_secrets)
            }
            MessagePart::ToolOutput { body, .. } => {
                scrub_in_place(body, crate::redact::redact_secrets)
            }
            _ => false,
        };
    }
    if changed {
        msg.rebuild_content();
    }
    changed
}

impl<C: Channel> Agent<C> {
    /// Credential-scrub the messages added since the last request, in place.
    #[tracing::instrument(
        name = "core.context.scrub_tail",
        skip_all,
        level = "debug",
        fields(start = tracing::field::Empty, scanned = tracing::field::Empty)
    )]
    pub(super) fn scrub_unsent_tail(&mut self) {
        if !self.runtime.config.redact_credentials {
            self.msg.layout.scrub_progress = None;
            return;
        }
        let verified = self.msg.layout.scrub_progress.filter(|p| {
            self.msg
                .messages
                .get(p.index)
                .is_some_and(|m| fingerprint(m) == p.fingerprint)
        });
        let start = verified.map_or(1, |p| p.index + 1);
        let mut scanned = 0usize;
        for msg in self.msg.messages.iter_mut().skip(start) {
            if msg.role == Role::System {
                continue;
            }
            scanned += 1;
            scrub_message(msg);
        }
        let span = tracing::Span::current();
        span.record("start", start);
        span.record("scanned", scanned);
        self.msg.layout.scrub_progress = self.msg.messages.len().checked_sub(1).and_then(|index| {
            self.msg.messages.get(index).map(|m| ScrubProgress {
                index,
                fingerprint: fingerprint(m),
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use super::*;
    use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};

    fn agent() -> Agent<MockChannel> {
        let mut agent = Agent::new(
            AnyProvider::Mock(MockProvider::with_responses(vec![])),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.runtime.config.redact_credentials = true;
        agent
    }

    fn user_with_text_part(text: &str) -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::Text {
                text: text.to_owned(),
            }],
        )
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: id.to_owned(),
                content: content.to_owned(),
                is_error: false,
            }],
        )
    }

    fn assistant(text: &str) -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::Text {
                text: text.to_owned(),
            }],
        )
    }

    fn part_texts(msg: &Message) -> Vec<&str> {
        msg.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text } => Some(text.as_str()),
                MessagePart::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_part_secrets_are_scrubbed_and_content_follows() {
        let mut agent = agent();
        agent
            .msg
            .messages
            .push(user_with_text_part("use key sk-abc123def456 now"));
        agent.scrub_unsent_tail();
        let msg = &agent.msg.messages[1];
        assert!(!part_texts(msg)[0].contains("sk-abc123def456"));
        assert!(
            !msg.content.contains("sk-abc123def456"),
            "content is rebuilt"
        );
    }

    #[test]
    fn tool_result_secrets_are_scrubbed_but_paths_survive() {
        let mut agent = agent();
        agent.msg.messages.push(tool_result(
            "t1",
            "read /home/dev/app/.env: key sk_live_abcdef123456",
        ));
        agent.scrub_unsent_tail();
        let text = part_texts(&agent.msg.messages[1])[0].to_owned();
        assert!(!text.contains("sk_live_"));
        assert!(
            text.contains("/home/dev/app/.env"),
            "tool paths are data: {text}"
        );
    }

    #[test]
    fn messages_after_a_tool_iteration_are_not_skipped() {
        let mut agent = agent();
        agent.msg.messages.push(user_with_text_part("start"));
        agent.msg.messages.push(assistant("calling tool"));
        agent.msg.messages.push(tool_result("t1", "ok"));
        agent.scrub_unsent_tail();

        // Tool results carry empty `content`, so a content-only marker would match the next
        // tool result and skip the assistant message in between.
        agent
            .msg
            .messages
            .push(assistant("here is sk-abc123def456 again"));
        agent.msg.messages.push(tool_result("t2", "ok"));
        agent
            .msg
            .messages
            .push(assistant("and sk_live_abcdef123456"));
        agent.scrub_unsent_tail();

        for msg in &agent.msg.messages[1..] {
            for text in part_texts(msg) {
                assert!(
                    !text.contains("sk-abc123def456") && !text.contains("sk_live_"),
                    "unscrubbed text survived: {text}"
                );
            }
        }
    }

    #[test]
    fn sent_messages_are_not_rewritten_again() {
        let mut agent = agent();
        agent
            .msg
            .messages
            .push(user_with_text_part("hello /Users/dev/x"));
        agent.scrub_unsent_tail();
        let first = part_texts(&agent.msg.messages[1])[0].to_owned();
        agent.msg.messages.push(assistant("hi"));
        agent.scrub_unsent_tail();
        assert_eq!(part_texts(&agent.msg.messages[1])[0], first);
        assert_eq!(
            agent.msg.layout.scrub_progress.map(|p| p.index),
            Some(2),
            "progress tracks the last message"
        );
    }

    #[test]
    fn a_vanished_progress_message_triggers_a_full_pass() {
        let mut agent = agent();
        agent.msg.messages.push(user_with_text_part("one"));
        agent.msg.messages.push(assistant("two"));
        agent.scrub_unsent_tail();
        // History shrank (trim): the recorded index no longer holds the same message.
        agent.msg.messages.remove(1);
        agent
            .msg
            .messages
            .push(assistant("late secret sk-abc123def456"));
        agent.scrub_unsent_tail();
        assert!(!part_texts(&agent.msg.messages[2])[0].contains("sk-abc123def456"));
    }

    #[test]
    fn disabled_redaction_leaves_messages_alone() {
        let mut agent = agent();
        agent.runtime.config.redact_credentials = false;
        agent
            .msg
            .messages
            .push(user_with_text_part("key sk-abc123def456"));
        agent.scrub_unsent_tail();
        assert!(part_texts(&agent.msg.messages[1])[0].contains("sk-abc123def456"));
        assert!(agent.msg.layout.scrub_progress.is_none());
    }
}
