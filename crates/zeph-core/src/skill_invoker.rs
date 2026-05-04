// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool executor that returns a skill body as tool output with trust-aware sanitization.
//!
//! [`SkillInvokeExecutor`] implements `invoke_skill` — a native tool the LLM can call to
//! retrieve and immediately act under a skill's instructions. Unlike `load_skill` (which is
//! intent-neutral preview), `invoke_skill` carries intent-to-apply semantics: the next turn
//! is expected to follow the returned skill body.
//!
//! The executor applies the same defense-in-depth pipeline as `format_skills_prompt`:
//! - Non-Trusted bodies pass through [`sanitize_skill_text`].
//! - Quarantined bodies are additionally wrapped with [`wrap_quarantined`].
//! - Blocked skills are refused before any body read.
//! - `args` are always sanitized regardless of trust level (LLM-chosen text).
//!
//! `invoke_skill` and `load_skill` are both listed in `QUARANTINE_DENIED`, so when a
//! Quarantined skill is active the trust gate refuses both before this executor is reached.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::Deserialize;
use zeph_common::SkillTrustLevel;
use zeph_skills::prompt::{sanitize_skill_text, wrap_quarantined};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params, truncate_tool_output,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

/// Parameters for the `invoke_skill` tool call.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InvokeSkillParams {
    /// Exact skill name from the `<other_skills>` catalog.
    pub skill_name: String,
    /// Optional free-form arguments forwarded verbatim to the skill body as a trailing
    /// `<args>…</args>` block. Capped at 4096 characters.
    #[serde(default)]
    pub args: String,
}

/// Tool executor that returns a skill body by name with trust-aware sanitization.
///
/// Holds a shared reference to the skill registry and a per-turn trust snapshot
/// refreshed by the agent loop. Both are cheap `Arc` clones — no allocation on hot path.
#[derive(Clone, Debug)]
pub struct SkillInvokeExecutor {
    registry: Arc<RwLock<SkillRegistry>>,
    /// Per-skill trust snapshot refreshed once per turn by the agent.
    /// Absence of an entry means no trust row exists — treat as Quarantined
    /// (see `SkillTrustLevel::default`).
    trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustLevel>>>,
}

impl SkillInvokeExecutor {
    /// Create a new executor with shared registry and trust snapshot.
    ///
    /// Both `Arc`s must be the same instances held by the agent so updates are
    /// visible without re-constructing the executor.
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustLevel>>>,
    ) -> Self {
        Self {
            registry,
            trust_snapshot,
        }
    }

    /// Resolve the trust level for a skill from the snapshot.
    ///
    /// Returns `SkillTrustLevel::default()` (Quarantined) when no row exists — fail-closed.
    fn resolve_trust(&self, skill_name: &str) -> SkillTrustLevel {
        self.trust_snapshot
            .read()
            .get(skill_name)
            .copied()
            .unwrap_or_default()
    }
}

impl ToolExecutor for SkillInvokeExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "invoke_skill".into(),
            description: "Invoke a skill by name. Returns the skill body as tool output; the \
                next turn should act under those instructions. Parameters: \
                skill_name (required) — exact name from <other_skills>; \
                args (optional) — <=4096 chars appended as <args>...</args>. \
                Use when a cataloged skill clearly matches the current task and you \
                intend to follow it in the next turn."
                .into(),
            schema: schemars::schema_for!(InvokeSkillParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "invoke_skill" {
            return Ok(None);
        }
        let params: InvokeSkillParams = deserialize_params(&call.params)?;
        let skill_name: String = params.skill_name.chars().take(128).collect();

        let trust = self.resolve_trust(&skill_name);
        // Sanitize skill_name before it appears in any tool output: it originates from the LLM
        // and could carry injection markers (e.g. `<|im_start|>`).
        let skill_name_safe = sanitize_skill_text(&skill_name);

        // Blocked skills are refused before any body read — executor defense layer.
        if trust == SkillTrustLevel::Blocked {
            return Ok(Some(make_output(format!(
                "skill is blocked by policy: {skill_name_safe}"
            ))));
        }

        // Clone body out of the read guard before any .await — never hold lock across await.
        let body = {
            let guard = self.registry.read();
            guard.body(&skill_name).map(str::to_owned)
        };

        let summary = match body {
            Ok(raw_body) => {
                // Apply the same pipeline as `format_skills_prompt:194-204`:
                // sanitize for non-Trusted, additionally wrap for Quarantined.
                let sanitized = if trust == SkillTrustLevel::Trusted {
                    raw_body
                } else {
                    sanitize_skill_text(&raw_body)
                };
                let wrapped = if trust == SkillTrustLevel::Quarantined {
                    wrap_quarantined(&skill_name_safe, &sanitized)
                } else {
                    sanitized
                };
                let full = if params.args.trim().is_empty() {
                    wrapped
                } else {
                    let args = params.args.chars().take(4096).collect::<String>();
                    // args originate from LLM text — sanitize regardless of trust.
                    let args_safe = sanitize_skill_text(&args);
                    format!("{wrapped}\n\n<args>\n{args_safe}\n</args>")
                };
                truncate_tool_output(&full)
            }
            Err(_) => format!("skill not found: {skill_name_safe}"),
        };

        Ok(Some(make_output(summary)))
    }
}

