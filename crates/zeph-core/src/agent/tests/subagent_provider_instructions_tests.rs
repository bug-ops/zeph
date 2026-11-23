// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for #7153 (per-definition provider resolution) and the core wiring of #7157
//! (parent instruction block carried in `SpawnContext`).
//!
//! Needs `#[cfg(test)] mod subagent_provider_instructions_tests;` in `agent/tests/mod.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use zeph_config::{ModelSpec, ProviderEntry, ProviderKind, SubAgentConfig, SubagentInstructions};
use zeph_llm::provider::LlmProvider as _;
use zeph_subagent::{SubAgentDef, SubAgentError};

use crate::agent::Agent;
use crate::agent::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider,
};
use crate::agent::state::ProviderConfigSnapshot;
use crate::instructions::InstructionBlock;

fn snapshot() -> ProviderConfigSnapshot {
    ProviderConfigSnapshot {
        claude_api_key: None,
        openai_api_key: None,
        gemini_api_key: None,
        compatible_api_keys: HashMap::new(),
        llm_request_timeout_secs: 30,
        embedding_model: "nomic-embed-text".to_owned(),
        gonka_private_key: None,
        gonka_address: None,
        cocoon_access_hash: None,
    }
}

fn entry(name: &str) -> ProviderEntry {
    ProviderEntry {
        name: Some(name.to_owned()),
        provider_type: ProviderKind::Ollama,
        model: Some("qwen3:8b".to_owned()),
        ..ProviderEntry::default()
    }
}

fn agent() -> Agent<MockChannel> {
    Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
}

fn def_with(model: Option<ModelSpec>) -> SubAgentDef {
    let mut d = SubAgentDef::for_test("probe");
    d.model = model;
    d
}

#[test]
fn resolve_subagent_provider_none_and_inherit_clone_the_parent() {
    let agent = agent().with_provider_pool(vec![entry("quality")], snapshot());
    let parent = agent.provider.name().to_owned();
    for model in [None, Some(ModelSpec::Inherit)] {
        let p = agent
            .resolve_subagent_provider(&def_with(model))
            .expect("inherit resolves");
        assert_eq!(p.name(), parent);
    }
}

#[test]
fn resolve_subagent_provider_named_selects_pool_entry_case_insensitively() {
    let agent = agent().with_provider_pool(vec![entry("quality")], snapshot());
    let p = agent
        .resolve_subagent_provider(&def_with(Some(ModelSpec::Named("Quality".into()))))
        .expect("named resolves");
    assert_eq!(p.name(), "quality");
    assert_ne!(p.name(), agent.provider.name());
}

#[test]
fn resolve_subagent_provider_unknown_name_with_wired_pool_is_typed_error() {
    let agent = agent().with_provider_pool(vec![entry("quality")], snapshot());
    let err = agent
        .resolve_subagent_provider(&def_with(Some(ModelSpec::Named("opus".into()))))
        .expect_err("unknown provider");
    assert!(matches!(
        err,
        SubAgentError::UnknownProvider { ref def_name, ref provider }
            if def_name == "probe" && provider == "opus"
    ));
}

#[test]
fn resolve_subagent_provider_named_without_wired_pool_falls_back_to_parent() {
    let agent = agent();
    let p = agent
        .resolve_subagent_provider(&def_with(Some(ModelSpec::Named("quality".into()))))
        .expect("falls back");
    assert_eq!(p.name(), agent.provider.name());
}

fn blocks() -> Vec<InstructionBlock> {
    vec![InstructionBlock {
        source: PathBuf::from("/p/zeph.md"),
        content: "PINEAPPLE-RULE".to_owned(),
    }]
}

#[test]
fn build_spawn_context_carries_rendered_parent_instruction_block() {
    let agent = agent().with_instruction_blocks(blocks());
    let cfg = SubAgentConfig::default();
    assert_eq!(cfg.instructions, SubagentInstructions::Inherit);
    let ctx = agent.build_spawn_context(&cfg);
    let text = ctx.inherited_instructions.expect("block carried");
    assert_eq!(
        text.as_str(),
        crate::context::render_instruction_blocks(&blocks())
    );
    assert!(text.as_str().contains("PINEAPPLE-RULE"));
}

#[test]
fn build_spawn_context_omits_block_when_config_is_none_or_no_blocks() {
    let none_cfg = SubAgentConfig {
        instructions: SubagentInstructions::None,
        ..SubAgentConfig::default()
    };
    assert!(
        agent()
            .with_instruction_blocks(blocks())
            .build_spawn_context(&none_cfg)
            .inherited_instructions
            .is_none()
    );
    assert!(
        agent()
            .build_spawn_context(&SubAgentConfig::default())
            .inherited_instructions
            .is_none()
    );
}
