// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool executor that returns a skill body as tool output with trust-aware sanitization.
//!
//! [`SkillInvokeExecutor`] implements `invoke_skill` — a native tool the LLM can call to
//! retrieve and immediately act under a skill's instructions. Unlike `load_skill` (which is
//! intent-neutral preview), `invoke_skill` carries intent-to-apply semantics: the next turn
//! is expected to follow the returned skill body.
//!
//! The shared `SkillTrustGate` (crate-private) applies the same defense-in-depth
//! pipeline as `format_skills_prompt` (and as `load_skill`'s
//! [`SkillLoaderExecutor`](crate::skill_loader::SkillLoaderExecutor)):
//! - Non-Trusted bodies pass through [`sanitize_skill_text`].
//! - Quarantined bodies are additionally wrapped with [`wrap_quarantined`](zeph_skills::prompt::wrap_quarantined).
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
use zeph_skills::prompt::sanitize_skill_text;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{
    ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params, truncate_tool_output,
};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::skill_trust_gate::{SkillBodyResolution, SkillTrustGate};

/// Per-invocation trust metadata snapshot for a single skill.
///
/// Populated once per turn from the trust DB by `build_skill_trust_map` and shared
/// with `SkillInvokeExecutor` so it can resolve trust without hitting `SQLite` on each
/// tool call. When `requires_trust_check` is `true`, `execute_tool_call` re-hashes
/// the skill's `SKILL.md` before dispatch (tamper detection per #4293).
#[derive(Clone, Debug)]
pub struct SkillTrustSnapshot {
    /// Access level governing which tools the skill may invoke.
    pub trust_level: SkillTrustLevel,
    /// Whether to re-hash `SKILL.md` on every invocation and abort if the digest changed.
    pub requires_trust_check: bool,
    /// blake3 hex hash of `SKILL.md` recorded at trust-grant time.
    pub blake3_hash: String,
}

/// Projects a [`SkillTrustSnapshot`] map down to just [`SkillTrustLevel`] — used by prompt
/// formatting functions (`format_skills_prompt`, `format_skills_catalog`) that only need the
/// level, not `requires_trust_check`/`blake3_hash`.
pub(crate) fn snapshot_map_to_trust_levels(
    map: &HashMap<String, SkillTrustSnapshot>,
) -> HashMap<String, SkillTrustLevel> {
    map.iter()
        .map(|(k, v)| (k.clone(), v.trust_level))
        .collect()
}

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
/// Delegates trust resolution, integrity checking, sanitization, and quarantine wrapping to a
/// shared `SkillTrustGate` (crate-private) — the same pipeline `load_skill`
/// ([`crate::skill_loader::SkillLoaderExecutor`]) uses, so the two tools cannot drift apart.
#[derive(Clone, Debug)]
pub struct SkillInvokeExecutor {
    gate: SkillTrustGate,
}

