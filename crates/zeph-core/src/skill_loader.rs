// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool executor that loads a full skill body by name, gated by the same trust-aware pipeline
//! as `invoke_skill`.
//!
//! [`SkillLoaderExecutor`] implements `load_skill` — a native tool the LLM can call to preview a
//! skill's full body without committing to follow it (unlike `invoke_skill`, which carries
//! intent-to-apply semantics). Both tools share a
//! `SkillTrustGate` (crate-private) built from the same
//! `trust_snapshot` `Arc`, so they observe identical trust state within a turn:
//! - Non-Trusted bodies pass through [`sanitize_skill_text`](zeph_skills::prompt::sanitize_skill_text).
//! - Quarantined bodies are additionally wrapped with [`wrap_quarantined`](zeph_skills::prompt::wrap_quarantined).
//! - Blocked skills are refused before any body read.
//! - `skill_name` is sanitized before it appears in any output path (found, blocked, not-found).
//!
//! `load_skill` and `invoke_skill` are both listed in `QUARANTINE_DENIED`, so when a Quarantined
//! skill is active the trust gate refuses both before this executor is reached.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params, truncate_tool_output,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::skill_invoker::SkillTrustSnapshot;
use crate::skill_trust_gate::{SkillBodyResolution, SkillTrustGate};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadSkillParams {
    /// Name of the skill to load (from `<other_skills>` catalog).
    pub skill_name: String,
}

/// Tool executor that loads a full skill body by name from the shared registry.
///
/// Delegates trust resolution, integrity checking, sanitization, and quarantine wrapping to a
/// shared `SkillTrustGate` (crate-private) — the same pipeline `invoke_skill`
/// ([`crate::skill_invoker::SkillInvokeExecutor`]) uses, so the two tools cannot drift apart.
#[derive(Clone, Debug)]
pub struct SkillLoaderExecutor {
    gate: SkillTrustGate,
}

