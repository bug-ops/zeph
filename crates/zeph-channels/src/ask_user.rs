// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `ask_user` flow of the bot adapters (Telegram, Discord, Slack).
//!
//! Every adapter already implements [`ConfirmLoop`], so a question is one prompt message
//! followed by waiting for the next in-scope reply. This module renders the prompt (with the
//! fixed [`ASK_USER_HEADER`] first), classifies each reply, and maps it onto an
//! [`AskUserOutcome`]. It applies no deadline: the agent owns the timeout and drops the future.

use std::fmt::Write as _;

use zeph_core::channel::{
    ASK_USER_COMMAND_NOTICE, ASK_USER_HEADER, ASK_USER_RETRY_HINT, AskUserKind, AskUserOutcome,
    AskUserRequest, ChannelError, ChoiceOptions, OtherAnswer, ReplyKind, classify_reply,
};

use crate::confirm::{ConfirmLoop, ReplyOutcome};

/// Zero-width space inserted to defuse markup in model-authored text.
const ZWSP: char = '\u{200b}';

/// Break `@everyone`, `@here`, `<@user>` and `<!channel>`-style mentions without changing how
/// the text reads, so a question can never ping a whole channel.
#[cfg(feature = "discord")]
#[must_use]
pub(crate) fn neutralize_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        let breaks = match c {
            '@' => true,
            '<' => matches!(chars.peek(), Some('@' | '!' | '#')),
            _ => false,
        };
        if breaks {
            out.push(ZWSP);
        }
    }
    out
}

/// Break markdown link syntax (inline `[text](url)`, reference `[text][1]` and shortcut
/// `[text]` links, and their `[1]: url` definitions) so a model-authored question cannot
/// render a disguised link (Discord, Telegram).
#[must_use]
pub(crate) fn neutralize_links(text: &str) -> String {
    text.replace(']', &format!("]{ZWSP}"))
}

