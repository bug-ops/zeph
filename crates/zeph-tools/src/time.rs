// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `get_current_time` tool (#6361): lets the LLM query the real current UTC time on demand.
//!
//! Large language models have no innate sense of wall-clock time. This executor closes that
//! gap with a near-zero-cost local clock read (no I/O), resolved through an injectable
//! [`zeph_common::ClockSource`] rather than calling [`std::time::SystemTime::now`] directly, so
//! it stays deterministic in tests (spec 070 FR-002, NFR-001).

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_common::{ClockSource, SystemClock, ToolName};

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};
use crate::registry::{InvocationHint, ToolDef};

const TOOL_NAME: &str = "get_current_time";

const TOOL_DESCRIPTION: &str = "Returns the current UTC date and time. Use this whenever you \
need to reason about \"today\", deadlines, or relative dates (\"next Monday\", \"in 3 days\") — \
never assume the current date from training data. UTC only; convert to a local timezone \
yourself if the user needs one.";

/// Output format for [`GetCurrentTimeExecutor`]. UTC only (spec 070 FR-009 — timezone display
/// is a channel concern, out of scope here).
#[derive(Debug, PartialEq, Eq)]
enum TimeFormat {
    /// RFC 3339 UTC, e.g. `2026-07-18T02:30:00Z`.
    Rfc3339,
    /// Seconds since the Unix epoch.
    Unix,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TimeParams {
    /// Output format: "rfc3339" (default) or "unix" (seconds since the Unix epoch).
    #[serde(default)]
    format: Option<String>,
}

/// Tool executor for the `get_current_time` tool.
///
/// Resolves the current time through an injectable [`ClockSource`] (default
/// [`SystemClock`]) rather than reading the system clock directly, so callers can substitute
/// a deterministic clock in tests.
pub struct GetCurrentTimeExecutor {
    clock: Arc<dyn ClockSource>,
}

impl GetCurrentTimeExecutor {
    /// Creates an executor backed by `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn ClockSource>) -> Self {
        Self { clock }
    }
}

impl Default for GetCurrentTimeExecutor {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

impl std::fmt::Debug for GetCurrentTimeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetCurrentTimeExecutor")
            .finish_non_exhaustive()
    }
}

impl ToolExecutor for GetCurrentTimeExecutor {
    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let result = (|| {
            if call.tool_id != TOOL_NAME {
                return Ok(None);
            }
            let params: TimeParams = deserialize_params(&call.params)?;
            let now = self.clock.now();
            let format = match params.format.as_deref() {
                Some("unix") => TimeFormat::Unix,
                _ => TimeFormat::Rfc3339,
            };
            let summary = match format {
                TimeFormat::Rfc3339 => zeph_common::timestamp::rfc3339_from(now),
                TimeFormat::Unix => now
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .to_string(),
            };

            Ok(Some(ToolOutput {
                tool_name: ToolName::new(TOOL_NAME),
                summary,
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))
        })();
        std::future::ready(result)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: TOOL_NAME.into(),
            description: TOOL_DESCRIPTION.into(),
            schema: schemars::schema_for!(TimeParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]
    }

    fn is_tool_retryable(&self, _tool_id: &str) -> bool {
        true
    }

    fn execute(
        &self,
        _response: &str,
    ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    crate::tool_executor_no_inner_defaults!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};
    use zeph_common::FixedClock;

    fn make_call(format: Option<&str>) -> ToolCall {
        let mut params = serde_json::Map::new();
        if let Some(f) = format {
            params.insert("format".to_owned(), serde_json::Value::String(f.to_owned()));
        }
        ToolCall {
            tool_id: ToolName::new(TOOL_NAME),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn fixed_executor() -> GetCurrentTimeExecutor {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        GetCurrentTimeExecutor::new(Arc::new(FixedClock(t)))
    }

    #[tokio::test]
    async fn returns_rfc3339_by_default() {
        let executor = fixed_executor();
        let result = executor
            .execute_tool_call(&make_call(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.summary, "2023-11-14T22:13:20Z");
    }

    #[tokio::test]
    async fn returns_rfc3339_when_explicitly_requested() {
        let executor = fixed_executor();
        let result = executor
            .execute_tool_call(&make_call(Some("rfc3339")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.summary, "2023-11-14T22:13:20Z");
    }

    #[tokio::test]
    async fn returns_unix_seconds_when_requested() {
        let executor = fixed_executor();
        let result = executor
            .execute_tool_call(&make_call(Some("unix")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.summary, "1700000000");
    }

    #[tokio::test]
    async fn unrecognized_format_falls_back_to_rfc3339() {
        // Pins the current lenient behavior explicitly, so a future change to this fallback
        // (e.g. making it an error) is a deliberate decision, not an unnoticed regression.
        let executor = fixed_executor();
        let result = executor
            .execute_tool_call(&make_call(Some("banana")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.summary, "2023-11-14T22:13:20Z");
    }

    #[tokio::test]
    async fn returns_none_for_unknown_tool() {
        let executor = fixed_executor();
        let call = ToolCall {
            tool_id: ToolName::new("other_tool"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        assert!(executor.execute_tool_call(&call).await.unwrap().is_none());
    }

    #[test]
    fn tool_definitions_contains_get_current_time() {
        let executor = GetCurrentTimeExecutor::default();
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), TOOL_NAME);
    }

    #[test]
    fn is_retryable() {
        let executor = GetCurrentTimeExecutor::default();
        assert!(executor.is_tool_retryable(TOOL_NAME));
    }
}
