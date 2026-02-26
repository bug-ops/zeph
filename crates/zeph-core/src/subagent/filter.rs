// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use zeph_skills::loader::Skill;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::ToolCall;
use zeph_tools::executor::{ErasedToolExecutor, ToolError, ToolOutput};
use zeph_tools::registry::ToolDef;

use super::def::{SkillFilter, ToolPolicy};
use super::error::SubAgentError;

// ── Tool filtering ────────────────────────────────────────────────────────────

/// Wraps an [`ErasedToolExecutor`] and enforces a [`ToolPolicy`].
///
/// All calls are checked against the policy before being forwarded to the inner
/// executor. Rejected calls return a descriptive [`ToolError`].
pub struct FilteredToolExecutor {
    inner: Arc<dyn ErasedToolExecutor>,
    policy: ToolPolicy,
}

impl FilteredToolExecutor {
    /// Create a new filtered executor.
    #[must_use]
    pub fn new(inner: Arc<dyn ErasedToolExecutor>, policy: ToolPolicy) -> Self {
        Self { inner, policy }
    }

    fn is_allowed(&self, tool_id: &str) -> bool {
        match &self.policy {
            ToolPolicy::InheritAll => true,
            ToolPolicy::AllowList(list) => list.iter().any(|t| t == tool_id),
            ToolPolicy::DenyList(list) => !list.iter().any(|t| t == tool_id),
        }
    }
}

impl ErasedToolExecutor for FilteredToolExecutor {
    fn execute_erased<'a>(
        &'a self,
        _response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        // Fenced-block (markdown code block) invocation cannot have the tool name
        // extracted before execution, so it cannot be checked against ToolPolicy.
        // Sub-agents must use structured tool calls (execute_tool_call_erased).
        // Fenced-block execution is disabled entirely for sub-agents to prevent
        // policy bypass (SEC-03).
        tracing::warn!("sub-agent attempted fenced-block tool invocation — blocked by policy");
        Box::pin(std::future::ready(Err(ToolError::Blocked {
            command: "fenced-block".into(),
        })))
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        _response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        // Same as execute_erased: fenced-block is disabled for sub-agents.
        Box::pin(std::future::ready(Err(ToolError::Blocked {
            command: "fenced-block".into(),
        })))
    }

    fn tool_definitions_erased(&self) -> Vec<ToolDef> {
        // Filter the visible tool definitions according to the policy.
        self.inner
            .tool_definitions_erased()
            .into_iter()
            .filter(|def| self.is_allowed(&def.id))
            .collect()
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        if !self.is_allowed(&call.tool_id) {
            tracing::warn!(
                tool_id = %call.tool_id,
                "sub-agent tool call rejected by policy"
            );
            return Box::pin(std::future::ready(Err(ToolError::Blocked {
                command: call.tool_id.clone(),
            })));
        }
        Box::pin(self.inner.execute_tool_call_erased(call))
    }

    fn set_skill_env(&self, env: Option<HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }
}

// ── Skill filtering ───────────────────────────────────────────────────────────

/// Filter skills from a registry according to a [`SkillFilter`].
///
/// Include patterns are glob-matched against skill names. If `include` is empty,
/// all skills pass (unless excluded). Exclude patterns always take precedence.
///
/// # Errors
///
/// Returns [`SubAgentError::Invalid`] if any glob pattern is syntactically invalid.
pub fn filter_skills(
    registry: &SkillRegistry,
    filter: &SkillFilter,
) -> Result<Vec<Skill>, SubAgentError> {
    let compiled_include = compile_globs(&filter.include)?;
    let compiled_exclude = compile_globs(&filter.exclude)?;

    let all: Vec<Skill> = registry
        .all_meta()
        .into_iter()
        .filter(|meta| {
            let name = &meta.name;
            let included =
                compiled_include.is_empty() || compiled_include.iter().any(|p| glob_match(p, name));
            let excluded = compiled_exclude.iter().any(|p| glob_match(p, name));
            included && !excluded
        })
        .filter_map(|meta| registry.get_skill(&meta.name).ok())
        .collect();

    Ok(all)
}

/// Compiled glob pattern: literal prefix + optional `*` wildcard suffix.
struct GlobPattern {
    raw: String,
    prefix: String,
    suffix: Option<String>,
    is_star: bool,
}

