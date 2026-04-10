// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool and skill filtering for sub-agents.
//!
//! [`FilteredToolExecutor`] wraps any [`ErasedToolExecutor`] and enforces a [`ToolPolicy`]
//! plus an optional extra denylist on every tool invocation.
//!
//! [`PlanModeExecutor`] wraps any executor to allow catalog inspection while blocking all
//! execution — implementing the read-only planning permission mode.
//!
//! [`filter_skills`] applies glob-based include/exclude patterns against a skill registry.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use zeph_skills::loader::Skill;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::ToolCall;
use zeph_tools::executor::{ErasedToolExecutor, ToolError, ToolOutput, extract_fenced_blocks};
use zeph_tools::registry::{InvocationHint, ToolDef};

use super::def::{SkillFilter, ToolPolicy};
use super::error::SubAgentError;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Collect all fenced-block language tags from an executor's tool definitions.
fn collect_fenced_tags(executor: &dyn ErasedToolExecutor) -> Vec<&'static str> {
    executor
        .tool_definitions_erased()
        .into_iter()
        .filter_map(|def| match def.invocation {
            InvocationHint::FencedBlock(tag) => Some(tag),
            InvocationHint::ToolCall => None,
        })
        .collect()
}

// ── Tool filtering ────────────────────────────────────────────────────────────

/// Wraps an [`ErasedToolExecutor`] and enforces a [`ToolPolicy`] plus an optional
/// additional denylist (`disallowed`).
///
/// All calls are checked against the policy and the denylist before being forwarded
/// to the inner executor. The denylist is evaluated first — a tool in `disallowed`
/// is blocked even if `policy` would allow it (deny wins). Rejected calls return a
/// descriptive [`ToolError`].
pub struct FilteredToolExecutor {
    inner: Arc<dyn ErasedToolExecutor>,
    policy: ToolPolicy,
    disallowed: Vec<String>,
    /// Fenced-block language tags collected from `inner` at construction time.
    /// Used to detect actual fenced-block tool invocations in LLM responses.
    fenced_tags: Vec<&'static str>,
}

impl FilteredToolExecutor {
    /// Create a new filtered executor with the given policy and no additional denylist.
    ///
    /// Use [`with_disallowed`][Self::with_disallowed] when the agent definition also
    /// specifies `tools.except` entries.
    #[must_use]
    pub fn new(inner: Arc<dyn ErasedToolExecutor>, policy: ToolPolicy) -> Self {
        let fenced_tags = collect_fenced_tags(&*inner);
        Self {
            inner,
            policy,
            disallowed: Vec::new(),
            fenced_tags,
        }
    }

    /// Create a new filtered executor with an additional denylist.
    ///
    /// Tools in `disallowed` are blocked regardless of the base `policy`
    /// (deny wins over allow).
    #[must_use]
    pub fn with_disallowed(
        inner: Arc<dyn ErasedToolExecutor>,
        policy: ToolPolicy,
        disallowed: Vec<String>,
    ) -> Self {
        let fenced_tags = collect_fenced_tags(&*inner);
        Self {
            inner,
            policy,
            disallowed,
            fenced_tags,
        }
    }

    /// Return `true` if `response` contains at least one fenced block matching a registered tool.
    fn has_fenced_tool_invocation(&self, response: &str) -> bool {
        self.fenced_tags
            .iter()
            .any(|tag| !extract_fenced_blocks(response, tag).is_empty())
    }

