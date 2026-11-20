// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for the pure `ask_user` contracts of specs 090 / 102:
//! reply classification, choice-option validation, reply parsing, the fixed header, the
//! `Channel::ask_user` default decline and `InteractionConfig` wiring.
//!
//! The API under test is the one declared by the PR1 plan (architect r2-r4). Items the plan
//! does not pin down exactly are isolated in the `api` module so a single place needs fixing.

use std::time::{Duration, Instant};

use zeph_core::InteractionConfig;
use zeph_core::channel::{
    ASK_USER_HEADER, AskUserOutcome, Channel, ChoiceOptions, DeclineReason, LoopbackChannel,
    ReplyKind, classify_reply,
};

/// Constructors/accessors the plan leaves open; adjust here only.
mod api {
    use zeph_core::channel::{AskUserAnswer, AskUserRequest, ChoiceOptions, OtherAnswer};

    pub fn free_text(q: &str) -> AskUserRequest {
        AskUserRequest::free_text(q).expect("valid free-text question")
    }

    pub fn single_choice(q: &str, labels: &[&str], other: OtherAnswer) -> AskUserRequest {
        let opts = ChoiceOptions::new(labels.iter().map(|s| (*s).to_owned()).collect())
            .expect("valid options");
        AskUserRequest::single_choice(q, opts, other).expect("valid choice question")
    }

    pub fn multi_choice(q: &str, labels: &[&str]) -> AskUserRequest {
        let opts = ChoiceOptions::new(labels.iter().map(|s| (*s).to_owned()).collect())
            .expect("valid options");
        AskUserRequest::multi_choice(q, opts, OtherAnswer::Forbidden).expect("valid question")
    }

    pub fn selected(a: &AskUserAnswer) -> Vec<usize> {
        let AskUserAnswer::Selected { indices, .. } = a else {
            panic!("expected a Selected answer, got {a:?}");
        };
        indices.clone()
    }
}
use api::{free_text, multi_choice, selected, single_choice};
use zeph_core::channel::{AskUserAnswer, OtherAnswer};

// ---- classify_reply (N3) -------------------------------------------------------------

#[test]
fn classify_reply_cancel_keyword_is_cancel() {
    assert_eq!(classify_reply("cancel"), ReplyKind::Cancel);
}

#[test]
fn classify_reply_slash_cancel_is_cancel() {
    assert_eq!(classify_reply("/cancel"), ReplyKind::Cancel);
}

#[test]
fn classify_reply_cancel_is_trimmed_and_case_insensitive() {
    assert_eq!(classify_reply("  CaNcEl \n"), ReplyKind::Cancel);
    assert_eq!(classify_reply("/Cancel"), ReplyKind::Cancel);
}

#[test]
fn classify_reply_registered_command_is_command() {
    let name = zeph_commands::COMMANDS
        .iter()
        .map(|c| c.name)
        .find(|n| n.starts_with('/') && *n != "/cancel")
        .expect("COMMANDS has at least one slash command");
    assert_eq!(classify_reply(name), ReplyKind::Command);
}

#[test]
fn classify_reply_registered_command_with_args_is_command() {
    assert_eq!(classify_reply("/status now"), ReplyKind::Command);
}

#[test]
fn classify_reply_unix_path_is_answer() {
    assert_eq!(
        classify_reply("/home/me/proj"),
        ReplyKind::Answer("/home/me/proj".to_owned())
    );
}

#[test]
fn classify_reply_command_prefix_without_separator_is_answer() {
    // `/status` is a command, `/statusbar` must not be (same rule as find_handler).
    assert_eq!(
        classify_reply("/statusbar"),
        ReplyKind::Answer("/statusbar".to_owned())
    );
}

#[test]
fn classify_reply_plain_text_is_answer_unchanged() {
    assert_eq!(
        classify_reply("  keep the spaces  "),
        ReplyKind::Answer("  keep the spaces  ".to_owned())
    );
}

#[test]
fn classify_reply_cancel_as_substring_is_answer() {
    assert_eq!(
        classify_reply("cancel my subscription"),
        ReplyKind::Answer("cancel my subscription".to_owned())
    );
}

// ---- ChoiceOptions ---------------------------------------------------------------------

fn labels(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("opt{i}")).collect()
}

#[test]
fn choice_options_new_rejects_cancel_labels() {
    for bad in ["cancel", "/cancel", " Cancel ", "/CANCEL"] {
        let r = ChoiceOptions::new(vec!["ok".to_owned(), bad.to_owned()]);
        assert!(r.is_err(), "label {bad:?} must be rejected");
    }
}

#[test]
fn choice_options_new_enforces_count_bounds() {
    assert!(ChoiceOptions::new(labels(1)).is_err());
    assert!(ChoiceOptions::new(labels(2)).is_ok());
    assert!(ChoiceOptions::new(labels(8)).is_ok());
    assert!(ChoiceOptions::new(labels(9)).is_err());
}

#[test]
fn choice_options_new_rejects_all_digit_labels() {
    for bad in ["8", " 8 ", "42", "\t7\n"] {
        let r = ChoiceOptions::new(vec!["ok".to_owned(), bad.to_owned()]);
        assert!(
            r.is_err(),
            "digit-only label {bad:?} would shadow an index reply"
        );
    }
}

