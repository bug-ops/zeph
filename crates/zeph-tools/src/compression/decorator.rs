// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `CompressedExecutor<E>` — decorator that post-processes tool output through a compressor.
//!
//! Wraps the ROOT executor (any `ToolExecutor` implementation). The compressor is applied
//! only to successful `ToolOutput.summary` strings — on error, the raw result is returned
//! unchanged.
//!
//! # Invariant (T4)
//!
//! Audit logging is performed by the wrapped tool implementations, not here. Because
//! `CompressedExecutor` wraps *outside* the tool boundary, audit JSONL always records
//! the raw pre-compression payload.

use std::sync::Arc;

use crate::executor::ToolExecutor;
use crate::{ToolCall, ToolError, ToolOutput};

use super::OutputCompressor;

/// Decorator that runs a compressor on each successful tool output.
///
/// The `inner` executor is called first; its output is then passed to `compressor.compress`.
/// If compression returns `Ok(None)` or `Err(...)`, the original `summary` is kept intact.
/// Compression errors are logged as warnings but never propagate to the caller.
///
/// # Type parameters
///
/// - `E` — the wrapped [`ToolExecutor`]. Often `CompositeExecutor` or `DynExecutor`.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_tools::compression::{CompressedExecutor, IdentityCompressor};
/// // let executor = CompressedExecutor::new(inner_executor, Arc::new(IdentityCompressor), 200);
/// ```
#[derive(Debug)]
pub struct CompressedExecutor<E: ToolExecutor> {
    inner: E,
    compressor: Arc<dyn OutputCompressor>,
    min_lines_to_compress: usize,
}

impl<E: ToolExecutor> CompressedExecutor<E> {
    /// Wrap `inner` with `compressor`.
    ///
    /// Outputs with fewer than `min_lines` lines skip the compressor entirely.
    #[must_use]
    pub fn new(inner: E, compressor: Arc<dyn OutputCompressor>, min_lines: usize) -> Self {
        Self {
            inner,
            compressor,
            min_lines_to_compress: min_lines,
        }
    }

    /// Apply compression to `output`, logging on error and returning the original on failure.
    async fn maybe_compress(&self, output: ToolOutput) -> ToolOutput {
        let line_count = output.summary.lines().count();
        if line_count < self.min_lines_to_compress {
            return output;
        }

        match self
            .compressor
            .compress(&output.tool_name, &output.summary)
            .await
        {
            Ok(Some(compressed)) => {
                tracing::debug!(
                    compressor = self.compressor.name(),
                    tool = %output.tool_name.as_str(),
                    original_len = output.summary.len(),
                    compressed_len = compressed.len(),
                    "CompressedExecutor: output compressed"
                );
                ToolOutput {
                    summary: compressed,
                    ..output
                }
            }
            Ok(None) => output,
            Err(e) => {
                tracing::warn!(
                    compressor = self.compressor.name(),
                    error = %e,
                    "CompressedExecutor: compression error, using raw output"
                );
                output
            }
        }
    }
}