    /// Check whether `tool_id` is allowed under the current policy and denylist.
    ///
    /// Matching is exact string equality. MCP compound tool IDs (e.g. `mcp__server__tool`)
    /// must be listed in full in `tools.except` — partial names or prefixes are not matched.
    fn is_allowed(&self, tool_id: &str) -> bool {
        if self.disallowed.iter().any(|t| t == tool_id) {
            return false;
        }
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
        response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        // Sub-agents must use structured tool calls (execute_tool_call_erased).
        // Fenced-block execution is disabled to prevent policy bypass (SEC-03).
        //
        // However, this method is also called for plain-text LLM responses that
        // contain markdown code fences unrelated to tool invocations. Returning
        // Err unconditionally causes the agent loop to treat every text response
        // as a failed tool call and exhaust all turns without producing output.
        //
        // Only block when the response actually contains a fenced block that
        // matches a registered fenced-block tool language tag.
        if self.has_fenced_tool_invocation(response) {
            tracing::warn!("sub-agent attempted fenced-block tool invocation — blocked by policy");
            return Box::pin(std::future::ready(Err(ToolError::Blocked {
                command: "fenced-block".into(),
            })));
        }
        Box::pin(std::future::ready(Ok(None)))
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        // Same policy as execute_erased: only block actual fenced-block invocations.
        if self.has_fenced_tool_invocation(response) {
            tracing::warn!(
                "sub-agent attempted confirmed fenced-block tool invocation — blocked by policy"
            );
            return Box::pin(std::future::ready(Err(ToolError::Blocked {
                command: "fenced-block".into(),
            })));
        }
        Box::pin(std::future::ready(Ok(None)))
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

    fn is_tool_retryable_erased(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable_erased(tool_id)
    }
}

// ── Plan mode executor ────────────────────────────────────────────────────────

/// Wraps an [`ErasedToolExecutor`] for `Plan` permission mode.
///
/// Exposes the real tool catalog via `tool_definitions_erased()` so the LLM can
/// reference existing tools in its plan, but blocks all execution methods with
/// [`ToolError::Blocked`]. This implements read-only planning: the agent sees what
/// tools exist but cannot invoke them.
pub struct PlanModeExecutor {
    inner: Arc<dyn ErasedToolExecutor>,
}

impl PlanModeExecutor {
    /// Wrap `inner` with plan-mode restrictions.
    #[must_use]
    pub fn new(inner: Arc<dyn ErasedToolExecutor>) -> Self {
        Self { inner }
    }
}

impl ErasedToolExecutor for PlanModeExecutor {
    fn execute_erased<'a>(
        &'a self,
        _response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(std::future::ready(Err(ToolError::Blocked {
            command: "plan_mode".into(),
        })))
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        _response: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(std::future::ready(Err(ToolError::Blocked {
            command: "plan_mode".into(),
        })))
    }

    fn tool_definitions_erased(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions_erased()
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        tracing::debug!(
            tool_id = %call.tool_id,
            "tool execution blocked in plan mode"
        );
        Box::pin(std::future::ready(Err(ToolError::Blocked {
            command: call.tool_id.clone(),
        })))
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
        false
    }
}

// ── Skill filtering ───────────────────────────────────────────────────────────

