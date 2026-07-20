// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::sync::Arc;

use parking_lot::RwLock;
use zeph_memory::embedding_store::SearchFilter;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::types::ConversationId;
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};
use zeph_tools::registry::{InvocationHint, ToolDef};
use zeph_tools::{CheckpointActionResult, CheckpointListResult};

use zeph_sanitizer::ContentTrustLevel;
use zeph_sanitizer::memory_validation::MemoryWriteValidator;

/// Shared maximum content-trust-tier slot (issue #6490, `MemGhost`; TOCTOU-hardened by
/// #6558/#6569).
///
/// Stores the `u8` discriminant of [`ContentTrustLevel`]. `MemoryToolExecutor` reads it to
/// decide whether the interactive `memory_save` tool call needs user confirmation before
/// content derived from untrusted tool output is persisted.
///
/// Three writers keep this correct despite `MemoryToolExecutor` having no access to `Agent`'s
/// message history (the `ToolExecutor` trait is deliberately object-safe, with no `&Agent`
/// parameter — see `execute_tool_call_erased`):
/// - `Agent::ratchet_memory_consent_trust_for_dispatch` (`agent/tool_execution/sanitize.rs`) —
///   the authoritative writer. Runs at the top of `handle_native_tool_calls`, BEFORE any tool
///   call in the batch (including `memory_save` itself) starts executing, combining (a) the
///   worst-case trust tier the batch's tool names could introduce and (b) whatever untrusted
///   content is still tagged on a message in the live conversation context. Closes the
///   same-tier/cross-tier parallel-dispatch race (#6569: previously the slot was only updated
///   *after* `join_all` on the whole batch had already resolved) and the cross-turn deferral
///   bypass (#6558: previously a hard reset every turn discarded trust for content that was
///   still sitting in context, not yet compacted away).
/// - `Agent::sanitize_tool_output` — still ratchets up as each tool's output is classified,
///   as a defense-in-depth duplicate of (a) above (a no-op in practice since both derive the
///   same trust tier from the same tool name).
/// - `begin_turn`/`/clear` reset it to `0` as a floor; this is safe (not a re-introduction of
///   #6558) only because `ratchet_memory_consent_trust_for_dispatch` unconditionally recomputes
///   the correct value from live context before any subsequent tool dispatch.
pub type MemoryConsentTrustSlot = Arc<RwLock<u8>>;

/// Write-time consent-gate parameters attached via [`MemoryToolExecutor::with_consent_gate`].
struct ConsentGate {
    trust_slot: MemoryConsentTrustSlot,
    confirm_threshold: ContentTrustLevel,
}

/// Parse a `[memory.consent_gate]` trust-tier config string (`confirm_threshold`/
/// `disclose_threshold`) into a [`ContentTrustLevel`], falling back to
/// [`ContentTrustLevel::ExternalUntrusted`] (the most conservative tier) and logging a warning
/// on an unrecognized value.
///
/// # Examples
///
/// ```rust
/// use zeph_core::memory_tools::parse_consent_trust_level;
/// use zeph_sanitizer::ContentTrustLevel;
///
/// assert_eq!(
///     parse_consent_trust_level("local_untrusted"),
///     ContentTrustLevel::LocalUntrusted
/// );
/// assert_eq!(
///     parse_consent_trust_level("not_a_real_tier"),
///     ContentTrustLevel::ExternalUntrusted
/// );
/// ```
#[must_use]
pub fn parse_consent_trust_level(s: &str) -> ContentTrustLevel {
    ContentTrustLevel::from_str_opt(s).unwrap_or_else(|| {
        tracing::warn!(
            value = s,
            "invalid memory.consent_gate trust-tier value, falling back to external_untrusted"
        );
        ContentTrustLevel::ExternalUntrusted
    })
}

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