impl SkillLoaderExecutor {
    /// Create a new executor with shared registry and trust snapshot.
    ///
    /// `trust_snapshot` must be the same `Arc` shared with `SkillInvokeExecutor` (see
    /// `agent_setup::build_skill_executors` in the binary crate) so `load_skill` and
    /// `invoke_skill` see identical trust state within a turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use std::sync::Arc;
    ///
    /// use parking_lot::RwLock;
    /// use zeph_core::SkillLoaderExecutor;
    /// use zeph_skills::registry::SkillRegistry;
    ///
    /// let registry = Arc::new(RwLock::new(SkillRegistry::empty()));
    /// let trust_snapshot = Arc::new(RwLock::new(HashMap::new()));
    /// let _executor = SkillLoaderExecutor::new(registry, trust_snapshot);
    /// ```
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
    ) -> Self {
        Self {
            gate: SkillTrustGate::new(registry, trust_snapshot),
        }
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
            output_schema: None,
            server_id: None,
        }]
    }

    #[tracing::instrument(name = "core.skill_loader.execute", skip_all, fields(skill = tracing::field::Empty))]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "load_skill" {
            return Ok(None);
        }
        let params: LoadSkillParams = deserialize_params(&call.params)?;
        let skill_name: String = params.skill_name.chars().take(128).collect();

        tracing::Span::current().record("skill", skill_name.as_str());

        let summary = match self.gate.resolve_body(&skill_name).await? {
            SkillBodyResolution::Refused(message) | SkillBodyResolution::NotFound(message) => {
                message
            }
            SkillBodyResolution::Body(wrapped) => truncate_tool_output(&wrapped),
        };

        Ok(Some(ToolOutput {
            tool_name: zeph_common::ToolName::new("load_skill"),
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

    use zeph_common::SkillTrustLevel;

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

    fn make_snapshot(level: SkillTrustLevel) -> SkillTrustSnapshot {
        SkillTrustSnapshot {
            trust_level: level,
            requires_trust_check: false,
            blake3_hash: String::new(),
        }
    }

    /// `trust_map` is `None` to exercise the missing-row-defaults-to-Trusted path.
    fn make_executor(
        registry: SkillRegistry,
        trust_map: HashMap<String, SkillTrustLevel>,
    ) -> SkillLoaderExecutor {
        let snapshot_map: HashMap<String, SkillTrustSnapshot> = trust_map
            .into_iter()
            .map(|(k, v)| (k, make_snapshot(v)))
            .collect();
        SkillLoaderExecutor::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(snapshot_map)),
        )
    }

    fn make_call(skill_name: &str) -> ToolCall {
        ToolCall {
            tool_id: zeph_common::ToolName::new("load_skill"),
            params: serde_json::json!({"skill_name": skill_name})
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    #[tokio::test]
    async fn load_existing_skill_returns_body() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            make_registry_with_skill(dir.path(), "git-commit", "## Instructions\nDo git stuff");
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("git-commit"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("## Instructions"));
        assert!(result.summary.contains("Do git stuff"));
    }

    #[tokio::test]
    async fn load_nonexistent_skill_returns_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("nonexistent"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("skill not found"));
        assert!(result.summary.contains("nonexistent"));
    }

    #[test]
    fn tool_definitions_returns_load_skill() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "load_skill");
    }

    #[tokio::test]
    async fn execute_returns_none_for_wrong_tool_id() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("bash"),
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
    async fn long_skill_body_is_truncated() {
        use zeph_tools::executor::MAX_TOOL_OUTPUT_CHARS;
        let dir = tempfile::tempdir().unwrap();
        let long_body = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 1000);
        let registry = make_registry_with_skill(dir.path(), "big-skill", &long_body);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("big-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("truncated"));
        assert!(result.summary.len() < long_body.len() + 200);
    }

    #[tokio::test]
    async fn empty_registry_returns_error_message() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("any"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("skill not found"));
    }

    // GAP-1: direct execute() always returns None
    #[tokio::test]
    async fn execute_always_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor.execute("any response text").await.unwrap();
        assert!(result.is_none());
    }

    // GAP-2: concurrent reads all succeed
    #[tokio::test]
    async fn concurrent_execute_tool_call_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let registry =
            make_registry_with_skill(dir.path(), "shared-skill", "## Concurrent test body");
        let executor = Arc::new(make_executor(registry, HashMap::new()));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ex = Arc::clone(&executor);
                tokio::spawn(async move { ex.execute_tool_call(&make_call("shared-skill")).await })
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
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call(""))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("skill not found"));
    }

    // GAP-4: missing skill_name field returns ToolError from deserialize_params
    #[tokio::test]
    async fn missing_skill_name_field_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("load_skill"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    // ── Trust-gating tests (#6050) ──────────────────────────────────────────

    #[tokio::test]
    async fn blocked_skill_is_refused_without_body_read() {
        let dir = tempfile::tempdir().unwrap();
        let body = "secret body that should not be returned";
        let registry = make_registry_with_skill(dir.path(), "blocked-skill", body);
        let trust = HashMap::from([("blocked-skill".to_owned(), SkillTrustLevel::Blocked)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call("blocked-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("blocked by policy"));
        assert!(!result.summary.contains("secret body"));
    }

    #[tokio::test]
    async fn verified_skill_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let body = "Normal body <|im_start|>injected";
        let registry = make_registry_with_skill(dir.path(), "verified-skill", body);
        let trust = HashMap::from([("verified-skill".to_owned(), SkillTrustLevel::Verified)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call("verified-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("Normal body"));
        assert!(result.summary.contains("[BLOCKED:<|im_start|>]"));
        assert!(
            !result
                .summary
                .replace("[BLOCKED:<|im_start|>]", "")
                .contains("<|im_start|>")
        );
    }

    #[tokio::test]
    async fn quarantined_skill_is_sanitized_and_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let body = "Quarantined content";
        let registry = make_registry_with_skill(dir.path(), "quarantined-skill", body);
        let trust = HashMap::from([("quarantined-skill".to_owned(), SkillTrustLevel::Quarantined)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call("quarantined-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("QUARANTINED"));
        assert!(result.summary.contains("Quarantined content"));
    }

    #[tokio::test]
    async fn trusted_skill_returns_body_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let body = "## Instructions\nDo trusted things";
        let registry = make_registry_with_skill(dir.path(), "trusted-skill", body);
        let trust = HashMap::from([("trusted-skill".to_owned(), SkillTrustLevel::Trusted)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call("trusted-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("## Instructions"));
        assert!(result.summary.contains("Do trusted things"));
    }

    #[tokio::test]
    async fn no_trust_row_defaults_to_trusted_behavior() {
        // A missing trust-map entry means "never classified yet", not "known untrusted" — it
        // must resolve to Trusted (SkillTrustLevel::MISSING_ENTRY_FALLBACK). Falling back to
        // Quarantined here would spuriously wrap legitimate skills whenever the trust map is
        // transiently empty.
        let dir = tempfile::tempdir().unwrap();
        let body = "Some body";
        let registry = make_registry_with_skill(dir.path(), "unknown-skill", body);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("unknown-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(!result.summary.contains("QUARANTINED"));
        assert!(result.summary.contains(body));
    }

    #[tokio::test]
    async fn not_found_error_sanitizes_skill_name() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("<|im_start|>nonexistent"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("skill not found"));
        assert!(result.summary.contains("[BLOCKED:<|im_start|>]"));
        assert!(
            !result
                .summary
                .replace("[BLOCKED:<|im_start|>]", "")
                .contains("<|im_start|>")
        );
    }

    #[tokio::test]
    async fn tampered_requires_trust_check_skill_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        let body = "## Original body";
        let registry = make_registry_with_skill(dir.path(), "tampered-skill", body);
        let snapshots = HashMap::from([(
            "tampered-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
            },
        )]);
        let executor = SkillLoaderExecutor::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(snapshots)),
        );
        let result = executor
            .execute_tool_call(&make_call("tampered-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.summary.contains("demoted to Quarantined"),
            "output must mention demotion: {}",
            result.summary
        );
        assert!(
            !result.summary.contains("Original body"),
            "body must not be returned on hash mismatch: {}",
            result.summary
        );
    }
}