/// Filter skills from a registry according to a [`SkillFilter`].
///
/// Include patterns are glob-matched against skill names. If `include` is empty,
/// all skills pass (unless excluded). Exclude patterns always take precedence.
///
/// Supported glob syntax:
/// - `*` — wildcard matching any substring (e.g., `"git-*"`)
/// - Literal strings — exact match only
/// - `**` is **not** supported and returns [`SubAgentError::Invalid`]
///
/// # Errors
///
/// Returns [`SubAgentError::Invalid`] if any glob pattern is syntactically invalid.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_skills::registry::SkillRegistry;
/// use zeph_subagent::filter_skills;
/// use zeph_subagent::SkillFilter;
///
/// let registry = SkillRegistry::load(&[] as &[&str]);
/// let filter = SkillFilter { include: vec![], exclude: vec![] };
/// let skills = filter_skills(&registry, &filter).unwrap();
/// assert!(skills.is_empty());
/// ```
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
    #![allow(clippy::default_trait_access)]

    use super::*;
    use crate::def::ToolPolicy;

    // ── FilteredToolExecutor tests ─────────────────────────────────────────

    struct StubExecutor {
        tools: Vec<&'static str>,
    }

    /// Stub executor that exposes tools with `InvocationHint::FencedBlock(tag)`.
    struct StubFencedExecutor {
        tag: &'static str,
    }

    impl ErasedToolExecutor for StubFencedExecutor {
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
            use zeph_tools::registry::InvocationHint;
            vec![ToolDef {
                id: self.tag.into(),
                description: "fenced stub".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::FencedBlock(self.tag),
            }]
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
                locations: None,
                raw_response: None,
                claim_source: None,
            }));
            Box::pin(std::future::ready(result))
        }

        fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
            false
        }
    }

    fn fenced_stub_box(tag: &'static str) -> Arc<dyn ErasedToolExecutor> {
        Arc::new(StubFencedExecutor { tag })
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
                locations: None,
                raw_response: None,
                claim_source: None,
            }));
            Box::pin(std::future::ready(result))
        }

        fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
            false
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
            params: serde_json::Map::default(),
            caller_id: None,
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
            params: serde_json::Map::default(),
            caller_id: None,
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
            params: serde_json::Map::default(),
            caller_id: None,
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn inherit_all_permits_any_tool() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::default(),
            caller_id: None,
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
            params: serde_json::Map::default(),
            caller_id: None,
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

    // ── fenced-block detection tests (fix for #1432) ──────────────────────

    #[tokio::test]
    async fn fenced_block_matching_tag_is_blocked() {
        // Executor has a FencedBlock("bash") tool; response contains ```bash block.
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let res = exec.execute_erased("```bash\nls\n```").await;
        assert!(
            res.is_err(),
            "actual fenced-block invocation must be blocked"
        );
    }

    #[tokio::test]
    async fn fenced_block_matching_tag_confirmed_is_blocked() {
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let res = exec.execute_confirmed_erased("```bash\nls\n```").await;
        assert!(
            res.is_err(),
            "actual fenced-block invocation (confirmed) must be blocked"
        );
    }

    #[tokio::test]
    async fn no_fenced_tools_plain_text_returns_ok_none() {
        // No fenced-block tools registered → plain text must return Ok(None).
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let res = exec.execute_erased("This is a plain text response.").await;
        assert!(
            res.unwrap().is_none(),
            "plain text must not be treated as a tool call"
        );
    }

    #[tokio::test]
    async fn markdown_non_tool_fence_returns_ok_none() {
        // Response has a ```rust fence but no FencedBlock tool with tag "rust" is registered.
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let res = exec
            .execute_erased("Here is some code:\n```rust\nfn main() {}\n```")
            .await;
        assert!(
            res.unwrap().is_none(),
            "non-tool code fence must not trigger blocking"
        );
    }

    #[tokio::test]
    async fn no_fenced_tools_plain_text_confirmed_returns_ok_none() {
        let exec = FilteredToolExecutor::new(stub_box(&["shell"]), ToolPolicy::InheritAll);
        let res = exec
            .execute_confirmed_erased("Plain response without any fences.")
            .await;
        assert!(res.unwrap().is_none());
    }

    /// Regression test for #1432: fenced executor + plain text (no fences at all) must return
    /// Ok(None) so the agent loop can break. Previously this returned Err(Blocked)
    /// unconditionally, exhausting all sub-agent turns.
    #[tokio::test]
    async fn fenced_executor_plain_text_returns_ok_none() {
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let res = exec
            .execute_erased("Here is my analysis of the code. No shell commands needed.")
            .await;
        assert!(
            res.unwrap().is_none(),
            "plain text with fenced executor must not be treated as a tool call"
        );
    }

    /// Unclosed fence (no closing ```) must not trigger blocking — it is not an executable
    /// tool invocation. Verified by debugger as an intentional false-negative.
    #[tokio::test]
    async fn unclosed_fenced_block_returns_ok_none() {
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let res = exec.execute_erased("```bash\nls -la\n").await;
        assert!(
            res.unwrap().is_none(),
            "unclosed fenced block must not be treated as a tool invocation"
        );
    }

    /// Multiple fenced blocks where one matches a registered tag — must block.
    #[tokio::test]
    async fn multiple_fences_one_matching_tag_is_blocked() {
        let exec = FilteredToolExecutor::new(fenced_stub_box("bash"), ToolPolicy::InheritAll);
        let response = "Here is an example:\n```python\nprint('hello')\n```\nAnd the fix:\n```bash\nrm -rf /tmp/old\n```";
        let res = exec.execute_erased(response).await;
        assert!(
            res.is_err(),
            "response containing a matching fenced block must be blocked"
        );
    }

    // ── disallowed_tools (tools.except) tests ─────────────────────────────

    #[tokio::test]
    async fn disallowed_blocks_tool_from_allow_list() {
        let exec = FilteredToolExecutor::with_disallowed(
            stub_box(&["shell", "web"]),
            ToolPolicy::AllowList(vec!["shell".into(), "web".into()]),
            vec!["shell".into()],
        );
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::default(),
            caller_id: None,
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(
            res.is_err(),
            "disallowed tool must be blocked even if in allow list"
        );
    }

    #[tokio::test]
    async fn disallowed_allows_non_disallowed_tool() {
        let exec = FilteredToolExecutor::with_disallowed(
            stub_box(&["shell", "web"]),
            ToolPolicy::AllowList(vec!["shell".into(), "web".into()]),
            vec!["shell".into()],
        );
        let call = ToolCall {
            tool_id: "web".into(),
            params: serde_json::Map::default(),
            caller_id: None,
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(res.is_ok(), "non-disallowed tool must be allowed");
    }

    #[test]
    fn disallowed_empty_list_no_change() {
        let exec = FilteredToolExecutor::with_disallowed(
            stub_box(&["shell", "web"]),
            ToolPolicy::InheritAll,
            vec![],
        );
        let defs = exec.tool_definitions_erased();
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn tool_definitions_filters_disallowed_tools() {
        let exec = FilteredToolExecutor::with_disallowed(
            stub_box(&["shell", "web", "dangerous"]),
            ToolPolicy::InheritAll,
            vec!["dangerous".into()],
        );
        let defs = exec.tool_definitions_erased();
        assert_eq!(defs.len(), 2);
        assert!(!defs.iter().any(|d| d.id == "dangerous"));
    }

    // ── #1184: PlanModeExecutor + disallowed_tools catalog test ───────────

    #[test]
    fn plan_mode_with_disallowed_excludes_from_catalog() {
        // FilteredToolExecutor wrapping PlanModeExecutor must exclude disallowed tools from
        // tool_definitions_erased(), verifying that deny-list is enforced in plan mode catalog.
        let inner = Arc::new(PlanModeExecutor::new(stub_box(&["shell", "web"])));
        let exec = FilteredToolExecutor::with_disallowed(
            inner,
            ToolPolicy::InheritAll,
            vec!["shell".into()],
        );
        let defs = exec.tool_definitions_erased();
        assert!(
            !defs.iter().any(|d| d.id == "shell"),
            "shell must be excluded from catalog"
        );
        assert!(
            defs.iter().any(|d| d.id == "web"),
            "web must remain in catalog"
        );
    }

    // ── PlanModeExecutor tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn plan_mode_blocks_execute_erased() {
        let exec = PlanModeExecutor::new(stub_box(&["shell"]));
        let res = exec.execute_erased("response").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn plan_mode_blocks_execute_confirmed_erased() {
        let exec = PlanModeExecutor::new(stub_box(&["shell"]));
        let res = exec.execute_confirmed_erased("response").await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn plan_mode_blocks_tool_call() {
        let exec = PlanModeExecutor::new(stub_box(&["shell"]));
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::default(),
            caller_id: None,
        };
        let res = exec.execute_tool_call_erased(&call).await;
        assert!(res.is_err(), "plan mode must block all tool execution");
    }

    #[test]
    fn plan_mode_exposes_real_tool_definitions() {
        let exec = PlanModeExecutor::new(stub_box(&["shell", "web"]));
        let defs = exec.tool_definitions_erased();
        // Real tool catalog exposed — LLM can reference tools in its plan.
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d.id == "shell"));
        assert!(defs.iter().any(|d| d.id == "web"));
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
                if !pattern.contains("**")
                    && let Ok(p) = compile_glob(&pattern)
                {
                    let _ = glob_match(&p, &name);
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