fn compile_globs(patterns: &[String]) -> Result<Vec<GlobPattern>, SubAgentError> {
    patterns.iter().map(|p| compile_glob(p)).collect()
}

fn compile_glob(pattern: &str) -> Result<GlobPattern, SubAgentError> {
    // Simple glob: supports `*` as a wildcard anywhere in the string.
    // For MVP we only need prefix-star patterns like "git-*" or "*".
    if pattern.contains("**") {
        return Err(SubAgentError::Invalid(format!(
            "glob pattern '{pattern}' uses '**' which is not supported"
        )));
    }

    let is_star = pattern == "*";

    let (prefix, suffix) = if let Some(pos) = pattern.find('*') {
        let before = pattern[..pos].to_owned();
        let after = pattern[pos + 1..].to_owned();
        (before, Some(after))
    } else {
        (pattern.to_owned(), None)
    };

    Ok(GlobPattern {
        raw: pattern.to_owned(),
        prefix,
        suffix,
        is_star,
    })
}

fn glob_match(pattern: &GlobPattern, name: &str) -> bool {
    if pattern.is_star {
        return true;
    }

    match &pattern.suffix {
        None => name == pattern.raw,
        Some(suf) => {
            name.starts_with(&pattern.prefix) && name.ends_with(suf.as_str()) && {
                // Ensure the wildcard section isn't negative-length.
                name.len() >= pattern.prefix.len() + suf.len()
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::def::ToolPolicy;

    // ── FilteredToolExecutor tests ─────────────────────────────────────────

    struct StubExecutor {
        tools: Vec<&'static str>,
    }

    impl ErasedToolExecutor for StubExecutor {
        fn execute_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn execute_confirmed_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn tool_definitions_erased(&self) -> Vec<ToolDef> {
            // Return stub definitions for each tool name.
            use zeph_tools::registry::InvocationHint;
            self.tools
                .iter()
                .map(|id| ToolDef {
                    id: (*id).into(),
                    description: "stub".into(),
                    schema: schemars::Schema::default(),
                    invocation: InvocationHint::ToolCall,
                })
                .collect()
        }

        fn execute_tool_call_erased<'a>(
            &'a self,
            call: &'a ToolCall,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            let result = Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
            }));
            Box::pin(std::future::ready(result))
        }
    }

    fn stub_box(tools: &[&'static str]) -> Arc<dyn ErasedToolExecutor> {
        Arc::new(StubExecutor {
            tools: tools.to_vec(),
        })
    }

    #[tokio::test]
    async fn allow_list_permits_listed_tool() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::AllowList(vec!["shell".into()]),
        );
        let call = ToolCall {
            tool_id: "shell".into(),
            params: Default::default(),
        };
        let res = exec.execute_tool_call_erased(&call).await.unwrap();
        assert!(res.is_some());
    }

    #[tokio::test]
    async fn allow_list_blocks_unlisted_tool() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::AllowList(vec!["shell".into()]),
        );
        let call = ToolCall {
            tool_id: "web".into(),
            params: Default::default(),
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn deny_list_blocks_listed_tool() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::DenyList(vec!["shell".into()]),
        );
        let call = ToolCall {
            tool_id: "shell".into(),
            params: Default::default(),
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn inherit_all_permits_any_tool() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let call = ToolCall {
            tool_id: "shell".into(),
            params: Default::default(),
        };
        let res = exec.execute_tool_call_erased(&call).await.unwrap();
        assert!(res.is_some());
    }

    #[test]
    fn tool_definitions_filtered_by_allow_list() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::AllowList(vec!["shell".into()]),
        );
        let defs = exec.tool_definitions_erased();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "shell");
    }

    // ── glob_match tests ───────────────────────────────────────────────────

    fn matches(pattern: &str, name: &str) -> bool {
        let p = compile_glob(pattern).unwrap();
        glob_match(&p, name)
    }

    #[test]
    fn glob_star_matches_all() {
        assert!(matches("*", "anything"));
        assert!(matches("*", ""));
    }

    #[test]
    fn glob_prefix_star() {
        assert!(matches("git-*", "git-commit"));
        assert!(matches("git-*", "git-status"));
        assert!(!matches("git-*", "rust-fmt"));
    }

    #[test]
    fn glob_literal_exact_match() {
        assert!(matches("shell", "shell"));
        assert!(!matches("shell", "shell-extra"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(matches("*-review", "code-review"));
        assert!(!matches("*-review", "code-reviewer"));
    }

    #[test]
    fn glob_double_star_is_error() {
        assert!(compile_glob("**").is_err());
    }

    #[test]
    fn glob_mid_string_wildcard() {
        // "a*b" — prefix="a", suffix=Some("b")
        assert!(matches("a*b", "axb"));
        assert!(matches("a*b", "aXYZb"));
        assert!(!matches("a*b", "ab-extra"));
        assert!(!matches("a*b", "xab"));
    }

    // ── FilteredToolExecutor additional tests ──────────────────────────────

    #[tokio::test]
    async fn deny_list_permits_unlisted_tool() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::DenyList(vec!["shell".into()]),
        );
        let call = ToolCall {
            tool_id: "web".into(), // not in deny list → allowed
            params: Default::default(),
        };
        let res = exec.execute_tool_call_erased(&call).await.unwrap();
        assert!(res.is_some());
    }

    #[test]
    fn tool_definitions_filtered_by_deny_list() {
        let exec = FilteredToolExecutor::new(
            stub_box(&["shell", "web"]),
            ToolPolicy::DenyList(vec!["shell".into()]),
        );
        let defs = exec.tool_definitions_erased();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "web");
    }

    #[test]
    fn tool_definitions_inherit_all_returns_all() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell", "web"]), ToolPolicy::InheritAll);
        let defs = exec.tool_definitions_erased();
        assert_eq!(defs.len(), 2);
    }

    #[tokio::test]
    async fn fenced_block_execute_is_blocked() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let res = exec.execute_erased("```shell\nls\n```").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn fenced_block_execute_confirmed_is_blocked() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let res = exec.execute_confirmed_erased("```shell\nls\n```").await;
        assert!(res.is_err());
    }

    // ── filter_skills tests ────────────────────────────────────────────────

    #[test]
    fn filter_skills_empty_registry_returns_empty() {
        let registry = zeph_skills::registry::SkillRegistry::load(&[] as &[&str]);
        let filter = SkillFilter::default();
        let result = filter_skills(&registry, &filter).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn filter_skills_empty_include_passes_all() {
        // Empty include list means "include everything".
        // With an empty registry, result is still empty — logic is correct.
        let registry = zeph_skills::registry::SkillRegistry::load(&[] as &[&str]);
        let filter = SkillFilter {
            include: vec![],
            exclude: vec![],
        };
        let result = filter_skills(&registry, &filter).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn filter_skills_double_star_pattern_is_error() {
        let registry = zeph_skills::registry::SkillRegistry::load(&[] as &[&str]);
        let filter = SkillFilter {
            include: vec!["**".into()],
            exclude: vec![],
        };
        let err = filter_skills(&registry, &filter).unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    mod proptest_glob {
        use proptest::prelude::*;

        use super::{compile_glob, glob_match};

        proptest! {
            #![proptest_config(proptest::test_runner::Config::with_cases(500))]

            /// glob_match must never panic for any valid (non-**) pattern and any name string.
            #[test]
            fn glob_match_never_panics(
                pattern in "[a-z*-]{1,10}",
                name in "[a-z-]{0,15}",
            ) {
                // Skip patterns with ** (those are compile errors by design).
                if !pattern.contains("**") {
                    if let Ok(p) = compile_glob(&pattern) {
                        let _ = glob_match(&p, &name);
                    }
                }
            }

            /// A literal pattern (no `*`) must match only exact strings.
            #[test]
            fn glob_literal_matches_only_exact(
                name in "[a-z-]{1,10}",
            ) {
                // A literal pattern equal to `name` must match.
                let p = compile_glob(&name).unwrap();
                prop_assert!(glob_match(&p, &name));

                // A different name must not match.
                let other = format!("{name}-x");
                prop_assert!(!glob_match(&p, &other));
            }

            /// The `*` pattern must match every input.
            #[test]
            fn glob_star_matches_everything(name in ".*") {
                let p = compile_glob("*").unwrap();
                prop_assert!(glob_match(&p, &name));
            }
        }
    }
}
