// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_memory::sqlite::SqliteStore;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};
use zeph_tools::registry::{InvocationHint, ToolDef};

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ReadOverflowParams {
    /// The bare UUID from the overflow notice. The `overflow:` prefix is accepted but stripped automatically.
    id: String,
}

pub struct OverflowToolExecutor {
    sqlite: Arc<SqliteStore>,
    conversation_id: Option<i64>,
}

impl OverflowToolExecutor {
    pub const TOOL_NAME: &'static str = "read_overflow";

    #[must_use]
    pub fn new(sqlite: Arc<SqliteStore>) -> Self {
        Self {
            sqlite,
            conversation_id: None,
        }
    }

    #[must_use]
    pub fn with_conversation(mut self, conversation_id: i64) -> Self {
        self.conversation_id = Some(conversation_id);
        self
    }
}

impl ToolExecutor for OverflowToolExecutor {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: Self::TOOL_NAME.into(),
            description: "Retrieve the full content of a tool output that was truncated due to \
                size. Use when a previous tool result contains an overflow notice. \
                Parameters: id (string, required) — the bare UUID from the notice \
                (e.g. '550e8400-e29b-41d4-a716-446655440000'). \
                Returns: full original tool output text. Errors: NotFound if the \
                overflow entry has expired or does not exist."
                .into(),
            schema: schemars::schema_for!(ReadOverflowParams),
            invocation: InvocationHint::ToolCall,
        }]
    }

    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != Self::TOOL_NAME {
            return Ok(None);
        }
        let params: ReadOverflowParams = deserialize_params(&call.params)?;

        let id = params.id.strip_prefix("overflow:").unwrap_or(&params.id);

        if uuid::Uuid::parse_str(id).is_err() {
            return Err(ToolError::InvalidParams {
                message: "id must be a valid UUID".to_owned(),
            });
        }

        let Some(conv_id) = self.conversation_id else {
            return Err(ToolError::Execution(std::io::Error::other(
                "overflow entry not found or expired",
            )));
        };

        match self.sqlite.load_overflow(id, conv_id).await {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Ok(Some(ToolOutput {
                    tool_name: Self::TOOL_NAME.to_owned(),
                    summary: text,
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
            Ok(None) => Err(ToolError::Execution(std::io::Error::other(
                "overflow entry not found or expired",
            ))),
            Err(e) => Err(ToolError::Execution(std::io::Error::other(format!(
                "failed to load overflow: {e}"
            )))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_memory::sqlite::SqliteStore;

    async fn make_store_with_conv() -> (Arc<SqliteStore>, i64) {
        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
        (Arc::new(store), cid.0)
    }

    fn make_call(id: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("id".into(), serde_json::Value::String(id.to_owned()));
        ToolCall {
            tool_id: "read_overflow".to_owned(),
            params,
        }
    }

    #[tokio::test]
    async fn tool_definitions_returns_one_tool() {
        let (store, _) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store);
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), OverflowToolExecutor::TOOL_NAME);
    }

    #[tokio::test]
    async fn execute_always_returns_none() {
        let (store, _) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store);
        let result = exec.execute("anything").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn unknown_tool_returns_none() {
        let (store, _) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store);
        let call = ToolCall {
            tool_id: "other_tool".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn invalid_uuid_returns_error() {
        let (store, cid) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store).with_conversation(cid);
        let call = make_call("not-a-uuid");
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn overflow_prefix_accepted_and_stripped() {
        let (store, cid) = make_store_with_conv().await;
        let content = b"prefixed overflow content";
        let uuid = store
            .save_overflow(cid, content)
            .await
            .expect("save_overflow");

        let exec = OverflowToolExecutor::new(Arc::clone(&store)).with_conversation(cid);
        let call = make_call(&format!("overflow:{uuid}"));
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary.as_bytes(), content);
    }

    #[tokio::test]
    async fn bare_uuid_still_accepted() {
        let (store, cid) = make_store_with_conv().await;
        let content = b"bare uuid content";
        let uuid = store
            .save_overflow(cid, content)
            .await
            .expect("save_overflow");

        let exec = OverflowToolExecutor::new(Arc::clone(&store)).with_conversation(cid);
        let call = make_call(&uuid);
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.summary.as_bytes(), content);
    }

    #[tokio::test]
    async fn invalid_uuid_with_overflow_prefix_returns_error() {
        let (store, cid) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store).with_conversation(cid);
        let call = make_call("overflow:not-a-uuid");
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[tokio::test]
    async fn missing_entry_returns_error() {
        let (store, cid) = make_store_with_conv().await;
        let exec = OverflowToolExecutor::new(store).with_conversation(cid);
        let call = make_call("00000000-0000-0000-0000-000000000000");
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn no_conversation_returns_error() {
        let (store, cid) = make_store_with_conv().await;
        let uuid = store.save_overflow(cid, b"data").await.expect("save");
        // Executor without conversation_id must return error (not panic).
        let exec = OverflowToolExecutor::new(store);
        let call = make_call(&uuid);
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn valid_entry_returns_content() {
        let (store, cid) = make_store_with_conv().await;
        let content = b"full tool output content";
        let uuid = store
            .save_overflow(cid, content)
            .await
            .expect("save_overflow");

        let exec = OverflowToolExecutor::new(Arc::clone(&store)).with_conversation(cid);
        let call = make_call(&uuid);
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(result.tool_name, OverflowToolExecutor::TOOL_NAME);
        assert_eq!(result.summary.as_bytes(), content);
    }

    #[tokio::test]
    async fn cross_conversation_access_denied() {
        let (store, cid1) = make_store_with_conv().await;
        let cid2 = store
            .create_conversation()
            .await
            .expect("create_conversation")
            .0;
        let uuid = store.save_overflow(cid1, b"secret").await.expect("save");
        // Executor bound to cid2 must not retrieve cid1's overflow.
        let exec = OverflowToolExecutor::new(Arc::clone(&store)).with_conversation(cid2);
        let call = make_call(&uuid);
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(_)),
            "must not access overflow from a different conversation"
        );
    }

    #[tokio::test]
    async fn read_overflow_output_is_not_reoverflowed() {
        // Verify that the tool returns raw content regardless of size.
        // The caller (native.rs) is responsible for skipping overflow for read_overflow results.
        let (store, cid) = make_store_with_conv().await;
        let big_content = "x".repeat(100_000).into_bytes();
        let uuid = store
            .save_overflow(cid, &big_content)
            .await
            .expect("save_overflow");

        let exec = OverflowToolExecutor::new(Arc::clone(&store)).with_conversation(cid);
        let call = make_call(&uuid);
        let result = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert_eq!(
            result.summary.len(),
            100_000,
            "full content must be returned"
        );
    }
}