#[test]
fn choice_options_new_accepts_labels_with_digits_and_text() {
    assert!(ChoiceOptions::new(vec!["2 GB".to_owned(), "4 GB".to_owned()]).is_ok());
    assert!(ChoiceOptions::new(vec!["v2".to_owned(), "x".to_owned()]).is_ok());
}

#[test]
fn choice_options_new_rejects_duplicates_empty_and_oversized() {
    assert!(ChoiceOptions::new(vec!["a".into(), "a".into()]).is_err());
    assert!(ChoiceOptions::new(vec!["a".into(), String::new()]).is_err());
    assert!(ChoiceOptions::new(vec!["a".into(), "x".repeat(201)]).is_err());
    assert!(ChoiceOptions::new(vec!["a".into(), "x".repeat(200)]).is_ok());
}

// ---- AskUserRequest::parse_reply ---------------------------------------------------------

#[test]
fn parse_reply_single_choice_one_based_index() {
    let req = single_choice("pick", &["red", "green"], OtherAnswer::Forbidden);
    assert_eq!(selected(&req.parse_reply("2").unwrap()), vec![1]);
}

#[test]
fn parse_reply_index_out_of_range_is_err() {
    let req = single_choice("pick", &["red", "green"], OtherAnswer::Forbidden);
    assert!(req.parse_reply("3").is_err());
    assert!(req.parse_reply("0").is_err());
}

#[test]
fn parse_reply_label_is_case_insensitive() {
    let req = single_choice("pick", &["Red", "Green"], OtherAnswer::Forbidden);
    assert_eq!(selected(&req.parse_reply("gReEn").unwrap()), vec![1]);
}

#[test]
fn parse_reply_free_text_rejected_when_other_forbidden() {
    let req = single_choice("pick", &["red", "green"], OtherAnswer::Forbidden);
    assert!(req.parse_reply("purple").is_err());
}

#[test]
fn parse_reply_free_text_accepted_when_other_allowed() {
    let req = single_choice("pick", &["red", "green"], OtherAnswer::Allowed);
    assert!(req.parse_reply("purple").is_ok());
}

#[test]
fn parse_reply_multi_choice_comma_separated() {
    let req = multi_choice("pick", &["a", "b", "c"]);
    let mut got = selected(&req.parse_reply("1, 3").unwrap());
    got.sort_unstable();
    assert_eq!(got, vec![0, 2]);
}

#[test]
fn parse_reply_multi_choice_any_bad_index_rejects_whole_reply() {
    let req = multi_choice("pick", &["a", "b", "c"]);
    assert!(req.parse_reply("1,4").is_err());
}

#[test]
fn parse_reply_free_text_question_returns_text_unchanged() {
    let req = free_text("where?");
    assert_eq!(
        req.parse_reply("/home/me").unwrap(),
        AskUserAnswer::Text("/home/me".to_owned())
    );
}

// ---- fixed header (S5) ---------------------------------------------------------------------

#[test]
fn ask_user_header_warns_about_secrets() {
    let h = ASK_USER_HEADER.to_lowercase();
    assert!(h.contains("never share"), "header: {ASK_USER_HEADER}");
    assert!(h.contains("password") && h.contains("key"));
}

// ---- Channel default (FR-003 / FR-004 / SC-002) -----------------------------------------------

#[tokio::test]
async fn loopback_channel_ask_user_without_responder_declines_non_interactive_fast() {
    let (mut ch, _handle) = LoopbackChannel::pair(8);
    let started = Instant::now();
    let out = ch.ask_user(free_text("anything?")).await.expect("no error");
    assert_eq!(out, AskUserOutcome::Declined(DeclineReason::NonInteractive));
    assert!(started.elapsed() < Duration::from_millis(100));
}

// ---- InteractionConfig (N2) ---------------------------------------------------------------------

/// Fails to compile if `InteractionConfig` implements `Default` (it must not, so every deps
/// struct literal is forced to set it).
const _: fn() = || {
    trait AmbiguousIfDefault<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfDefault<()> for T {}
    impl<T: Default> AmbiguousIfDefault<u8> for T {}
    let _ = <InteractionConfig as AmbiguousIfDefault<_>>::some_item;
};

#[test]
fn interaction_config_from_config_default_is_both_disabled() {
    let ic = InteractionConfig::from_config(&zeph_core::config::Config::default());
    assert!(!ic.ask_user.enabled);
    assert!(!ic.cache_keepalive.enabled);
}

#[test]
fn interaction_config_from_config_is_field_for_field() {
    let mut cfg = zeph_core::config::Config::default();
    cfg.tools.ask_user.enabled = true;
    cfg.tools.ask_user.timeout_secs = zeph_config::AskUserTimeout::new(17).unwrap();
    cfg.tools.ask_user.max_questions_per_turn = std::num::NonZeroU8::new(2).unwrap();
    cfg.llm.cache_keepalive.enabled = true;
    cfg.llm.cache_keepalive.max_consecutive_pings = zeph_config::KeepalivePingCap::new(4).unwrap();
    let ic = InteractionConfig::from_config(&cfg);
    assert_eq!(ic.ask_user, cfg.tools.ask_user);
    assert_eq!(ic.cache_keepalive, cfg.llm.cache_keepalive);
}
