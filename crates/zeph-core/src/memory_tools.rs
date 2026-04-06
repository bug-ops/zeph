// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::sync::Arc;

use zeph_memory::embedding_store::SearchFilter;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::types::ConversationId;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};
use zeph_tools::registry::{InvocationHint, ToolDef};

use zeph_sanitizer::memory_validation::MemoryWriteValidator;

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemorySearchParams {
    /// Natural language query to search memory for relevant past messages and facts.
    query: String,
    /// Maximum number of results to return (default: 5, max: 20).
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    5
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemorySaveParams {
    /// The content to save to long-term memory. Should be a concise, self-contained fact or note.
    content: String,
    /// Role label for the saved message (default: "assistant").
    #[serde(default = "default_role")]
    role: String,
}

fn default_role() -> String {
    "assistant".into()
}

pub struct MemoryToolExecutor {
    memory: Arc<SemanticMemory>,
    conversation_id: ConversationId,
    validator: MemoryWriteValidator,
}

impl MemoryToolExecutor {
    #[must_use]
    pub fn new(memory: Arc<SemanticMemory>, conversation_id: ConversationId) -> Self {
        Self {
            memory,
            conversation_id,
            validator: MemoryWriteValidator::new(
                zeph_sanitizer::memory_validation::MemoryWriteValidationConfig::default(),
            ),
        }
    }

    /// Create with a custom validator (used when security config is loaded).
    #[must_use]
    pub fn with_validator(
        memory: Arc<SemanticMemory>,
        conversation_id: ConversationId,
        validator: MemoryWriteValidator,
    ) -> Self {
        Self {
            memory,
            conversation_id,
            validator,
        }
    }
}

impl ToolExecutor for MemoryToolExecutor {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "memory_search".into(),
                description: "Search long-term memory for relevant past messages, facts, and session summaries. Use to recall facts, preferences, or information the user provided during this or previous conversations.\n\nParameters: query (string, required) - natural language search query; limit (integer, optional) - max results 1-20 (default: 5)\nReturns: ranked list of memory entries with similarity scores and timestamps\nErrors: Execution on database failure\nExample: {\"query\": \"user preference for output format\", \"limit\": 5}".into(),
                schema: schemars::schema_for!(MemorySearchParams),
                invocation: InvocationHint::ToolCall,
            },
            ToolDef {
                id: "memory_save".into(),
                description: "Save a fact or note to long-term memory for cross-session recall. Use sparingly for key decisions, user preferences, or critical context worth remembering across sessions.\n\nParameters: content (string, required) - concise, self-contained fact or note; role (string, optional) - message role label (default: \"assistant\")\nReturns: confirmation with saved entry ID\nErrors: Execution on database failure; InvalidParams if content is empty\nExample: {\"content\": \"User prefers JSON output over YAML\", \"role\": \"assistant\"}".into(),
                schema: schemars::schema_for!(MemorySaveParams),
                invocation: InvocationHint::ToolCall,
            },
        ]
    }

    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    #[allow(clippy::too_many_lines)] // two tools with validation, search, and multi-source aggregation
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        match call.tool_id.as_str() {
            "memory_search" => {
                let params: MemorySearchParams = deserialize_params(&call.params)?;
                let limit = params.limit.clamp(1, 20) as usize;

                let filter = Some(SearchFilter {
                    conversation_id: Some(self.conversation_id),
                    role: None,
                    category: None,
                });

                let recalled = self
                    .memory
                    .recall(&params.query, limit, filter)
                    .await
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                let key_facts = self
                    .memory
                    .search_key_facts(&params.query, limit)
                    .await
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                let summaries = self
                    .memory
                    .search_session_summaries(&params.query, limit, Some(self.conversation_id))
                    .await
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                let mut output = String::new();

                let _ = writeln!(output, "## Recalled Messages ({} results)", recalled.len());
                for r in &recalled {
                    let role = match r.message.role {
                        zeph_llm::provider::Role::User => "user",
                        zeph_llm::provider::Role::Assistant => "assistant",
                        zeph_llm::provider::Role::System => "system",
                    };
                    let content = r.message.content.trim();
                    let _ = writeln!(output, "[score: {:.2}] {role}: {content}", r.score);
                }

                let _ = writeln!(output);
                let _ = writeln!(output, "## Key Facts ({} results)", key_facts.len());
                for fact in &key_facts {
                    let _ = writeln!(output, "- {fact}");
                }

                let _ = writeln!(output);
                let _ = writeln!(output, "## Session Summaries ({} results)", summaries.len());
                for s in &summaries {
                    let _ = writeln!(
                        output,
                        "[conv #{}, score: {:.2}] {}",
                        s.conversation_id, s.score, s.summary_text
                    );
                }

                Ok(Some(ToolOutput {
                    tool_name: "memory_search".to_owned(),
                    summary: output,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: Some(zeph_tools::ClaimSource::Memory),
                }))
            }
            "memory_save" => {
                let params: MemorySaveParams = deserialize_params(&call.params)?;

                if params.content.is_empty() {
                    return Err(ToolError::InvalidParams {
                        message: "content must not be empty".to_owned(),
                    });
                }
                if params.content.len() > 4096 {
                    return Err(ToolError::InvalidParams {
                        message: "content exceeds maximum length of 4096 characters".to_owned(),
                    });
                }

                // Schema validation: check content before writing to memory.
                if let Err(e) = self.validator.validate_memory_save(&params.content) {
                    return Err(ToolError::InvalidParams {
                        message: format!("memory write rejected: {e}"),
                    });
                }

                let role = params.role.as_str();

                // Explicit user-directed saves bypass goal-conditioned scoring (goal_text = None).
                let message_id_opt = self
                    .memory
                    .remember(self.conversation_id, role, &params.content, None)
                    .await
                    .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

                let summary = match message_id_opt {
                    Some(message_id) => format!(
                        "Saved to memory (message_id: {message_id}, conversation: {}). Content will be available for future recall.",
                        self.conversation_id
                    ),
                    None => "Memory admission rejected: message did not meet quality threshold."
                        .to_owned(),
                };

                Ok(Some(ToolOutput {
                    tool_name: "memory_save".to_owned(),
                    summary,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: Some(zeph_tools::ClaimSource::Memory),
                }))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_memory::semantic::SemanticMemory;

    async fn make_memory() -> SemanticMemory {
        SemanticMemory::with_sqlite_backend(
            ":memory:",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
            0.7,
            0.3,
        )
        .await
        .unwrap()
    }

    fn make_executor(memory: SemanticMemory) -> MemoryToolExecutor {
        MemoryToolExecutor::new(Arc::new(memory), ConversationId(1))
    }

    #[tokio::test]
    async fn tool_definitions_returns_two_tools() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].id.as_ref(), "memory_search");
        assert_eq!(defs[1].id.as_ref(), "memory_save");
    }

    #[tokio::test]
    async fn execute_always_returns_none() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let result = executor.execute("any response").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_returns_none() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let call = ToolCall {
            tool_id: "unknown_tool".to_owned(),
            params: serde_json::Map::new(),
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn memory_search_returns_output() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let mut params = serde_json::Map::new();
        params.insert(
            "query".into(),
            serde_json::Value::String("test query".into()),
        );
        let call = ToolCall {
            tool_id: "memory_search".to_owned(),
            params,
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.tool_name, "memory_search");
        assert!(output.summary.contains("Recalled Messages"));
        assert!(output.summary.contains("Key Facts"));
        assert!(output.summary.contains("Session Summaries"));
    }

    #[tokio::test]
    async fn memory_save_stores_and_returns_confirmation() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        // Create conversation first
        let cid = sqlite.create_conversation().await.unwrap();
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid);

        let mut params = serde_json::Map::new();
        params.insert(
            "content".into(),
            serde_json::Value::String("User prefers dark mode".into()),
        );
        let call = ToolCall {
            tool_id: "memory_save".to_owned(),
            params,
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        let output = result.unwrap();
        assert!(output.summary.contains("Saved to memory"));
        assert!(output.summary.contains("message_id:"));
    }

    #[tokio::test]
    async fn memory_save_empty_content_returns_error() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let mut params = serde_json::Map::new();
        params.insert("content".into(), serde_json::Value::String(String::new()));
        let call = ToolCall {
            tool_id: "memory_save".to_owned(),
            params,
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn memory_save_oversized_content_returns_error() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let mut params = serde_json::Map::new();
        params.insert(
            "content".into(),
            serde_json::Value::String("x".repeat(4097)),
        );
        let call = ToolCall {
            tool_id: "memory_save".to_owned(),
            params,
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    /// `memory_search` description must mention user-provided facts so the model
    /// prefers it over `search_code` for recalling information from conversation (#2475).
    #[tokio::test]
    async fn memory_search_description_mentions_user_provided_facts() {
        let memory = make_memory().await;
        let executor = make_executor(memory);
        let defs = executor.tool_definitions();
        let memory_search = defs
            .iter()
            .find(|d| d.id.as_ref() == "memory_search")
            .unwrap();
        assert!(
            memory_search
                .description
                .contains("user provided during this or previous conversations"),
            "memory_search description must contain disambiguation phrase; got: {}",
            memory_search.description
        );
    }
}
