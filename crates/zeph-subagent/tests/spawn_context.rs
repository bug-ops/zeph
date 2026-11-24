// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for the sub-agent halves of #7153 (provider recorded), #7157 (inherited
//! instructions carrier) and #7159 (implicit, scoped `load_skill`).

mod common;

use common::{call, stub_box};
use zeph_common::trust_level::TranscriptTrustSchema;
use zeph_subagent::filter::implicit_load_skill_scope;
use zeph_subagent::{
    FilteredToolExecutor, InheritedInstructions, SkillScope, SpawnContext, SubAgentDef,
    SubAgentError, SubAgentState, ToolPolicy, TranscriptMeta,
};
use zeph_tools::executor::{ErasedToolExecutor, ToolError};

fn meta(provider: Option<&str>) -> TranscriptMeta {
    TranscriptMeta {
        agent_id: "id".to_owned(),
        agent_name: "bot".to_owned(),
        def_name: "bot".to_owned(),
        status: SubAgentState::Completed,
        started_at: "2026-01-01T00:00:00Z".to_owned(),
        finished_at: None,
        resumed_from: None,
        turns_used: 1,
        trust_schema: TranscriptTrustSchema::PerResult,
        inherited_trust: None,
        provider: provider.map(str::to_owned),
    }
}

#[test]
fn test_transcript_meta_provider_roundtrips() {
    let json = serde_json::to_string(&meta(Some("quality"))).expect("serialize");
    assert!(json.contains("\"provider\":\"quality\""), "{json}");
    let back: TranscriptMeta = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.provider.as_deref(), Some("quality"));
}

#[test]
fn test_transcript_meta_provider_absent_is_skipped_and_legacy_sidecar_loads() {
    let json = serde_json::to_string(&meta(None)).expect("serialize");
    assert!(!json.contains("provider"), "{json}");
    let back: TranscriptMeta = serde_json::from_str(&json).expect("legacy sidecar");
    assert_eq!(back.provider, None);
}

#[test]
fn test_unknown_provider_error_names_definition_and_provider() {
    let err = SubAgentError::UnknownProvider {
        def_name: "probe".to_owned(),
        provider: "opus".to_owned(),
    };
    let text = err.to_string();
    assert!(text.contains("probe") && text.contains("opus"), "{text}");
}

#[test]
fn test_inherited_instructions_rejects_empty_text() {
    assert!(InheritedInstructions::new(String::new()).is_none());
    assert!(InheritedInstructions::new("rule: be brief".to_owned()).is_some());
}

#[test]
fn test_spawn_context_default_carries_no_instructions_and_empty_scope() {
    let ctx = SpawnContext::default();
    assert!(ctx.inherited_instructions.is_none());
    assert!(ctx.skill_scope.is_empty());
}

fn def_with(tools: ToolPolicy, include: &[&str]) -> SubAgentDef {
    let mut d = SubAgentDef::for_test("d");
    d.tools = tools;
    d.skills.include = include.iter().map(|s| (*s).to_owned()).collect();
    d
}

fn matched() -> Vec<String> {
    vec!["github".to_owned()]
}

#[test]
fn test_implicit_scope_allow_list_without_load_skill_and_include_is_scoped() {
    let def = def_with(ToolPolicy::AllowList(vec!["read".into()]), &["github"]);
    let scope = implicit_load_skill_scope(&def, &matched()).expect("granted");
    assert!(scope.contains("github"));
    assert!(!scope.contains("git"));
}

#[test]
fn test_implicit_scope_allow_list_without_include_is_not_granted() {
    let def = def_with(ToolPolicy::AllowList(vec!["read".into()]), &[]);
    assert!(implicit_load_skill_scope(&def, &[]).is_none());
}

#[test]
fn test_implicit_scope_explicit_load_skill_or_open_policies_keep_unscoped_behaviour() {
    for tools in [
        ToolPolicy::AllowList(vec!["read".into(), "load_skill".into()]),
        ToolPolicy::InheritAll,
        ToolPolicy::DenyList(vec!["bash".into()]),
    ] {
        let def = def_with(tools, &["github"]);
        assert!(implicit_load_skill_scope(&def, &matched()).is_none());
    }
}

#[test]
fn test_implicit_scope_denied_or_excepted_load_skill_is_not_granted() {
    let mut def = def_with(ToolPolicy::AllowList(vec!["read".into()]), &["github"]);
    def.disallowed_tools = vec!["load_skill".into()];
    assert!(implicit_load_skill_scope(&def, &matched()).is_none());
}

fn scoped_executor(policy: ToolPolicy, disallowed: Vec<String>) -> FilteredToolExecutor {
    FilteredToolExecutor::with_disallowed(stub_box(&["read", "load_skill"]), policy, disallowed)
        .with_skill_scope(["github".to_owned()].into_iter().collect::<SkillScope>())
}

#[tokio::test]
async fn test_scoped_load_skill_forwards_included_skill_and_blocks_others() {
    let exec = scoped_executor(ToolPolicy::AllowList(vec!["read".into()]), vec![]);
    let ids: Vec<String> = exec
        .tool_definitions_erased()
        .into_iter()
        .map(|d| d.id.to_string())
        .collect();
    assert!(ids.contains(&"load_skill".to_owned()), "{ids:?}");

    let ok = call_with("load_skill", "skill_name", "github");
    assert!(exec.execute_tool_call_erased(&ok).await.is_ok());
    let other = call_with("load_skill", "skill_name", "git");
    assert!(matches!(
        exec.execute_tool_call_erased(&other).await,
        Err(ToolError::Blocked { .. })
    ));
    let missing = call("load_skill");
    assert!(matches!(
        exec.execute_tool_call_erased(&missing).await,
        Err(ToolError::InvalidParams { .. })
    ));
}

#[test]
fn test_scoped_load_skill_hidden_when_excepted() {
    let exec = scoped_executor(
        ToolPolicy::AllowList(vec!["read".into()]),
        vec!["load_skill".into()],
    );
    assert!(
        exec.tool_definitions_erased()
            .iter()
            .all(|d| d.id != "load_skill")
    );
}

#[test]
fn test_load_skill_hidden_without_scope_for_allow_list_without_it() {
    let exec = FilteredToolExecutor::new(
        stub_box(&["read", "load_skill"]),
        ToolPolicy::AllowList(vec!["read".into()]),
    );
    assert!(
        exec.tool_definitions_erased()
            .iter()
            .all(|d| d.id != "load_skill")
    );
}

/// Structured tool call for `tool_id` with a single string param.
fn call_with(tool_id: &str, key: &str, value: &str) -> zeph_tools::ToolCall {
    let mut c = call(tool_id);
    c.params
        .insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
    c
}
