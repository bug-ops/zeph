use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

/// Chains two `ToolExecutor` implementations with first-match-wins dispatch.
///
/// Tries `first`, falls through to `second` if it returns `Ok(None)`.
/// Errors from `first` propagate immediately without trying `second`.
#[derive(Debug)]
pub struct CompositeExecutor<A: ToolExecutor, B: ToolExecutor> {
    first: A,
    second: B,
}

impl<A: ToolExecutor, B: ToolExecutor> CompositeExecutor<A, B> {
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
        defs.extend(self.second.tool_definitions());
        defs
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if let Some(output) = self.first.execute_tool_call(call).await? {
            return Ok(Some(output));
        }
        self.second.execute_tool_call(call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MatchingExecutor;
    impl ToolExecutor for MatchingExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: "test".to_owned(),
                summary: "matched".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
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
                tool_name: "test".to_owned(),
                summary: "second".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
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
                    tool_name: "bash".to_owned(),
                    summary: "shell_handler".to_owned(),
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
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
            tool_id: "read".to_owned(),
            params: std::collections::HashMap::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "file_handler");
    }

    #[tokio::test]
    async fn tool_call_routes_to_shell_executor() {
        let composite = CompositeExecutor::new(FileToolExecutor, ShellToolExecutor);
        let call = ToolCall {
            tool_id: "bash".to_owned(),
            params: std::collections::HashMap::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary, "shell_handler");
    }

    #[tokio::test]
    async fn tool_call_unhandled_returns_none() {
        let composite = CompositeExecutor::new(FileToolExecutor, ShellToolExecutor);
        let call = ToolCall {
            tool_id: "unknown".to_owned(),
            params: std::collections::HashMap::new(),
        };
        let result = composite.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }
}
