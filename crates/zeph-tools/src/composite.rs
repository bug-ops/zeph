// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Composite executor that chains two [`ToolExecutor`] implementations.

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

/// Chains two [`ToolExecutor`] implementations with first-match-wins dispatch.
///
/// For each method, `first` is tried first. If it returns `Ok(None)` (i.e. it does not
/// handle the input), `second` is tried. If `first` returns an `Err`, the error propagates
/// immediately without consulting `second`.
///
/// Use this to compose a chain of specialized executors at startup instead of a dynamic
/// `Vec<Box<dyn ...>>`. Nest multiple `CompositeExecutor`s to handle more than two backends.
///
/// Tool definitions from both executors are merged, with `first` taking precedence when
/// both define a tool with the same ID.
///
/// # Example
///
/// ```rust
/// use zeph_tools::{
///     CompositeExecutor, ShellExecutor, WebScrapeExecutor, ShellConfig, ScrapeConfig,
/// };
///
/// let shell = ShellExecutor::new(&ShellConfig::default());
/// let scrape = WebScrapeExecutor::new(&ScrapeConfig::default());
/// let executor = CompositeExecutor::new(shell, scrape);
/// // executor handles both bash blocks and scrape/fetch tool calls.
/// ```
#[derive(Debug)]
pub struct CompositeExecutor<A: ToolExecutor, B: ToolExecutor> {
    first: A,
    second: B,
}

