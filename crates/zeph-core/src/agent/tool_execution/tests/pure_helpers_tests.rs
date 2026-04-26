// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent::tool_execution::{
    doom_loop_hash, normalize_for_doom_loop, tool_def_to_definition,
};

#[test]
fn tool_def_strips_schema_and_title() {
    use schemars::Schema;
    use zeph_tools::registry::{InvocationHint, ToolDef};

    let raw: serde_json::Value = serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "BashParams",
        "type": "object",
        "properties": {
            "command": { "type": "string" }
        },
        "required": ["command"]
    });
    let schema: Schema = serde_json::from_value(raw).expect("valid schema");
    let def = ToolDef {
        id: "bash".into(),
        description: "run a shell command".into(),
        schema,
        invocation: InvocationHint::ToolCall,
        output_schema: None,
    };

    let result = tool_def_to_definition(&def);
    let map = result.parameters.as_object().expect("should be object");
    assert!(!map.contains_key("$schema"));
    assert!(!map.contains_key("title"));
    assert!(map.contains_key("type"));
    assert!(map.contains_key("properties"));
}

#[test]
fn normalize_empty_string() {
    assert_eq!(normalize_for_doom_loop(""), "");
}

#[test]
fn normalize_multiple_tool_results() {
    let s = "[tool_result: id1]\nok\n[tool_result: id2]\nfail\n[tool_result: id3]\nok";
    let expected = "[tool_result]\nok\n[tool_result]\nfail\n[tool_result]\nok";
    assert_eq!(normalize_for_doom_loop(s), expected);
}

#[test]
fn normalize_strips_tool_result_ids() {
    let a = "[tool_result: toolu_abc123]\nerror: missing field";
    let b = "[tool_result: toolu_xyz789]\nerror: missing field";
    assert_eq!(normalize_for_doom_loop(a), normalize_for_doom_loop(b));
    assert_eq!(
        normalize_for_doom_loop(a),
        "[tool_result]\nerror: missing field"
    );
}

#[test]
fn normalize_strips_tool_use_ids() {
    let a = "[tool_use: bash(toolu_abc)]";
    let b = "[tool_use: bash(toolu_xyz)]";
    assert_eq!(normalize_for_doom_loop(a), normalize_for_doom_loop(b));
    assert_eq!(normalize_for_doom_loop(a), "[tool_use: bash]");
}

#[test]
fn normalize_preserves_plain_text() {
    let text = "hello world, no tool tags here";
    assert_eq!(normalize_for_doom_loop(text), text);
}

#[test]
fn normalize_handles_mixed_tag_order() {
    let s = "[tool_use: bash(id1)] result: [tool_result: id2]";
    assert_eq!(
        normalize_for_doom_loop(s),
        "[tool_use: bash] result: [tool_result]"
    );
}

// Helpers to hash a string the same way doom_loop_hash would if it materialized.
fn hash_str(s: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut h = DefaultHasher::new();
    h.write(s.as_bytes());
    h.finish()
}

// doom_loop_hash must produce the same value as hashing the normalize_for_doom_loop output.
fn expected_hash(content: &str) -> u64 {
    hash_str(&normalize_for_doom_loop(content))
}

#[test]
fn doom_loop_hash_matches_normalize_then_hash_plain_text() {
    let s = "hello world, no tool tags here";
    assert_eq!(doom_loop_hash(s), expected_hash(s));
}

#[test]
fn doom_loop_hash_matches_normalize_then_hash_tool_result() {
    let s = "[tool_result: toolu_abc123]\nerror: missing field";
    assert_eq!(doom_loop_hash(s), expected_hash(s));
}

#[test]
fn doom_loop_hash_matches_normalize_then_hash_tool_use() {
    let s = "[tool_use: bash(toolu_abc)]";
    assert_eq!(doom_loop_hash(s), expected_hash(s));
}

#[test]
fn doom_loop_hash_matches_normalize_then_hash_mixed() {
    let s = "[tool_use: bash(id1)] result: [tool_result: id2]";
    assert_eq!(doom_loop_hash(s), expected_hash(s));
}

#[test]
fn doom_loop_hash_matches_normalize_then_hash_multiple_results() {
    let s = "[tool_result: id1]\nok\n[tool_result: id2]\nfail\n[tool_result: id3]\nok";
    assert_eq!(doom_loop_hash(s), expected_hash(s));
}

#[test]
fn doom_loop_hash_same_content_different_ids_equal() {
    let a = "[tool_result: toolu_abc]\nerror";
    let b = "[tool_result: toolu_xyz]\nerror";
    assert_eq!(doom_loop_hash(a), doom_loop_hash(b));
}

#[test]
fn doom_loop_hash_empty_string() {
    assert_eq!(doom_loop_hash(""), expected_hash(""));
}
