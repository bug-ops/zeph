// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "This integration test uses unwraps to keep setup and assertion failures concise."
)]

//! Acceptance tests (tests-first) for #7132: typed `CallAttribution { caller: Option<CallerId>,
//! active_skills: Vec<SkillName> }` embedded once as `attribution` in `ToolCall` and `AuditEntry`.
//!
//! Assumed API: `zeph_tools::CallerId::new(impl AsRef<str>)` / `as_str()` (serde transparent),
//! `zeph_tools::CallAttribution` fields `caller` and `active_skills`, public field
//! `attribution: CallAttribution` on `ToolCall` and `AuditEntry` (`#[serde(flatten)]`, keys
//! `caller_id` / `skill_name`), `ToolCall::attribution(&self) -> &CallAttribution`.

use zeph_common::types::SkillName;
use zeph_tools::{AuditEntry, CallAttribution, CallerId, ToolCall};

fn memory_entry() -> AuditEntry {
    AuditEntry::memory_write("tool_output", "saved: preview", None, None)
}

#[test]
fn call_attribution_default_has_no_caller_and_no_skills() {
    let a = CallAttribution::default();
    assert!(a.caller.is_none());
    assert_eq!(a.active_skills, Vec::<SkillName>::new());
}

#[test]
fn tool_call_default_attribution_equals_default_attribution() {
    assert_eq!(
        ToolCall::default().attribution(),
        &CallAttribution::default()
    );
}

#[test]
fn tool_call_attribution_reflects_the_embedded_value() {
    let attribution = CallAttribution {
        caller: Some(CallerId::new("user-1")),
        active_skills: vec![SkillName::parse("deploy-helper").unwrap()],
    };
    let call = ToolCall {
        attribution: attribution.clone(),
        ..ToolCall::default()
    };
    assert_eq!(call.attribution(), &attribution);
    assert_eq!(
        call.attribution().caller.as_ref().unwrap().as_str(),
        "user-1"
    );
}

#[test]
fn caller_id_serializes_as_a_plain_string() {
    let json = serde_json::to_string(&CallerId::new("u-7")).unwrap();
    assert_eq!(json, "\"u-7\"");
    let back: CallerId = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "u-7");
}

#[test]
fn audit_entry_json_flattens_caller_id_and_omits_empty_skill_name() {
    let json = serde_json::to_value(memory_entry()).unwrap();
    assert_eq!(json["caller_id"], "tool_output");
    assert!(
        json.get("skill_name").is_none(),
        "empty skills must be omitted: {json}"
    );
    assert!(
        json.get("attribution").is_none(),
        "attribution must be flattened: {json}"
    );
}

#[test]
fn audit_entry_json_includes_skill_name_array_when_skills_are_active() {
    let mut entry = memory_entry();
    entry.attribution.active_skills = vec![
        SkillName::parse("deploy-helper").unwrap(),
        SkillName::parse("git-flow").unwrap(),
    ];
    let json = serde_json::to_value(entry).unwrap();
    assert_eq!(
        json["skill_name"],
        serde_json::json!(["deploy-helper", "git-flow"])
    );
}

#[test]
fn audit_entry_json_omits_caller_id_when_there_is_no_caller() {
    let mut entry = memory_entry();
    entry.attribution.caller = None;
    let json = serde_json::to_value(entry).unwrap();
    assert!(json.get("caller_id").is_none(), "{json}");
}
