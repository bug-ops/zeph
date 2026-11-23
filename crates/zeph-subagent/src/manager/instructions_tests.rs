// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7157: the parent's instruction block is appended after the definition
//! body unless the definition opts out.
//!
//! Needs `#[cfg(test)] mod instructions_tests;` in `manager/mod.rs`. The config-level default
//! (`[agents] instructions`) is covered through the same switch once the plan names where the
//! spawn path reads it; see the handoff gap list.

use zeph_config::SubagentInstructions;

use super::SpawnContext;
use super::spawn::build_system_prompt_with_memory;
use crate::InheritedInstructions;
use crate::def::SubAgentDef;

fn ctx_with(text: &str) -> SpawnContext {
    SpawnContext {
        inherited_instructions: InheritedInstructions::new(text.to_owned()),
        ..SpawnContext::default()
    }
}

fn def(instructions: Option<SubagentInstructions>) -> SubAgentDef {
    let mut d = SubAgentDef::for_test("a");
    d.system_prompt = "DEF-BODY".to_owned();
    d.permissions.instructions = instructions;
    d
}

#[tokio::test]
async fn test_build_system_prompt_inherit_appends_instructions_after_definition_body() {
    let mut d = def(Some(SubagentInstructions::Inherit));
    let prompt = build_system_prompt_with_memory(&mut d, None, &ctx_with("RULES-BLOCK")).await;
    let body = prompt.find("DEF-BODY").expect("body present");
    let rules = prompt.find("RULES-BLOCK").expect("instructions present");
    assert!(body < rules, "instructions must follow the body: {prompt}");
}

#[tokio::test]
async fn test_build_system_prompt_none_omits_instructions() {
    let mut d = def(Some(SubagentInstructions::None));
    let prompt = build_system_prompt_with_memory(&mut d, None, &ctx_with("RULES-BLOCK")).await;
    assert!(prompt.contains("DEF-BODY"));
    assert!(!prompt.contains("RULES-BLOCK"), "{prompt}");
}

#[tokio::test]
async fn test_build_system_prompt_without_inherited_block_is_unchanged() {
    let mut d = def(Some(SubagentInstructions::Inherit));
    let prompt = build_system_prompt_with_memory(&mut d, None, &SpawnContext::default()).await;
    assert!(prompt.contains("DEF-BODY"));
    assert!(!prompt.contains("instructions"), "{prompt}");
}