fn make_output(summary: String) -> ToolOutput {
    ToolOutput {
        tool_name: zeph_common::ToolName::new("invoke_skill"),
        summary,
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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

    fn make_executor(
        registry: SkillRegistry,
        trust_map: HashMap<String, SkillTrustLevel>,
    ) -> SkillInvokeExecutor {
        SkillInvokeExecutor::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(trust_map)),
        )
    }

    fn make_call(skill_name: &str) -> ToolCall {
        ToolCall {
            tool_id: zeph_common::ToolName::new("invoke_skill"),
            params: serde_json::json!({"skill_name": skill_name})
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,
        }
    }

    fn make_call_with_args(skill_name: &str, args: &str) -> ToolCall {
        ToolCall {
            tool_id: zeph_common::ToolName::new("invoke_skill"),
            params: serde_json::json!({"skill_name": skill_name, "args": args})
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,
        }
    }

    #[tokio::test]
    async fn trusted_skill_returns_body_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let body = "## Instructions\nDo trusted things";
        let registry = make_registry_with_skill(dir.path(), "my-skill", body);
        let trust = HashMap::from([("my-skill".to_owned(), SkillTrustLevel::Trusted)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call("my-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("## Instructions"));
        assert!(result.summary.contains("Do trusted things"));
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
        // The raw marker must only appear inside the [BLOCKED:...] wrapper, never standalone.
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
    async fn no_trust_row_defaults_to_quarantined_behavior() {
        // Default trust is Quarantined — fail-closed.
        let dir = tempfile::tempdir().unwrap();
        let body = "Some body";
        let registry = make_registry_with_skill(dir.path(), "unknown-skill", body);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("unknown-skill"))
            .await
            .unwrap()
            .unwrap();
        // Quarantined path: body is wrapped.
        assert!(result.summary.contains("QUARANTINED"));
    }

    #[tokio::test]
    async fn nonexistent_skill_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("nonexistent"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("skill not found"));
    }

    #[tokio::test]
    async fn wrong_tool_id_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("bash"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_always_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let result = executor.execute("any text").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn args_are_appended_to_trusted_body() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry_with_skill(dir.path(), "argskill", "Body text");
        let trust = HashMap::from([("argskill".to_owned(), SkillTrustLevel::Trusted)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call_with_args("argskill", "user arg"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("Body text"));
        assert!(result.summary.contains("<args>"));
        assert!(result.summary.contains("user arg"));
    }

    #[tokio::test]
    async fn args_are_sanitized_regardless_of_trust() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry_with_skill(dir.path(), "trustskill", "Body");
        let trust = HashMap::from([("trustskill".to_owned(), SkillTrustLevel::Trusted)]);
        let executor = make_executor(registry, trust);
        let result = executor
            .execute_tool_call(&make_call_with_args("trustskill", "<|im_start|>injected"))
            .await
            .unwrap()
            .unwrap();
        assert!(result.summary.contains("[BLOCKED:<|im_start|>]"));
        // The raw marker must only appear inside the [BLOCKED:...] wrapper, never standalone.
        assert!(
            !result
                .summary
                .replace("[BLOCKED:<|im_start|>]", "")
                .contains("<|im_start|>")
        );
    }

    #[tokio::test]
    async fn tool_definitions_returns_invoke_skill() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let executor = make_executor(registry, HashMap::new());
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "invoke_skill");
    }
}