impl<E: ToolExecutor> ToolExecutor for CompressedExecutor<E> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let result = self.inner.execute(response).await?;
        match result {
            Some(out) => Ok(Some(self.maybe_compress(out).await)),
            None => Ok(None),
        }
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let result = self.inner.execute_confirmed(response).await?;
        match result {
            Some(out) => Ok(Some(self.maybe_compress(out).await)),
            None => Ok(None),
        }
    }

    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let result = self.inner.execute_tool_call(call).await?;
        match result {
            Some(out) => Ok(Some(self.maybe_compress(out).await)),
            None => Ok(None),
        }
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let result = self.inner.execute_tool_call_confirmed(call).await?;
        match result {
            Some(out) => Ok(Some(self.maybe_compress(out).await)),
            None => Ok(None),
        }
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_speculatable(tool_id)
    }

    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        self.inner.requires_confirmation(call)
    }

    fn checkpoint_undo(&self, n: usize) -> crate::executor::CheckpointActionResult {
        self.inner.checkpoint_undo(n)
    }

    fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
        self.inner.checkpoint_redo()
    }

    fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
        self.inner.checkpoint_list()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use zeph_common::ToolName;

    use super::*;
    use crate::compression::{CompressionError, OutputCompressor};
    use crate::{SkillTrustLevel, ToolCall, ToolError, ToolOutput, registry::ToolDef};

    /// Records the raw output it receives so the test can assert on it.
    struct SpyExecutor {
        received_summary: Arc<Mutex<Option<String>>>,
        raw_output: String,
    }

    impl SpyExecutor {
        fn new(raw: impl Into<String>) -> (Self, Arc<Mutex<Option<String>>>) {
            let spy = Arc::new(Mutex::new(None));
            (
                Self {
                    received_summary: Arc::clone(&spy),
                    raw_output: raw.into(),
                },
                spy,
            )
        }
    }

    fn make_output(tool_name: ToolName, summary: String) -> ToolOutput {
        ToolOutput {
            tool_name,
            summary,
            blocks_executed: 0,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
            ..Default::default()
        }
    }

    impl ToolExecutor for SpyExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(make_output(
                ToolName::new("spy"),
                self.raw_output.clone(),
            )))
        }

        async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
            self.execute(response).await
        }

        fn tool_definitions(&self) -> Vec<ToolDef> {
            vec![]
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            let out = make_output(call.tool_id.clone(), self.raw_output.clone());
            *self.received_summary.lock().unwrap() = Some(out.summary.clone());
            Ok(Some(out))
        }

        async fn execute_tool_call_confirmed(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            self.execute_tool_call(call).await
        }

        fn set_skill_env(&self, _env: Option<HashMap<String, String>>) {}
        fn set_effective_trust(&self, _level: SkillTrustLevel) {}
        fn is_tool_retryable(&self, _tool_id: &str) -> bool {
            false
        }
        fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
            false
        }
        fn requires_confirmation(&self, _call: &ToolCall) -> bool {
            false
        }
        fn checkpoint_undo(&self, _n: usize) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult::unsupported()
        }
        fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult::unsupported()
        }
        fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
            crate::executor::CheckpointListResult::default()
        }
    }

    /// Always replaces output with a fixed "compressed" string.
    #[derive(Debug)]
    struct StubCompressor;

    impl OutputCompressor for StubCompressor {
        fn compress<'a>(
            &'a self,
            _tool_name: &'a ToolName,
            _output: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CompressionError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(Some("COMPRESSED".to_owned())) })
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    /// T4 invariant: audit (inner) receives raw output; LLM context receives compressed output.
    ///
    /// The inner executor (`SpyExecutor`) records what it emits before `CompressedExecutor`
    /// applies the compressor. The assertion confirms that the inner layer saw the full raw
    /// string, while the outer `CompressedExecutor` returns the shortened version.
    #[tokio::test]
    async fn t4_audit_sees_raw_llm_sees_compressed() {
        let raw = "line\n".repeat(300);
        let (spy, received) = SpyExecutor::new(raw.clone());
        let executor = CompressedExecutor::new(spy, Arc::new(StubCompressor), 10);

        let call = ToolCall {
            tool_id: ToolName::new("spy"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = executor.execute_tool_call(&call).await.unwrap().unwrap();

        // Inner executor (audit layer) received the raw payload.
        assert_eq!(received.lock().unwrap().as_deref(), Some(raw.as_str()));
        // Outer executor (LLM context layer) received the compressed payload.
        assert_eq!(out.summary, "COMPRESSED");
    }

    /// Output below the line-count threshold passes through without compression.
    #[tokio::test]
    async fn maybe_compress_skips_when_below_threshold() {
        let short = "line\n".repeat(5);
        let (spy, _received) = SpyExecutor::new(short.clone());
        let executor = CompressedExecutor::new(spy, Arc::new(StubCompressor), 100);

        let call = ToolCall {
            tool_id: ToolName::new("spy"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = executor.execute_tool_call(&call).await.unwrap().unwrap();
        // StubCompressor would return "COMPRESSED" — but threshold not met, so raw passes through.
        assert_eq!(out.summary, short);
    }

    /// Compressor error falls back to the raw (uncompressed) output.
    #[tokio::test]
    async fn compression_error_falls_back_to_raw() {
        #[derive(Debug)]
        struct ErrorCompressor;
        impl OutputCompressor for ErrorCompressor {
            fn compress<'a>(
                &'a self,
                _tool_name: &'a ToolName,
                _output: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CompressionError>> + Send + 'a>>
            {
                Box::pin(async move { Err(CompressionError::CompileTimeout) })
            }
            fn name(&self) -> &'static str {
                "error"
            }
        }

        let raw = "line\n".repeat(300);
        let (spy, _) = SpyExecutor::new(raw.clone());
        let executor = CompressedExecutor::new(spy, Arc::new(ErrorCompressor), 10);

        let call = ToolCall {
            tool_id: ToolName::new("spy"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = executor.execute_tool_call(&call).await.unwrap().unwrap();
        // Error compressor → raw output preserved (T4 safety invariant).
        assert_eq!(out.summary, raw);
    }

    /// Inner executor whose cross-cutting methods return distinguishable non-default
    /// values, used to prove `CompressedExecutor` forwards rather than falling through
    /// to the base `ToolExecutor` defaults.
    #[derive(Debug)]
    struct CheckpointStubExecutor;

    impl ToolExecutor for CheckpointStubExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(&self, _: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        fn requires_confirmation(&self, _call: &ToolCall) -> bool {
            true
        }
        fn checkpoint_undo(&self, _n: usize) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult {
                reverted_commands: 1,
                restored: 2,
                deleted: 3,
                supported: true,
                message: "stub-undo".to_owned(),
            }
        }
        fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult {
                reverted_commands: 4,
                restored: 5,
                deleted: 6,
                supported: true,
                message: "stub-redo".to_owned(),
            }
        }
        fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
            crate::executor::CheckpointListResult {
                entries: vec![],
                redo_depth: 7,
                supported: true,
            }
        }
        async fn execute_tool_call_confirmed(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            self.execute_tool_call(call).await
        }
        fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
            false
        }
    }

    /// Regression test for #6012: `requires_confirmation` and the checkpoint trio must be
    /// forwarded to `self.inner`. Before the fix they fell through to the base
    /// `ToolExecutor` defaults (`false` / `unsupported()`) regardless of the inner
    /// executor's actual policy or checkpoint state.
    #[test]
    fn requires_confirmation_and_checkpoints_delegated_to_inner() {
        let executor =
            CompressedExecutor::new(CheckpointStubExecutor, Arc::new(StubCompressor), 10);

        let call = ToolCall {
            tool_id: ToolName::new("spy"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        assert!(executor.requires_confirmation(&call));

        let undo = executor.checkpoint_undo(1);
        assert!(undo.supported);
        assert_eq!(undo.message, "stub-undo");

        let redo = executor.checkpoint_redo();
        assert!(redo.supported);
        assert_eq!(redo.message, "stub-redo");

        let list = executor.checkpoint_list();
        assert!(list.supported);
        assert_eq!(list.redo_depth, 7);
    }
}
