// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::{Arc, RwLock};

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params, truncate_tool_output,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadSkillParams {
    /// Name of the skill to load (from `<other_skills>` catalog).
    pub skill_name: String,
}

/// Tool executor that loads a full skill body by name from the shared registry.
#[derive(Clone, Debug)]
pub struct SkillLoaderExecutor {
    registry: Arc<RwLock<SkillRegistry>>,
}

impl SkillLoaderExecutor {
    #[must_use]
    pub fn new(registry: Arc<RwLock<SkillRegistry>>) -> Self {
        Self { registry }
    }
}

impl ToolExecutor for SkillLoaderExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "load_skill".into(),
            description: "Load the full body of a skill by name when you see a relevant entry in the <other_skills> catalog.\n\nParameters: name (string, required) - exact skill name from the <other_skills> catalog\nReturns: complete skill instructions (SKILL.md body), or error if skill not found\nErrors: InvalidParams if name is empty; Execution if skill not found in registry\nExample: {\"name\": \"code-review\"}".into(),
            schema: schemars::schema_for!(LoadSkillParams),
            invocation: InvocationHint::ToolCall,
        }]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "load_skill" {
            return Ok(None);
        }
        let params: LoadSkillParams = deserialize_params(&call.params)?;
        let skill_name: String = params.skill_name.chars().take(128).collect();
        let body = {
            let guard = self.registry.read().map_err(|_| ToolError::InvalidParams {
                message: "registry lock poisoned".into(),
            })?;
            guard.get_body(&skill_name).map(str::to_owned)
        };

        let summary = match body {
            Ok(b) => truncate_tool_output(&b),
            Err(_) => format!("skill not found: {skill_name}"),
        };

        Ok(Some(ToolOutput {
            tool_name: "load_skill".to_owned(),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn make_registry_with_skill(dir: &Path, name: &str, body: &str) -> SkillRegistry {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\n{body}"),
        )
        .unwrap();
        SkillRegistry::load(&[dir.to_path_buf()])
    }

    #[tokio::test]
    async fn load_existing_skill_returns_body() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            make_registry_with_skill(dir.path(), "git-commit", "## Instructions\nDo git stuff");
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::json!({"skill_name": "git-commit"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("## Instructions"));
        assert!(result.summary.contains("Do git stuff"));
    }

    #[tokio::test]
    async fn load_nonexistent_skill_returns_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::json!({"skill_name": "nonexistent"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("skill not found"));
        assert!(result.summary.contains("nonexistent"));
    }

    #[test]
    fn tool_definitions_returns_load_skill() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "load_skill");
    }

    #[tokio::test]
    async fn execute_returns_none_for_wrong_tool_id() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "bash".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn long_skill_body_is_truncated() {
        use zeph_tools::executor::MAX_TOOL_OUTPUT_CHARS;
        let dir = tempfile::tempdir().unwrap();
        let long_body = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 1000);
        let registry = make_registry_with_skill(dir.path(), "big-skill", &long_body);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::json!({"skill_name": "big-skill"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("truncated"));
        assert!(result.summary.len() < long_body.len() + 200);
    }

    #[tokio::test]
    async fn empty_registry_returns_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::json!({"skill_name": "any"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("skill not found"));
    }

    // GAP-1: direct execute() always returns None
    #[tokio::test]
    async fn execute_always_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let result = executor.execute("any response text").await.unwrap();
        assert!(result.is_none());
    }

    // GAP-2: concurrent reads all succeed
    #[tokio::test]
    async fn concurrent_execute_tool_call_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            make_registry_with_skill(dir.path(), "shared-skill", "## Concurrent test body");
        let executor = Arc::new(SkillLoaderExecutor::new(Arc::new(RwLock::new(registry))));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ex = Arc::clone(&executor);
                tokio::spawn(async move {
                    let call = ToolCall {
                        tool_id: "load_skill".to_owned(),
                        params: serde_json::json!({"skill_name": "shared-skill"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    };
                    ex.execute_tool_call(&call).await
                })
            })
            .collect();

        for h in handles {
            let result = h.await.unwrap().unwrap().unwrap();
            assert!(result.summary.contains("## Concurrent test body"));
        }
    }

    // GAP-3: empty skill_name returns "not found"
    #[tokio::test]
    async fn empty_skill_name_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::json!({"skill_name": ""})
                .as_object()
                .unwrap()
                .clone(),
        };
        let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(result.summary.contains("skill not found"));
    }

    // GAP-4: missing skill_name field returns ToolError from deserialize_params
    #[tokio::test]
    async fn missing_skill_name_field_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = SkillLoaderExecutor::new(Arc::new(RwLock::new(registry)));
        let call = ToolCall {
            tool_id: "load_skill".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }
}
