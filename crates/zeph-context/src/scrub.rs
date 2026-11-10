// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Part-aware credential scrub of a [`Message`], shared by the `replace` and `append` context
//! layouts (spec 088 section 7).
//!
//! Wire builders render a message from `parts` when it has any, never from `content`, so a scrub
//! that rewrites only `content` leaves secrets in tool results on their way to the provider.
//! [`scrub_message`] scrubs the text-bearing parts and rebuilds `content` from them.
//!
//! Not scrubbed, by design: `ToolUse.input` is built from already-scrubbed context and rewriting
//! it would break argument fidelity, and `RuntimeNote` is agent-authored from already-scrubbed
//! context.

use std::borrow::Cow;

use zeph_common::secret_redact::{scrub_content, scrub_secrets};
use zeph_llm::provider::{Message, MessagePart};

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
/// User and assistant text gets the full scrub (secrets, URL credentials, paths); tool results
/// and tool outputs lose secrets and URL credentials only, because a path in a tool result is
/// data the model needs. After a change to a structured message `content` is rebuilt from its
/// parts.
///
/// # Examples
///
/// ```
/// use zeph_context::scrub::scrub_message;
/// use zeph_llm::provider::{Message, MessagePart, Role};
///
/// let mut msg = Message::from_parts(
///     Role::User,
///     vec![MessagePart::ToolResult {
///         tool_use_id: "t1".into(),
///         content: "OPENAI_API_KEY=sk-proj-abc123def456".into(),
///         is_error: false,
///     }],
/// );
/// assert!(scrub_message(&mut msg));
/// assert!(!msg.content.contains("sk-proj-abc123def456"));
/// ```
#[tracing::instrument(name = "context.scrub.message", skip_all, level = "trace")]
pub fn scrub_message(msg: &mut Message) -> bool {
    if msg.parts.is_empty() {
        return scrub_in_place(&mut msg.content, scrub_content);
    }
    let mut changed = false;
    for part in &mut msg.parts {
        changed |= match part {
            MessagePart::Text { text } => scrub_in_place(text, scrub_content),
            MessagePart::ToolResult { content, .. } => scrub_in_place(content, scrub_secrets),
            MessagePart::ToolOutput { body, .. } => scrub_in_place(body, scrub_secrets),
            _ => false,
        };
    }
    if changed {
        msg.rebuild_content();
    }
    changed
}

#[cfg(test)]
mod tests {
    use zeph_llm::LlmProvider;
    use zeph_llm::claude::ClaudeProvider;
    use zeph_llm::provider::{Role, ToolDefinition};
    use zeph_llm::{OpenAiConfig, openai::OpenAiProvider};

    use super::*;

    const OPENAI_KEY: &str = "sk-proj-abc123def456ghi789";
    const DB_URL: &str = "postgres://admin:hunter2@db.internal/app";

    fn tool_result(content: &str) -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: content.into(),
                is_error: false,
            }],
        )
    }

    fn history(content: &str, scrub: bool) -> Vec<Message> {
        let mut assistant = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "cat .env"}),
            }],
        );
        assistant.rebuild_content();
        let mut result = tool_result(content);
        if scrub {
            scrub_message(&mut result);
        }
        vec![
            Message::from_legacy(Role::User, "check the app"),
            assistant,
            result,
        ]
    }

    fn tools() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "shell".into(),
            description: "run a command".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: None,
        }]
    }

    fn assert_clean(json: &serde_json::Value) {
        let wire = json.to_string();
        assert!(
            !wire.contains(OPENAI_KEY),
            "api key reached the wire: {wire}"
        );
        assert!(
            !wire.contains("hunter2"),
            "url password reached the wire: {wire}"
        );
    }

    #[test]
    fn tool_result_secrets_and_url_credentials_are_scrubbed() {
        let mut msg = tool_result(&format!(
            "OPENAI_API_KEY={OPENAI_KEY}\nDATABASE_URL={DB_URL}"
        ));
        assert!(scrub_message(&mut msg));
        let MessagePart::ToolResult { content, .. } = &msg.parts[0] else {
            panic!("part kind changed");
        };
        assert!(
            !content.contains(OPENAI_KEY) && !content.contains("hunter2"),
            "{content}"
        );
        assert!(!msg.content.contains(OPENAI_KEY), "content is rebuilt");
    }

    #[test]
    fn tool_output_body_is_scrubbed_but_paths_survive() {
        let mut msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "shell".into(),
                body: format!("/home/dev/.env: {OPENAI_KEY}"),
                compacted_at: None,
            }],
        );
        assert!(scrub_message(&mut msg));
        let MessagePart::ToolOutput { body, .. } = &msg.parts[0] else {
            panic!("part kind changed");
        };
        assert!(!body.contains(OPENAI_KEY));
        assert!(
            body.contains("/home/dev/.env"),
            "tool paths are data: {body}"
        );
    }

    #[test]
    fn plain_content_message_is_scrubbed_with_paths() {
        let mut msg = Message::from_legacy(Role::User, format!("key {OPENAI_KEY} in /Users/a/b"));
        assert!(scrub_message(&mut msg));
        assert!(!msg.content.contains(OPENAI_KEY) && !msg.content.contains("/Users/a"));
    }

    #[test]
    fn tool_use_input_is_left_intact() {
        let input = serde_json::json!({"command": format!("echo {OPENAI_KEY}")});
        let mut msg = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: input.clone(),
            }],
        );
        assert!(!scrub_message(&mut msg));
        let MessagePart::ToolUse { input: kept, .. } = &msg.parts[0] else {
            panic!("part kind changed");
        };
        assert_eq!(kept, &input);
    }

    #[test]
    fn clean_message_reports_no_change() {
        let mut msg = tool_result("all good");
        assert!(!scrub_message(&mut msg));
    }

    #[test]
    fn claude_wire_has_no_tool_result_secrets_after_scrub() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-5-5".into(), 1024);
        let messages = history(&format!("KEY={OPENAI_KEY} URL={DB_URL}"), true);
        assert_clean(&provider.debug_request_json(&messages, &tools(), false));
    }

    fn openai() -> OpenAiProvider {
        OpenAiProvider::new(OpenAiConfig {
            api_key: "k".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.2".into(),
            max_tokens: 1024,
            embedding_model: None,
            reasoning_effort: None,
            context_window: None,
            completion_tokens_param: None,
            vision: None,
        })
    }

    #[test]
    fn wire_carries_tool_result_secrets_without_the_scrub() {
        let messages = history(&format!("KEY={OPENAI_KEY}"), false);
        let wire = openai()
            .debug_request_json(&messages, &tools(), false)
            .to_string();
        assert!(
            wire.contains(OPENAI_KEY),
            "control: builders read parts, not content"
        );
    }

    #[test]
    fn openai_wire_has_no_tool_result_secrets_after_scrub() {
        let provider = openai();
        let messages = history(&format!("KEY={OPENAI_KEY} URL={DB_URL}"), true);
        assert_clean(&provider.debug_request_json(&messages, &tools(), false));
    }
}
