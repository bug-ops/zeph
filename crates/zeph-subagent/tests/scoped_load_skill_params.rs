// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7181: a scoped `load_skill` call without a usable `skill_name` is an
//! invalid-params error, and only a name outside the scope is a policy block.

mod common;

use common::{call, stub_box};
use zeph_subagent::filter::SkillScope;
use zeph_subagent::{FilteredToolExecutor, ToolPolicy};
use zeph_tools::executor::{ErasedToolExecutor, ToolError};

fn exec() -> FilteredToolExecutor {
    FilteredToolExecutor::new(
        stub_box(&["read", "load_skill"]),
        ToolPolicy::AllowList(vec!["read".into()]),
    )
    .with_skill_scope(["github".to_owned()].into_iter().collect::<SkillScope>())
}

#[tokio::test]
async fn test_scoped_load_skill_missing_skill_name_is_invalid_params() {
    let err = exec().execute_tool_call_erased(&call("load_skill")).await;
    match err {
        Err(ToolError::InvalidParams { message }) => {
            assert!(message.contains("skill_name"), "{message}");
        }
        other => panic!("expected InvalidParams, got {other:?}"),
    }
}

#[tokio::test]
async fn test_scoped_load_skill_non_string_skill_name_is_invalid_params() {
    let mut c = call("load_skill");
    c.params
        .insert("skill_name".to_owned(), serde_json::json!(42));
    let err = exec().execute_tool_call_erased(&c).await;
    assert!(
        matches!(err, Err(ToolError::InvalidParams { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn test_scoped_load_skill_name_outside_scope_stays_blocked() {
    let mut c = call("load_skill");
    c.params
        .insert("skill_name".to_owned(), serde_json::json!("git"));
    let err = exec().execute_tool_call_erased(&c).await;
    assert!(matches!(err, Err(ToolError::Blocked { .. })), "{err:?}");
}