impl SkillInvokeExecutor {
    /// Create a new executor with shared registry and trust snapshot.
    ///
    /// Both `Arc`s must be the same instances held by the agent — and shared with
    /// `SkillLoaderExecutor` — so updates are visible without re-constructing the executor.
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        trust_snapshot: Arc<RwLock<HashMap<String, SkillTrustSnapshot>>>,
    ) -> Self {
        Self {
            gate: SkillTrustGate::new(registry, trust_snapshot),
        }
    }

    /// Wires the shared per-turn trust floor (#6701) so an `invoke_skill` of a Quarantined
    /// body folds the turn's trust down for its remainder — see
    /// [`SkillTrustGate::with_turn_trust_floor`].
    #[must_use]
    pub fn with_turn_trust_floor(mut self, turn_trust_floor: zeph_common::TurnTrustFloor) -> Self {
        self.gate = self.gate.with_turn_trust_floor(turn_trust_floor);
        self
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
            server_id: None,
        }]
    }

    #[tracing::instrument(name = "core.skill_invoke.execute", skip_all, fields(skill = tracing::field::Empty))]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "invoke_skill" {
            return Ok(None);
        }
        let params: InvokeSkillParams = deserialize_params(&call.params)?;
        let skill_name: String = params.skill_name.chars().take(128).collect();

        tracing::Span::current().record("skill", skill_name.as_str());

        let summary = match self.gate.resolve_body(&skill_name).await? {
            SkillBodyResolution::Refused(message) | SkillBodyResolution::NotFound(message) => {
                message
            }
            SkillBodyResolution::Body(wrapped) => {
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
        };

        Ok(Some(make_output(summary)))
    }

    zeph_tools::tool_executor_no_inner_defaults!();
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
        ..Default::default()
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

    fn make_snapshot(level: SkillTrustLevel) -> SkillTrustSnapshot {
        SkillTrustSnapshot {
            trust_level: level,
            requires_trust_check: false,
            blake3_hash: String::new(),
        }
    }

    fn make_executor(
        registry: SkillRegistry,
        trust_map: HashMap<String, SkillTrustLevel>,
    ) -> SkillInvokeExecutor {
        let snapshot_map: HashMap<String, SkillTrustSnapshot> = trust_map
            .into_iter()
            .map(|(k, v)| (k, make_snapshot(v)))
            .collect();
        SkillInvokeExecutor::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(snapshot_map)),
        )
    }

    fn make_executor_with_snapshots(
        registry: SkillRegistry,
        snapshots: HashMap<String, SkillTrustSnapshot>,
    ) -> SkillInvokeExecutor {
        SkillInvokeExecutor::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(RwLock::new(snapshots)),
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

            tool_call_id: String::new(),
            skill_name: None,
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

            tool_call_id: String::new(),
            skill_name: None,
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
    async fn no_trust_row_defaults_to_trusted_behavior() {
        // A missing trust-map entry means "never classified yet", not "known untrusted" —
        // it must resolve to Trusted (SkillTrustLevel::MISSING_ENTRY_FALLBACK), matching the
        // documented contract in zeph-common. Falling back to Quarantined here would
        // spuriously wrap legitimate skills whenever the trust map is transiently empty.
        let dir = tempfile::tempdir().unwrap();
        let body = "Some body";
        let registry = make_registry_with_skill(dir.path(), "unknown-skill", body);
        let executor = make_executor(registry, HashMap::new());
        let result = executor
            .execute_tool_call(&make_call("unknown-skill"))
            .await
            .unwrap()
            .unwrap();
        // Trusted path: body is returned unwrapped.
        assert!(!result.summary.contains("QUARANTINED"));
        assert!(result.summary.contains(body));
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

            tool_call_id: String::new(),
            skill_name: None,
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

    // ── Per-invocation trust check tests ────────────────────────────────────

    #[tokio::test]
    async fn hash_match_passes_normally() {
        let dir = tempfile::tempdir().unwrap();
        let body = "## Trusted body";
        let registry = make_registry_with_skill(dir.path(), "checked-skill", body);
        let skill_dir = dir.path().join("checked-skill");
        let stored_hash = zeph_skills::trust::compute_skill_hash(&skill_dir).unwrap();
        let snapshots = HashMap::from([(
            "checked-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: stored_hash,
            },
        )]);
        let executor = make_executor_with_snapshots(registry, snapshots);
        let result = executor
            .execute_tool_call(&make_call("checked-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.summary.contains("Trusted body"),
            "body returned on hash match"
        );
    }

    #[tokio::test]
    async fn hash_mismatch_demotes_to_quarantined_and_aborts() {
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
        let snapshot_arc = Arc::new(RwLock::new(snapshots));
        let executor =
            SkillInvokeExecutor::new(Arc::new(RwLock::new(registry)), Arc::clone(&snapshot_arc));
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
            "body must not be returned on hash mismatch"
        );
        // Snapshot entry must be demoted in memory.
        let level = snapshot_arc
            .read()
            .get("tampered-skill")
            .map(|s| s.trust_level);
        assert_eq!(level, Some(SkillTrustLevel::Quarantined));
    }

    #[tokio::test]
    async fn requires_trust_check_false_skips_hash() {
        // When requires_trust_check=false, even a deliberately wrong hash must NOT block.
        let dir = tempfile::tempdir().unwrap();
        let body = "## Body without check";
        let registry = make_registry_with_skill(dir.path(), "no-check-skill", body);
        let snapshots = HashMap::from([(
            "no-check-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: false,
                blake3_hash: "wrong_hash_that_would_fail_if_checked".to_owned(),
            },
        )]);
        let executor = make_executor_with_snapshots(registry, snapshots);
        let result = executor
            .execute_tool_call(&make_call("no-check-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.summary.contains("Body without check"),
            "body must be returned when check disabled"
        );
    }

    #[tokio::test]
    async fn requires_trust_check_true_empty_hash_aborts_with_distinct_error() {
        // Legacy DB row or misconfiguration: requires_trust_check=true but blake3_hash is empty.
        // Must abort with a distinct diagnostic, not "hash mismatch".
        let dir = tempfile::tempdir().unwrap();
        let body = "## Some body";
        let registry = make_registry_with_skill(dir.path(), "legacy-skill", body);
        let snapshots = HashMap::from([(
            "legacy-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: String::new(), // empty — legacy row
            },
        )]);
        let executor = make_executor_with_snapshots(registry, snapshots);
        let result = executor
            .execute_tool_call(&make_call("legacy-skill"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.summary.contains("no stored hash found"),
            "must emit distinct error for missing hash: {}",
            result.summary
        );
        assert!(
            !result.summary.contains("demoted to Quarantined"),
            "must not emit mismatch message for missing hash: {}",
            result.summary
        );
        assert!(
            !result.summary.contains("Some body"),
            "body must not be returned: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn skill_dir_none_aborts_invocation() {
        // Skill is in the snapshot with requires_trust_check=true but not in registry.
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let snapshots = HashMap::from([(
            "ghost-skill".to_owned(),
            SkillTrustSnapshot {
                trust_level: SkillTrustLevel::Trusted,
                requires_trust_check: true,
                blake3_hash: "deadbeef".to_owned(),
            },
        )]);
        let executor = make_executor_with_snapshots(registry, snapshots);
        let result = executor
            .execute_tool_call(&make_call("ghost-skill"))
            .await
            .unwrap()
            .unwrap();
        // Fail-closed: skill_dir not found → abort.
        assert!(
            result.summary.contains("skill directory not found")
                || result.summary.contains("skill not found"),
            "must abort when skill_dir is missing: {}",
            result.summary
        );
    }
}
