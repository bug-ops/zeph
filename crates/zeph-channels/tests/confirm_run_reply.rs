// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "This integration test uses unwraps to keep setup and assertion failures concise."
)]

//! Acceptance tests (tests-first) for `ConfirmLoop::run_reply` (plan r2 Track A, M4') and the
//! guarantee that `run_confirm` keeps its spec-007 behaviour on top of it.

use std::time::Duration;

use tokio::sync::mpsc;
use zeph_channels::CONFIRM_TIMEOUT;
use zeph_channels::confirm::{ConfirmLoop, ReplyOutcome};
use zeph_core::channel::ChannelError;

struct Incoming {
    in_scope: bool,
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

    fn confirm_accepts(&self, incoming: &Incoming) -> bool {
        incoming.in_scope
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

fn fake() -> (mpsc::Sender<Incoming>, Fake) {
    let (tx, rx) = mpsc::channel(4);
    (
        tx,
        Fake {
            rx,
            sent: Vec::new(),
        },
    )
}

fn reply(text: &str) -> Incoming {
    Incoming {
        in_scope: true,
        text: text.to_owned(),
    }
}

#[tokio::test]
async fn run_reply_returns_raw_text_including_cancel_keyword() {
    let (tx, mut f) = fake();
    tx.send(reply("/cancel")).await.unwrap();
    let out = f.run_reply("Which?").await.unwrap();
    assert_eq!(out, ReplyOutcome::Text("/cancel".to_owned()));
}

#[tokio::test]
async fn run_reply_sends_the_prompt_once_without_a_timeout_suffix() {
    let (tx, mut f) = fake();
    tx.send(reply("x")).await.unwrap();
    f.run_reply("Which?").await.unwrap();
    assert_eq!(f.sent.len(), 1);
    assert!(f.sent[0].contains("Which?"));
    assert!(
        !f.sent[0].contains("timeout"),
        "the agent owns the deadline: {}",
        f.sent[0]
    );
}

#[tokio::test]
async fn run_reply_ignores_out_of_scope_messages() {
    let (tx, mut f) = fake();
    tx.send(Incoming {
        in_scope: false,
        text: "intruder".to_owned(),
    })
    .await
    .unwrap();
    tx.send(reply("mine")).await.unwrap();
    assert_eq!(
        f.run_reply("Which?").await.unwrap(),
        ReplyOutcome::Text("mine".to_owned())
    );
}

#[tokio::test]
async fn run_reply_reports_closed_when_receiver_closes() {
    let (tx, mut f) = fake();
    drop(tx);
    assert_eq!(f.run_reply("Which?").await.unwrap(), ReplyOutcome::Closed);
}

#[tokio::test(start_paused = true)]
async fn run_reply_has_no_deadline_of_its_own() {
    let (tx, mut f) = fake();
    let sender = tokio::spawn(async move {
        tokio::time::sleep(CONFIRM_TIMEOUT * 10).await;
        tx.send(reply("late")).await.unwrap();
    });
    let out = f.run_reply("Which?").await.unwrap();
    assert_eq!(out, ReplyOutcome::Text("late".to_owned()));
    sender.await.unwrap();
}

#[tokio::test]
async fn run_confirm_prompt_and_yes_semantics_unchanged() {
    let (tx, mut f) = fake();
    tx.send(reply(" YES ")).await.unwrap();
    assert!(f.run_confirm("Proceed?").await.unwrap());
    assert_eq!(
        f.sent,
        vec!["Proceed?\nReply 'yes' to confirm (timeout: 30s).".to_owned()],
        "no cancellation notice and no extra message"
    );
}

#[tokio::test]
async fn run_confirm_slash_cancel_is_a_plain_deny_without_notice() {
    let (tx, mut f) = fake();
    tx.send(reply("/cancel")).await.unwrap();
    assert!(!f.run_confirm("Proceed?").await.unwrap());
    assert_eq!(f.sent.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn run_confirm_denies_after_confirm_timeout() {
    let (_tx, mut f) = fake();
    let started = tokio::time::Instant::now();
    assert!(!f.run_confirm("Proceed?").await.unwrap());
    assert!(started.elapsed() >= CONFIRM_TIMEOUT);
    assert!(started.elapsed() < CONFIRM_TIMEOUT + Duration::from_secs(1));
}

#[tokio::test]
async fn run_confirm_closed_receiver_denies_without_error() {
    let (tx, mut f) = fake();
    drop(tx);
    assert!(!f.run_confirm("Proceed?").await.unwrap());
}