impl<A: ToolExecutor, B: ToolExecutor> CompositeExecutor<A, B> {
    /// Create a new `CompositeExecutor` wrapping `first` and `second`.
    #[must_use]
    pub fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<A: ToolExecutor, B: ToolExecutor> ToolExecutor for CompositeExecutor<A, B> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        if let Some(output) = self.first.execute(response).await? {
            return Ok(Some(output));
        }
        self.second.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        if let Some(output) = self.first.execute_confirmed(response).await? {
            return Ok(Some(output));
        }
        self.second.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        let mut defs = self.first.tool_definitions();
        let seen: std::collections::HashSet<String> =
            defs.iter().map(|d| d.id.to_string()).collect();
        for def in self.second.tool_definitions() {
            if !seen.contains(def.id.as_ref()) {
                defs.push(def);
            }
        }
        defs
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if let Some(output) = self.first.execute_tool_call(call).await? {
            return Ok(Some(output));
        }
        self.second.execute_tool_call(call).await
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.first.is_tool_retryable(tool_id) || self.second.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.first.is_tool_speculatable(tool_id) || self.second.is_tool_speculatable(tool_id)
    }

    /// Forward the active skill's env injection to BOTH inner executors.
    ///
    /// The base [`ToolExecutor::set_skill_env`] is a no-op, so without this override the
    /// composition tree built in `agent_setup` silently swallows env injection — the
    /// underlying `ShellExecutor` never sees `GITHUB_TOKEN` etc. Each layer ignores the call
    /// if it does not own a `skill_env` slot; layers that do (e.g. `ShellExecutor`) update
    /// their state. See #3869.
    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.first.set_skill_env(env.clone());
        self.second.set_skill_env(env);
    }

    /// Forward the active skill's trust level to BOTH inner executors.
    ///
    /// Mirrors [`Self::set_skill_env`]: without this override, `TrustGateExecutor` never
    /// observes a non-`Trusted` level when composed under `CompositeExecutor`, leaving
    /// quarantine enforcement effectively bypassed. See #3869.
    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.first.set_effective_trust(level);
        self.second.set_effective_trust(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolName;

    #[derive(Debug)]
    struct MatchingExecutor;
    impl ToolExecutor for MatchingExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: ToolName::new("test"),
                summary: "matched".to_owned(),
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

    #[derive(Debug)]
    struct NoMatchExecutor;
    impl ToolExecutor for NoMatchExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    #[derive(Debug)]
    struct ErrorExecutor;
    impl ToolExecutor for ErrorExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Err(ToolError::Blocked {
                command: "test".to_owned(),
            })
        }
    }

    #[derive(Debug)]
    struct SecondExecutor;
    impl ToolExecutor for SecondExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: ToolName::new("test"),
                summary: "second".to_owned(),
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

    #[tokio::test]
    async fn first_matches_returns_first() {
        let composite = CompositeExecutor::new(MatchingExecutor, SecondExecutor);
        let result = composite.execute("anything").await.unwrap();
        assert_eq!(result.unwrap().summary, "matched");
    }

    #[tokio::test]
    async fn first_none_falls_through_to_second() {
        let composite = CompositeExecutor::new(NoMatchExecutor, SecondExecutor);
        let result = composite.execute("anything").await.unwrap();
        assert_eq!(result.unwrap().summary, "second");
    }

    #[tokio::test]
    async fn both_none_returns_none() {
        let composite = CompositeExecutor::new(NoMatchExecutor, NoMatchExecutor);
        let result = composite.execute("anything").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn first_error_propagates_without_trying_second() {
        let composite = CompositeExecutor::new(ErrorExecutor, SecondExecutor);
        let result = composite.execute("anything").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn second_error_propagates_when_first_none() {
        let composite = CompositeExecutor::new(NoMatchExecutor, ErrorExecutor);
        let result = composite.execute("anything").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn execute_confirmed_first_matches() {
        let composite = CompositeExecutor::new(MatchingExecutor, SecondExecutor);
        let result = composite.execute_confirmed("anything").await.unwrap();
        assert_eq!(result.unwrap().summary, "matched");
    }

    #[tokio::test]
    async fn execute_confirmed_falls_through() {
        let composite = CompositeExecutor::new(NoMatchExecutor, SecondExecutor);
        let result = composite.execute_confirmed("anything").await.unwrap();
        assert_eq!(result.unwrap().summary, "second");
    }

    #[test]
    fn composite_debug() {
        let composite = CompositeExecutor::new(MatchingExecutor, SecondExecutor);
        let debug = format!("{composite:?}");
        assert!(debug.contains("CompositeExecutor"));
    }

    #[derive(Debug)]
    struct FileToolExecutor;
    impl ToolExecutor for FileToolExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            if call.tool_id == "read" || call.tool_id == "write" {
                Ok(Some(ToolOutput {
                    tool_name: call.tool_id.clone(),
                    summary: "file_handler".to_owned(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            } else {
                Ok(None)
            }
        }
    }

    #[derive(Debug)]
    struct ShellToolExecutor;
    impl ToolExecutor for ShellToolExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            if call.tool_id == "bash" {
                Ok(Some(ToolOutput {
                    tool_name: ToolName::new("bash"),
                    summary: "shell_handler".to_owned(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                }))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn tool_call_routes_to_file_executor() {
        let composite = CompositeExecutor::new(FileToolExecutor, ShellToolExecutor);
        let call = ToolCall {
            tool_id: ToolName::new("read"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "file_handler");
    }

    #[tokio::test]
    async fn tool_call_routes_to_shell_executor() {
        let composite = CompositeExecutor::new(FileToolExecutor, ShellToolExecutor);
        let call = ToolCall {
            tool_id: ToolName::new("bash"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "shell_handler");
    }

    #[tokio::test]
    async fn tool_call_unhandled_returns_none() {
        let composite = CompositeExecutor::new(FileToolExecutor, ShellToolExecutor);
        let call = ToolCall {
            tool_id: ToolName::new("unknown"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    /// Regression test for #3869: state-mutating setters MUST reach both inner executors,
    /// even across nested compositions. Prior to the fix, `set_skill_env` and
    /// `set_effective_trust` fell through to the default no-op `ToolExecutor` impls and
    /// were silently dropped at the `CompositeExecutor` boundary — breaking skill secret
    /// env injection (`x-requires-secrets`) and quarantine trust enforcement.
    mod state_forwarding {
        use super::*;
        use crate::SkillTrustLevel;
        use std::sync::Mutex;

        #[derive(Debug, Default)]
        struct SpyExecutor {
            last_env: Mutex<Option<std::collections::HashMap<String, String>>>,
            last_trust: Mutex<Option<SkillTrustLevel>>,
        }
        impl ToolExecutor for SpyExecutor {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
                *self.last_env.lock().unwrap() = env;
            }
            fn set_effective_trust(&self, level: SkillTrustLevel) {
                *self.last_trust.lock().unwrap() = Some(level);
            }
        }

        #[test]
        fn set_skill_env_reaches_both_inner_executors_in_nested_composition() {
            // Mirrors the production wiring shape: a tree of CompositeExecutor with
            // multiple leaves. All leaves must observe the call.
            let leaf_a = SpyExecutor::default();
            let leaf_b = SpyExecutor::default();
            let leaf_c = SpyExecutor::default();
            let nested = CompositeExecutor::new(leaf_a, leaf_b);
            let outer = CompositeExecutor::new(nested, leaf_c);

            let mut env = std::collections::HashMap::new();
            env.insert("GITHUB_TOKEN".to_owned(), "tok".to_owned());
            outer.set_skill_env(Some(env.clone()));

            // first.first (leaf_a)
            assert_eq!(
                outer.first.first.last_env.lock().unwrap().as_ref(),
                Some(&env)
            );
            // first.second (leaf_b)
            assert_eq!(
                outer.first.second.last_env.lock().unwrap().as_ref(),
                Some(&env)
            );
            // second (leaf_c)
            assert_eq!(outer.second.last_env.lock().unwrap().as_ref(), Some(&env));
        }

        #[test]
        fn set_effective_trust_reaches_both_inner_executors_in_nested_composition() {
            let leaf_a = SpyExecutor::default();
            let leaf_b = SpyExecutor::default();
            let outer = CompositeExecutor::new(leaf_a, leaf_b);

            outer.set_effective_trust(SkillTrustLevel::Quarantined);

            assert_eq!(
                *outer.first.last_trust.lock().unwrap(),
                Some(SkillTrustLevel::Quarantined)
            );
            assert_eq!(
                *outer.second.last_trust.lock().unwrap(),
                Some(SkillTrustLevel::Quarantined)
            );
        }
    }
}