/// Escape Slack control characters so `<url|text>` links and `<!channel>`-style mentions show
/// as plain text.
#[cfg(feature = "slack")]
#[must_use]
pub(crate) fn escape_slack(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_options(out: &mut String, options: &ChoiceOptions) {
    for (i, label) in options.labels().iter().enumerate() {
        let _ = writeln!(out, "{}. {label}", i + 1);
    }
}

/// The prompt shown for `request`: header, question, numbered options and reply instructions.
///
/// `cancel` is an instruction line, never a numbered option, so parsed indices stay below the
/// option count.
#[must_use]
pub(crate) fn render_prompt(request: &AskUserRequest) -> String {
    let mut out = format!("{ASK_USER_HEADER}\n\n{}\n", request.question());
    match request.kind() {
        AskUserKind::FreeText => out.push_str("\nReply with your answer."),
        AskUserKind::SingleChoice { options, other }
        | AskUserKind::MultiChoice { options, other } => {
            out.push('\n');
            render_options(&mut out, options);
            let multi = matches!(request.kind(), AskUserKind::MultiChoice { .. });
            out.push_str(if multi {
                "\nReply with numbers separated by commas, or option texts."
            } else {
                "\nReply with a number or the option text."
            });
            if *other == OtherAnswer::Allowed {
                out.push_str(" You can also type your own answer.");
            }
        }
    }
    out.push_str(" Reply `cancel` to cancel.");
    out
}

/// Ask `request` through an adapter's [`ConfirmLoop`].
///
/// `sanitize` escapes the rendered prompt for the adapter's markup; `is_local_command` marks
/// replies the adapter handles itself (Telegram `/start`), which cancel the question like a
/// registered slash command does.
///
/// # Errors
///
/// Returns an error if sending a prompt or notice fails.
pub(crate) async fn run_ask_user<L: ConfirmLoop>(
    channel: &mut L,
    request: AskUserRequest,
    sanitize: fn(&str) -> String,
    is_local_command: fn(&str) -> bool,
) -> Result<AskUserOutcome, ChannelError> {
    let base = sanitize(&render_prompt(&request));
    let mut prompt = base.clone();
    loop {
        let text = match channel.run_reply(&prompt).await? {
            ReplyOutcome::Text(text) => text,
            ReplyOutcome::Closed => return Ok(AskUserOutcome::Cancelled),
        };
        let answer = match classify_reply(&text) {
            ReplyKind::Cancel => return Ok(AskUserOutcome::Cancelled),
            ReplyKind::Command => return cancel_for_command(channel, sanitize).await,
            ReplyKind::Answer(_) if is_local_command(&text) => {
                return cancel_for_command(channel, sanitize).await;
            }
            ReplyKind::Answer(answer) => answer,
        };
        match request.parse_reply(&answer) {
            Ok(parsed) => return Ok(AskUserOutcome::Answered(parsed)),
            Err(error) => {
                tracing::debug!(%error, "ask_user: reply did not match the question");
                prompt = format!("{}\n\n{base}", sanitize(ASK_USER_RETRY_HINT));
            }
        }
    }
}

async fn cancel_for_command<L: ConfirmLoop>(
    channel: &mut L,
    sanitize: fn(&str) -> String,
) -> Result<AskUserOutcome, ChannelError> {
    channel
        .confirm_send_prompt(&sanitize(ASK_USER_COMMAND_NOTICE))
        .await?;
    Ok(AskUserOutcome::Cancelled)
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use zeph_core::channel::AskUserAnswer;

    struct Incoming {
        text: String,
    }

    struct Fake {
        rx: mpsc::Receiver<Incoming>,
        sent: Vec<String>,
    }

    impl ConfirmLoop for Fake {
        type Incoming = Incoming;

        fn confirm_label(&self) -> &'static str {
            "fake"
        }

        fn confirm_receiver(&mut self) -> &mut mpsc::Receiver<Incoming> {
            &mut self.rx
        }

        fn confirm_accepts(&self, _incoming: &Incoming) -> bool {
            true
        }

        fn confirm_reply_text<'a>(&self, incoming: &'a Incoming) -> &'a str {
            &incoming.text
        }

        #[expect(
            clippy::unused_async_trait_impl,
            reason = "The implemented trait requires an async method even though this implementation completes immediately."
        )]
        async fn confirm_send_prompt(&mut self, text: &str) -> Result<(), ChannelError> {
            self.sent.push(text.to_owned());
            Ok(())
        }
    }

    fn fake(replies: &[&str]) -> Fake {
        let (tx, rx) = mpsc::channel(8);
        for r in replies {
            tx.try_send(Incoming {
                text: (*r).to_owned(),
            })
            .unwrap();
        }
        Fake {
            rx,
            sent: Vec::new(),
        }
    }

    fn choice() -> AskUserRequest {
        let options = ChoiceOptions::new(vec!["red".into(), "green".into()]).unwrap();
        AskUserRequest::single_choice("Pick one", options, OtherAnswer::Forbidden).unwrap()
    }

    fn identity(text: &str) -> String {
        text.to_owned()
    }

    fn never(_: &str) -> bool {
        false
    }

    fn start_only(text: &str) -> bool {
        text.split_whitespace().next() == Some("/start")
    }

    #[test]
    fn prompt_starts_with_the_fixed_header_and_never_numbers_cancel() {
        let prompt = render_prompt(&choice());
        assert!(prompt.starts_with(ASK_USER_HEADER), "{prompt}");
        assert!(prompt.contains("1. red") && prompt.contains("2. green"));
        assert!(
            !prompt.contains("3."),
            "cancel must not be a numbered option: {prompt}"
        );
        assert!(prompt.contains("`cancel`"));
    }

    #[test]
    fn markdown_links_are_broken() {
        let out = neutralize_links("see [docs](https://evil.example) now");
        assert!(!out.contains("]("), "{out}");
        assert!(out.contains("[docs]") && out.contains("https://evil.example"));
    }

    #[test]
    fn no_markdown_link_form_survives() {
        use pulldown_cmark::{Event, Parser, Tag};

        for text in [
            "[docs](https://evil.example)",
            "[the console][1]\n\n[1]: https://evil.example",
            "[1]\n\n[1]: https://evil.example",
            "[docs]: https://evil.example\n\n[docs]",
        ] {
            let out = neutralize_links(text);
            let has_link = Parser::new(&out).any(|e| matches!(e, Event::Start(Tag::Link { .. })));
            assert!(!has_link, "link survived in {out:?}");
        }
    }

    #[cfg(feature = "slack")]
    #[test]
    fn slack_control_characters_are_escaped() {
        let out = escape_slack("<https://evil|click> <!channel> a&b");
        assert_eq!(out, "&lt;https://evil|click&gt; &lt;!channel&gt; a&amp;b");
    }

    #[cfg(feature = "discord")]
    #[test]
    fn mentions_are_defused() {
        let out = neutralize_mentions("hi @everyone <@123> <!channel> <#c> user@host");
        assert!(!out.contains("@everyone"));
        assert!(!out.contains("<@"));
        assert!(!out.contains("<!channel>"));
        assert!(out.contains("everyone"));
    }

    #[tokio::test]
    async fn a_number_selects_the_option() {
        let mut ch = fake(&["2"]);
        let out = run_ask_user(&mut ch, choice(), identity, never)
            .await
            .unwrap();
        assert_eq!(
            out,
            AskUserOutcome::Answered(AskUserAnswer::Selected { indices: vec![1] })
        );
        assert!(ch.sent[0].starts_with(ASK_USER_HEADER));
    }

    #[tokio::test]
    async fn cancel_keyword_and_slash_cancel_cancel() {
        for reply in ["cancel", "/cancel"] {
            let mut ch = fake(&[reply]);
            let out = run_ask_user(&mut ch, choice(), identity, never)
                .await
                .unwrap();
            assert_eq!(out, AskUserOutcome::Cancelled, "{reply}");
            assert_eq!(ch.sent.len(), 1, "no notice for an explicit cancel");
        }
    }

    #[tokio::test]
    async fn registered_command_cancels_with_a_notice() {
        let mut ch = fake(&["/status"]);
        let out = run_ask_user(&mut ch, choice(), identity, never)
            .await
            .unwrap();
        assert_eq!(out, AskUserOutcome::Cancelled);
        assert_eq!(
            ch.sent.last().map(String::as_str),
            Some(ASK_USER_COMMAND_NOTICE)
        );
    }

    #[tokio::test]
    async fn local_command_cancels_with_a_notice() {
        let mut ch = fake(&["/start"]);
        let out = run_ask_user(&mut ch, choice(), identity, start_only)
            .await
            .unwrap();
        assert_eq!(out, AskUserOutcome::Cancelled);
        assert_eq!(
            ch.sent.last().map(String::as_str),
            Some(ASK_USER_COMMAND_NOTICE)
        );
    }

    #[tokio::test]
    async fn a_path_is_an_answer_to_a_free_text_question() {
        let mut ch = fake(&["/home/me/proj"]);
        let request = AskUserRequest::free_text("Where?").unwrap();
        let out = run_ask_user(&mut ch, request, identity, never)
            .await
            .unwrap();
        assert_eq!(
            out,
            AskUserOutcome::Answered(AskUserAnswer::Text("/home/me/proj".into()))
        );
    }

    #[tokio::test]
    async fn an_invalid_reply_reprompts_until_a_valid_one() {
        let mut ch = fake(&["purple", "1"]);
        let out = run_ask_user(&mut ch, choice(), identity, never)
            .await
            .unwrap();
        assert_eq!(
            out,
            AskUserOutcome::Answered(AskUserAnswer::Selected { indices: vec![0] })
        );
        assert_eq!(ch.sent.len(), 2);
        assert!(ch.sent[1].starts_with(ASK_USER_RETRY_HINT));
    }

    #[tokio::test]
    async fn closed_receiver_cancels() {
        let mut ch = fake(&[]);
        let out = run_ask_user(&mut ch, choice(), identity, never)
            .await
            .unwrap();
        assert_eq!(out, AskUserOutcome::Cancelled);
    }
}