/// Executes `memory_search` and `memory_save` tool calls on behalf of the agent.
pub struct MemoryToolExecutor {
    memory: Arc<SemanticMemory>,
    conversation_id: ConversationId,
    validator: MemoryWriteValidator,
    /// When `true` the backing store is in-memory (bare mode) and saves do not persist across sessions.
    ephemeral: bool,
    /// Write-time memory-consent gate (issue #6490). `None` when `memory.consent_gate.enabled
    /// = false` — `memory_save` never requires confirmation in that case.
    consent_gate: Option<ConsentGate>,
    /// Audit sink for memory-write attribution (issue #6490). `None` when audit logging is
    /// disabled — `memory_save` writes are not logged, matching other executors' behavior.
    audit_logger: Option<Arc<zeph_tools::AuditLogger>>,
    /// Mirrors `memory.consent_gate.audit_all` (issue #6559). Deliberately independent of
    /// `consent_gate` (which is `None` whenever `enabled = false`) — `audit_all` gates the audit
    /// log regardless of the master `enabled` switch, matching
    /// `Agent::persist_message_inner`'s background-write-path gating. Defaults to `true` so
    /// callers that have not been updated to call `with_audit_all` keep the pre-#6559 behavior
    /// (audit fires whenever a logger is attached).
    audit_all: bool,
}

impl MemoryToolExecutor {
    /// Create with default validator and persistent (non-ephemeral) semantics.
    #[must_use]
    pub fn new(memory: Arc<SemanticMemory>, conversation_id: ConversationId) -> Self {
        Self {
            memory,
            conversation_id,
            validator: MemoryWriteValidator::new(
                zeph_sanitizer::memory_validation::MemoryWriteValidationConfig::default(),
            ),
            ephemeral: false,
            consent_gate: None,
            audit_logger: None,
            audit_all: true,
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
            ephemeral: false,
            consent_gate: None,
            audit_logger: None,
            audit_all: true,
        }
    }

