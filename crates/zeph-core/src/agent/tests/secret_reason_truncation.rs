// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Build the prompt string the same way `process_pending_secret_requests` does.
fn build_prompt(secret_key: &str, reason: Option<&str>) -> String {
    format!(
        "Sub-agent requests secret '{}'. Allow?{}",
        crate::text::truncate_to_chars(secret_key, 100),
        reason
            .map(|r| format!(" Reason: {}", crate::text::truncate_to_chars(r, 200)))
            .unwrap_or_default()
    )
}

#[test]
fn reason_short_ascii_unchanged() {
    let reason = "need access to external API";
    let prompt = build_prompt("MY_SECRET", Some(reason));
    assert!(prompt.contains(reason));
}

#[test]
fn reason_over_200_chars_truncated_to_200() {
    let reason = "a".repeat(300);
    let prompt = build_prompt("MY_SECRET", Some(&reason));
    // Extract the reason portion after "Reason: "
    let after = prompt.split("Reason: ").nth(1).unwrap();
    // truncate_to_chars appends … (U+2026) when truncating: 200 chars + ellipsis = 201.
    assert_eq!(after.chars().count(), 201);
    assert!(after.ends_with('\u{2026}'));
}

#[test]
fn reason_exactly_200_chars_unchanged() {
    let reason = "b".repeat(200);
    let prompt = build_prompt("MY_SECRET", Some(&reason));
    let after = prompt.split("Reason: ").nth(1).unwrap();
    // Exactly at limit: no truncation, no ellipsis.
    assert_eq!(after.chars().count(), 200);
    assert!(!after.ends_with('\u{2026}'));
}

#[test]
fn reason_multibyte_utf8_truncated_at_char_boundary() {
    // Each Cyrillic char is 2 bytes; 300 chars = 600 bytes.
    let reason = "й".repeat(300);
    let prompt = build_prompt("MY_SECRET", Some(&reason));
    let after = prompt.split("Reason: ").nth(1).unwrap();
    // truncate_to_chars appends … when truncating: 200 chars + ellipsis = 201.
    assert_eq!(after.chars().count(), 201);
    assert!(after.ends_with('\u{2026}'));
    assert!(std::str::from_utf8(after.as_bytes()).is_ok());
}

#[test]
fn reason_none_produces_no_reason_suffix() {
    let prompt = build_prompt("MY_SECRET", None);
    assert!(!prompt.contains("Reason:"));
    assert!(prompt.ends_with("Allow?"));
}

#[test]
fn secret_key_short_unchanged() {
    let prompt = build_prompt("MY_API_KEY", None);
    assert!(prompt.contains("MY_API_KEY"));
}

#[test]
fn secret_key_over_100_chars_truncated() {
    let key = "A".repeat(150);
    let prompt = build_prompt(&key, None);
    // Extract the key portion between "secret '" and "'."
    let after_quote = prompt.split("secret '").nth(1).unwrap();
    let key_in_prompt = after_quote.split("'. Allow?").next().unwrap();
    // truncate_to_chars appends … when truncating: 100 chars + ellipsis = 101.
    assert_eq!(key_in_prompt.chars().count(), 101);
    assert!(key_in_prompt.ends_with('\u{2026}'));
}

#[test]
fn secret_key_exactly_100_chars_unchanged() {
    let key = "B".repeat(100);
    let prompt = build_prompt(&key, None);
    let after_quote = prompt.split("secret '").nth(1).unwrap();
    let key_in_prompt = after_quote.split("'. Allow?").next().unwrap();
    assert_eq!(key_in_prompt.chars().count(), 100);
    assert!(!key_in_prompt.ends_with('\u{2026}'));
}
