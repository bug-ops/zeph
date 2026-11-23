// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7157: per-definition `permissions.instructions` override.

use zeph_config::SubagentInstructions;
use zeph_subagent::SubAgentDef;

fn parse(permissions: &str) -> Result<SubAgentDef, zeph_subagent::SubAgentError> {
    SubAgentDef::parse(&format!(
        "---\nname: a\ndescription: b\npermissions:\n  {permissions}\n---\n\nbody\n"
    ))
}

#[test]
fn test_permissions_instructions_none_and_inherit_parse() {
    assert_eq!(
        parse("instructions: none")
            .expect("none")
            .permissions
            .instructions,
        Some(SubagentInstructions::None)
    );
    assert_eq!(
        parse("instructions: inherit")
            .expect("inherit")
            .permissions
            .instructions,
        Some(SubagentInstructions::Inherit)
    );
}

#[test]
fn test_permissions_instructions_absent_is_none_option() {
    let def = SubAgentDef::parse("---\nname: a\ndescription: b\n---\n\nbody\n").expect("parse");
    assert_eq!(def.permissions.instructions, None);
}

#[test]
fn test_permissions_instructions_rejects_bool_and_unknown_value() {
    assert!(parse("instructions: true").is_err());
    assert!(parse("instructions: everything").is_err());
}