    /// Mark this executor as ephemeral (bare mode).
    ///
    /// When set, `memory_save` reports that the content is session-only and will not be
    /// available after the session ends.
    #[must_use]
    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Attach the write-time memory-consent gate (issue #6490, `MemGhost`).
    ///
    /// `trust_slot` must be the same [`MemoryConsentTrustSlot`] the owning `Agent` ratchets up
    /// in `sanitize_tool_output` — see `AgentBuilder::with_memory_consent_trust_slot`.
    /// `confirm_threshold` is the minimum trust tier (inclusive) that requires
    /// `Channel::confirm` before `memory_save` persists (`memory.consent_gate.confirm_threshold`
    /// in config).
    #[must_use]
    pub fn with_consent_gate(
        mut self,
        trust_slot: MemoryConsentTrustSlot,
        confirm_threshold: ContentTrustLevel,
    ) -> Self {
        self.consent_gate = Some(ConsentGate {
            trust_slot,
            confirm_threshold,
        });
        self
    }

    /// Attach an audit sink so every `memory_save` write is recorded with source attribution
    /// (issue #6490, `MemGhost` part D).
    #[must_use]
    pub fn with_audit(mut self, logger: Arc<zeph_tools::AuditLogger>) -> Self {
        self.audit_logger = Some(logger);
        self
    }

    /// Set whether `memory_save` writes are audited, mirroring `memory.consent_gate.audit_all`
    /// (issue #6559). Pass the config value here regardless of `consent_gate.enabled` — audit
    /// attribution and the interactive/disclosure gate are independent switches, matching
    /// `Agent::persist_message_inner`'s background-write-path gating (`store.rs`).
    #[must_use]
    pub fn with_audit_all(mut self, audit_all: bool) -> Self {
        self.audit_all = audit_all;
        self
    }

    /// Current maximum trust tier for the gate check, or [`ContentTrustLevel::Trusted`] when
    /// no consent gate is attached (gate disabled).
    ///
    /// Since #6558/#6569's fix, this reads a slot that reflects both the current dispatch
    /// batch AND any untrusted content still tagged in the live conversation context (see
    /// `Agent::ratchet_memory_consent_trust_for_dispatch`, `agent/tool_execution/sanitize.rs`)
    /// — not just "this turn's own tool output" as before.
    ///
    /// Intentional scope broadening (not an oversight): `memory_search` classifies as
    /// `ExternalUntrusted` (`MemoryRetrieval` source) and its retrieval result is tagged and
    /// persists in context like any other untrusted tool output. In a memory-enabled agent
    /// that calls `memory_search` frequently, this means most `memory_save` calls will require
    /// confirmation for as long as a `memory_search` result remains in the context window —
    /// a broader gate footprint than the original per-turn design. This is the fail-safe
    /// direction (more confirmations, never fewer) and is a deliberate tradeoff of closing
    /// #6558/#6569's TOCTOU windows, not a bug. If this UX shift proves too aggressive in
    /// practice, the fix is to exclude `memory_search` from `build_tool_output_source`'s
    /// context-tagging path specifically (`agent/tool_execution/sanitize.rs`) — not to weaken
    /// the gate check here.
    fn current_trust_level(&self) -> ContentTrustLevel {
        self.consent_gate
            .as_ref()
            .map_or(ContentTrustLevel::Trusted, |gate| {
                ContentTrustLevel::from_ordinal(*gate.trust_slot.read())
            })
    }

    /// Perform the actual `memory_save` write, bypassing the consent-gate confirmation check.
    ///
    /// Called both by `execute_tool_call` (when no confirmation is required) and
    /// `execute_tool_call_confirmed` (after the user has approved).
    async fn do_memory_save(
        &self,
        params: &MemorySaveParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
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
        let trust_level = self.current_trust_level();

        // Explicit user-directed saves bypass goal-conditioned scoring (goal_text = None).
        // Provenance (issue #6490): the LLM-supplied `role` is never used as a trust signal —
        // trust is derived from the turn's actual tool-output origin via the consent-gate slot.
        let message_id_opt = self
            .memory
            .remember_with_provenance(
                self.conversation_id,
                role,
                &params.content,
                None,
                None,
                Some(trust_level.as_str()),
            )
            .await
            .map_err(|e| ToolError::Execution(std::io::Error::other(e.to_string())))?;

        if self.audit_all
            && let Some(logger) = &self.audit_logger
        {
            let preview: String = params.content.chars().take(120).collect();
            let entry = zeph_tools::AuditEntry::memory_write(
                "memory_save",
                format!("save: {preview}"),
                None,
                Some(trust_level.as_str()),
            );
            logger.log(&entry).await;
        }

        let summary = match message_id_opt {
            Some(message_id) => {
                if self.ephemeral {
                    format!(
                        "Saved to session memory (message_id: {message_id}, conversation: {}). Ephemeral — not available after session ends.",
                        self.conversation_id
                    )
                } else {
                    format!(
                        "Saved to memory (message_id: {message_id}, conversation: {}). Content will be available for future recall.",
                        self.conversation_id
                    )
                }
            }
            None => "Memory admission rejected: message did not meet quality threshold.".to_owned(),
        };

        Ok(Some(ToolOutput {
            tool_name: zeph_common::ToolName::new("memory_save"),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(zeph_tools::ClaimSource::Memory),
            ..Default::default()
        }))
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
                output_schema: None,
                server_id: None,
            },
            ToolDef {
                id: "memory_save".into(),
                description: "Save a fact or note to long-term memory for cross-session recall. Use sparingly for key decisions, user preferences, or critical context worth remembering across sessions.\n\nParameters: content (string, required) - concise, self-contained fact or note; role (string, optional) - message role label (default: \"assistant\")\nReturns: confirmation with saved entry ID\nErrors: Execution on database failure; InvalidParams if content is empty\nExample: {\"content\": \"User prefers JSON output over YAML\", \"role\": \"assistant\"}".into(),
                schema: schemars::schema_for!(MemorySaveParams),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
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
                    .search_key_facts(&params.query, limit, Some(self.conversation_id))
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
                        zeph_llm::provider::Role::Assistant => "assistant",
                        zeph_llm::provider::Role::System => "system",
                        zeph_llm::provider::Role::User | _ => "user",
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
                    tool_name: zeph_common::ToolName::new("memory_search"),
                    summary: output,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: Some(zeph_tools::ClaimSource::Memory),
                    ..Default::default()
                }))
            }
            "memory_save" => {
                let params: MemorySaveParams = deserialize_params(&call.params)?;

                // Write-time consent gate (issue #6490, MemGhost): require interactive
                // confirmation when this turn already contains tool output at or above the
                // configured trust threshold. Uses the same ConfirmationRequired ->
                // Channel::confirm protocol as TrustGateExecutor::check_trust — the agent's
                // `handle_confirmation_phase` catches this and re-dispatches via
                // `execute_tool_call_confirmed` on approval.
                if let Some(gate) = &self.consent_gate
                    && self.current_trust_level() >= gate.confirm_threshold
                {
                    let preview: String = params.content.chars().take(80).collect();
                    let ellipsis = if params.content.chars().count() > 80 {
                        "…"
                    } else {
                        ""
                    };
                    let trust = self.current_trust_level();
                    return Err(ToolError::ConfirmationRequired {
                        command: format!(
                            "Save to memory: {preview}{ellipsis} [source: {}]",
                            trust.as_str()
                        ),
                    });
                }

                self.do_memory_save(&params).await
            }
            _ => Ok(None),
        }
    }

    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        if call.tool_id.as_str() != "memory_save" {
            return false;
        }
        let Some(gate) = &self.consent_gate else {
            return false;
        };
        self.current_trust_level() >= gate.confirm_threshold
    }

    /// Execute bypassing the consent-gate confirmation check (called after the user approves).
    ///
    /// `memory_search` has no confirmation policy of its own, so it delegates to
    /// [`ToolExecutor::execute_tool_call`] unchanged.
    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id.as_str() == "memory_save" {
            let params: MemorySaveParams = deserialize_params(&call.params)?;
            return self.do_memory_save(&params).await;
        }
        self.execute_tool_call(call).await
    }

    fn checkpoint_undo(&self, _n: usize) -> CheckpointActionResult {
        CheckpointActionResult::unsupported()
    }

    fn checkpoint_redo(&self) -> CheckpointActionResult {
        CheckpointActionResult::unsupported()
    }

    fn checkpoint_list(&self) -> CheckpointListResult {
        CheckpointListResult::default()
    }

    fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
        false
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
            tool_id: zeph_common::ToolName::new("unknown_tool"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
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
            tool_id: zeph_common::ToolName::new("memory_search"),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
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
            tool_id: zeph_common::ToolName::new("memory_save"),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
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
            tool_id: zeph_common::ToolName::new("memory_save"),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
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
            tool_id: zeph_common::ToolName::new("memory_save"),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn memory_save_ephemeral_returns_session_only_message() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid).ephemeral();

        let mut params = serde_json::Map::new();
        params.insert(
            "content".into(),
            serde_json::Value::String("temp fact".into()),
        );
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("memory_save"),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let output = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(
            output.summary.contains("Ephemeral"),
            "bare-mode save must mention ephemeral semantics; got: {}",
            output.summary
        );
        assert!(
            !output.summary.contains("available for future recall"),
            "bare-mode save must not claim cross-session persistence; got: {}",
            output.summary
        );
    }

    fn memory_save_call(content: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("content".into(), serde_json::Value::String(content.into()));
        ToolCall {
            tool_id: zeph_common::ToolName::new("memory_save"),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    // ── Write-time memory-consent gate (issue #6490, MemGhost) ─────────────────────

    #[tokio::test]
    async fn memory_save_without_consent_gate_never_requires_confirmation() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        // No `.with_consent_gate(...)` attached — must behave exactly as before #6490.
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid);
        let call = memory_save_call("a fact");
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_ok(), "expected no confirmation gate: {result:?}");
    }

    #[tokio::test]
    async fn memory_save_requires_confirmation_when_turn_trust_at_or_above_threshold() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let trust_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid).with_consent_gate(
            Arc::clone(&trust_slot),
            ContentTrustLevel::ExternalUntrusted,
        );

        // Simulate sanitize_tool_output having ratcheted the slot up this turn.
        *trust_slot.write() = ContentTrustLevel::ExternalUntrusted as u8;

        let call = memory_save_call("derived from untrusted web content");
        let result = executor.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(ToolError::ConfirmationRequired { .. })),
            "expected ConfirmationRequired, got: {result:?}"
        );
        assert!(executor.requires_confirmation(&call));
    }

    #[tokio::test]
    async fn memory_save_below_confirm_threshold_does_not_require_confirmation() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let trust_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid).with_consent_gate(
            Arc::clone(&trust_slot),
            ContentTrustLevel::ExternalUntrusted,
        );

        // Only LocalUntrusted this turn — below the ExternalUntrusted confirm threshold.
        *trust_slot.write() = ContentTrustLevel::LocalUntrusted as u8;

        let call = memory_save_call("derived from a local shell command");
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_ok(), "expected no confirmation gate: {result:?}");
    }

    #[tokio::test]
    async fn execute_tool_call_confirmed_bypasses_consent_gate_and_saves() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let trust_slot: MemoryConsentTrustSlot = Arc::new(RwLock::new(0u8));
        let executor = MemoryToolExecutor::new(Arc::new(memory), cid).with_consent_gate(
            Arc::clone(&trust_slot),
            ContentTrustLevel::ExternalUntrusted,
        );
        *trust_slot.write() = ContentTrustLevel::ExternalUntrusted as u8;

        let call = memory_save_call("approved after confirmation");
        // First attempt is gated.
        assert!(matches!(
            executor.execute_tool_call(&call).await,
            Err(ToolError::ConfirmationRequired { .. })
        ));
        // Confirmed re-dispatch bypasses the gate and actually saves.
        let result = executor.execute_tool_call_confirmed(&call).await;
        assert!(result.is_ok(), "confirmed save should succeed: {result:?}");
        let output = result.unwrap().unwrap();
        assert!(output.summary.contains("Saved to memory"));
    }

    // ── audit_all gating on the interactive memory_save path (issue #6559) ─────────

    async fn make_file_logger(log_path: &std::path::Path) -> Arc<zeph_tools::AuditLogger> {
        let audit_config = zeph_tools::AuditConfig {
            enabled: true,
            destination: zeph_tools::AuditDestination::File(log_path.to_path_buf()),
            tool_risk_summary: false,
        };
        Arc::new(
            zeph_tools::AuditLogger::from_config(&audit_config, false)
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn memory_save_audited_when_audit_all_true() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("audit.jsonl");
        let logger = make_file_logger(&log_path).await;

        let executor = MemoryToolExecutor::new(Arc::new(memory), cid)
            .with_audit(logger)
            .with_audit_all(true);

        let call = memory_save_call("audited fact");
        executor.execute_tool_call(&call).await.unwrap();

        let content = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            content.contains("memory_save"),
            "audit_all=true must record the interactive memory_save write, got: {content}"
        );
    }

    #[tokio::test]
    async fn memory_save_not_audited_when_audit_all_false() {
        let memory = make_memory().await;
        let sqlite = memory.sqlite().clone();
        let cid = sqlite.create_conversation().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("audit.jsonl");
        let logger = make_file_logger(&log_path).await;

        let executor = MemoryToolExecutor::new(Arc::new(memory), cid)
            .with_audit(logger)
            .with_audit_all(false);

        let call = memory_save_call("unaudited fact");
        executor.execute_tool_call(&call).await.unwrap();

        // The logger was never asked to write, so the destination file must stay empty —
        // matches persist_message_inner's audit_all=false behavior on the background path.
        let content = tokio::fs::read_to_string(&log_path)
            .await
            .unwrap_or_default();
        assert!(
            content.is_empty(),
            "audit_all=false must suppress the interactive memory_save audit entry, got: {content}"
        );
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
