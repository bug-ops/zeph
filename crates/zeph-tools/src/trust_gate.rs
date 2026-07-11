// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust-level enforcement layer for tool execution.

use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use parking_lot::RwLock;

use crate::SkillTrustLevel;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::permissions::{AutonomyLevel, PermissionAction, PermissionPolicy};
use crate::registry::ToolDef;

/// Tools denied when a Quarantined skill is active.
///
/// Re-exported from `zeph_common::quarantine::QUARANTINE_DENIED` — the canonical definition
/// lives there so both `zeph-skills` and `zeph-tools` can reference it without a dependency
/// cycle.
pub use zeph_common::quarantine::QUARANTINE_DENIED;

pub(crate) fn is_quarantine_denied(tool_id: &str) -> bool {
    QUARANTINE_DENIED
        .iter()
        .any(|denied| tool_id == *denied || tool_id.ends_with(&format!("_{denied}")))
}

/// Builds the denial message for a Quarantined-trust block.
///
/// `active_skills` is the turn's full active-skill list (`ToolCall::skill_name`), not just
/// the specific skill(s) whose trust caused the fold — `TrustGateExecutor` only tracks the
/// already-folded `effective_trust`, not per-skill levels, so it cannot name exactly which
/// skill(s) are Quarantined, nor whether `tool_id`'s own target skill (e.g. `invoke_skill`'s
/// `skill_name` param) is among them. Naming the turn's active skill set instead of flatly
/// blaming `tool_id` resolves the misattribution from #5729 without asserting anything the
/// gate cannot verify: this denial means the turn's *combined* trust floor is quarantined
/// (weakest-link policy, see `assembly.rs` and this module's doc comment) — it may or may not
/// be about the specific tool/skill targeted by this call.
pub(crate) fn quarantine_denial_message(tool_id: &str, active_skills: &[String]) -> String {
    if active_skills.is_empty() {
        format!("{tool_id} denied (trust=quarantined)")
    } else {
        format!(
            "{tool_id} denied: this turn's active skill set {active_skills:?} has a combined \
             trust floor of quarantined (weakest-link policy over all co-active skills this \
             turn; this reflects the turn's overall trust floor and may not be about the \
             specific tool/skill you targeted)"
        )
    }
}

pub(crate) fn trust_to_u8(level: SkillTrustLevel) -> u8 {
    match level {
        SkillTrustLevel::Trusted => 0,
        SkillTrustLevel::Verified => 1,
        SkillTrustLevel::Quarantined => 2,
        _ => 3,
    }
}

pub(crate) fn u8_to_trust(v: u8) -> SkillTrustLevel {
    match v {
        0 => SkillTrustLevel::Trusted,
        1 => SkillTrustLevel::Verified,
        2 => SkillTrustLevel::Quarantined,
        _ => SkillTrustLevel::Blocked,
    }
}

/// Wraps an inner `ToolExecutor` and applies trust-level permission overlays.
pub struct TrustGateExecutor<T: ToolExecutor> {
    inner: T,
    policy: PermissionPolicy,
    effective_trust: AtomicU8,
    /// Sanitized IDs of all registered MCP tools. When a Quarantined skill is
    /// active, any tool whose ID appears in this set is denied — regardless of
    /// whether its name matches `QUARANTINE_DENIED`. Populated at startup by
    /// calling `set_mcp_tool_ids` after MCP servers connect.
    mcp_tool_ids: Arc<RwLock<HashSet<String>>>,
}

impl<T: ToolExecutor + std::fmt::Debug> std::fmt::Debug for TrustGateExecutor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustGateExecutor")
            .field("inner", &self.inner)
            .field("policy", &self.policy)
            .field("effective_trust", &self.effective_trust())
            .field("mcp_tool_ids", &self.mcp_tool_ids)
            .finish()
    }
}

impl<T: ToolExecutor> TrustGateExecutor<T> {
    #[must_use]
    pub fn new(inner: T, policy: PermissionPolicy) -> Self {
        Self {
            inner,
            policy,
            effective_trust: AtomicU8::new(trust_to_u8(SkillTrustLevel::Trusted)),
            mcp_tool_ids: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Returns the shared MCP tool ID set so the caller can populate it after
    /// MCP servers have connected (and after `TrustGateExecutor` has been wrapped
    /// in a `DynExecutor`).
    #[must_use]
    pub fn mcp_tool_ids_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.mcp_tool_ids)
    }

    pub fn set_effective_trust(&self, level: SkillTrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), Ordering::Relaxed);
    }

    #[must_use]
    pub fn effective_trust(&self) -> SkillTrustLevel {
        u8_to_trust(self.effective_trust.load(Ordering::Relaxed))
    }

    fn is_mcp_tool(&self, tool_id: &str) -> bool {
        self.mcp_tool_ids.read().contains(tool_id)
    }

    /// Enforces per-call trust policy.
    ///
    /// `effective_trust` (see [`set_effective_trust`](Self::set_effective_trust)) is a single
    /// value folded via `SkillTrustLevel::min_trust` across ALL skills active in the current
    /// turn — computed in `zeph_core::agent::context::assembly`. This is a deliberate
    /// weakest-link policy: if ANY skill active this turn is Quarantined,
    /// [`QUARANTINE_DENIED`] tools (including `invoke_skill`/`load_skill`) are denied for the
    /// WHOLE turn, regardless of which specific skill/tool a call targets — this guards
    /// against a Quarantined (potentially prompt-injected) skill's content steering the model
    /// into invoking other tools/skills as a side channel. See #5729 for the resulting UX gap
    /// (an unrelated, non-quarantined skill's own `invoke_skill` call is also denied) and why
    /// the policy itself is intentionally kept — `active_skills` is used only to make the
    /// denial message name the turn's active skill set instead of misattributing the block to
    /// `tool_id` itself.
    fn check_trust(
        &self,
        tool_id: &str,
        input: &str,
        active_skills: &[String],
    ) -> Result<(), ToolError> {
        match self.effective_trust() {
            SkillTrustLevel::Blocked => {
                return Err(ToolError::Blocked {
                    command: "all tools blocked (trust=blocked)".to_owned(),
                });
            }
            SkillTrustLevel::Quarantined
                if is_quarantine_denied(tool_id) || self.is_mcp_tool(tool_id) =>
            {
                return Err(ToolError::Blocked {
                    command: quarantine_denial_message(tool_id, active_skills),
                });
            }
            _ => {}
        }

        // PermissionPolicy was designed for the bash tool. In Supervised mode, tools
        // without explicit rules default to Ask, which incorrectly blocks MCP/LSP tools
        // and native read-only tools (both are already categorized elsewhere: MCP/LSP
        // tools via `mcp_tool_ids`, read-only native tools via `permissions::READONLY_TOOLS`).
        // Skip the policy check only for tools that fall into one of those two known-safe
        // categories — trust-level enforcement above is sufficient for them. Any other
        // unconfigured tool (e.g. `diagnostics`, which runs cargo check/clippy and can
        // execute arbitrary code via build.rs/proc-macros) falls through to the Ask
        // default below; see #5575.
        // ReadOnly mode is excluded: its allowlist is enforced inside policy.check().
        if self.policy.autonomy_level() == AutonomyLevel::Supervised
            && self.policy.rules().get(tool_id).is_none()
            && (self.is_mcp_tool(tool_id) || crate::permissions::is_readonly_tool(tool_id))
        {
            return Ok(());
        }

        match self.policy.check(tool_id, input) {
            PermissionAction::Allow => Ok(()),
            PermissionAction::Ask => Err(ToolError::ConfirmationRequired {
                command: input.to_owned(),
            }),
            _ => Err(ToolError::Blocked {
                command: input.to_owned(),
            }),
        }
    }
}

impl<T: ToolExecutor> ToolExecutor for TrustGateExecutor<T> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        // The legacy fenced-block path does not provide a tool_id, so QUARANTINE_DENIED
        // cannot be applied selectively. Block entirely for Quarantined to match the
        // conservative posture: unknown tool identity = deny.
        match self.effective_trust() {
            SkillTrustLevel::Blocked | SkillTrustLevel::Quarantined => {
                return Err(ToolError::Blocked {
                    command: format!(
                        "tool execution denied (trust={})",
                        format!("{:?}", self.effective_trust()).to_lowercase()
                    ),
                });
            }
            _ => {}
        }
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        // Same rationale as execute(): no tool_id available for QUARANTINE_DENIED check.
        match self.effective_trust() {
            SkillTrustLevel::Blocked | SkillTrustLevel::Quarantined => {
                return Err(ToolError::Blocked {
                    command: format!(
                        "tool execution denied (trust={})",
                        format!("{:?}", self.effective_trust()).to_lowercase()
                    ),
                });
            }
            _ => {}
        }
        self.inner.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let input = call
            .params
            .get("command")
            .or_else(|| call.params.get("file_path"))
            .or_else(|| call.params.get("query"))
            .or_else(|| call.params.get("url"))
            .or_else(|| call.params.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.check_trust(
            call.tool_id.as_str(),
            input,
            call.skill_name.as_deref().unwrap_or(&[]),
        )?;
        self.inner.execute_tool_call(call).await
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        // Bypass check_trust: caller already obtained user approval.
        // Still enforce Blocked/Quarantined trust level constraints. This match intentionally
        // mirrors check_trust's Blocked/Quarantined branches above — keep the two in sync.
        match self.effective_trust() {
            SkillTrustLevel::Blocked => {
                return Err(ToolError::Blocked {
                    command: "all tools blocked (trust=blocked)".to_owned(),
                });
            }
            SkillTrustLevel::Quarantined
                if is_quarantine_denied(call.tool_id.as_str())
                    || self.is_mcp_tool(call.tool_id.as_str()) =>
            {
                return Err(ToolError::Blocked {
                    command: quarantine_denial_message(
                        call.tool_id.as_str(),
                        call.skill_name.as_deref().unwrap_or(&[]),
                    ),
                });
            }
            _ => {}
        }
        self.inner.execute_tool_call_confirmed(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_speculatable(tool_id)
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

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), Ordering::Relaxed);
    }

    /// Returns `true` when the current policy would require confirmation for `call`.
    ///
    /// Mirrors the decision in [`execute_tool_call`](Self::execute_tool_call) without
    /// executing the tool. The speculative engine calls this to skip dispatch for tools
    /// that require user approval.
    fn requires_confirmation(&self, call: &crate::executor::ToolCall) -> bool {
        let input = call
            .params
            .get("command")
            .or_else(|| call.params.get("file_path"))
            .or_else(|| call.params.get("query"))
            .or_else(|| call.params.get("url"))
            .or_else(|| call.params.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        matches!(
            self.check_trust(
                call.tool_id.as_str(),
                input,
                call.skill_name.as_deref().unwrap_or(&[]),
            ),
            Err(ToolError::ConfirmationRequired { .. })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[derive(Debug)]
    struct MockExecutor;
    impl ToolExecutor for MockExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
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

        crate::tool_executor_no_inner_defaults!();
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn make_call_with_cmd(tool_id: &str, cmd: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("command".into(), serde_json::Value::String(cmd.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn make_call_with_skills(tool_id: &str, skills: &[&str]) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: Some(skills.iter().map(ToString::to_string).collect()),
        }
    }

    fn blocked_command(result: Result<Option<ToolOutput>, ToolError>) -> String {
        match result {
            Err(ToolError::Blocked { command }) => command,
            other => panic!("expected Err(ToolError::Blocked {{ .. }}), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn supervised_readonly_native_tool_without_rule_allowed() {
        // "read" is a native read-only tool (permissions::READONLY_TOOLS) — it must
        // still bypass the Ask default in Supervised mode even without an explicit rule.
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("read")).await;
        assert!(result.is_ok());
    }

    /// Regression test for #5575: `bash` has no explicit policy rule and is neither an
    /// MCP tool nor a native read-only tool, so it must NOT bypass confirmation in
    /// Supervised mode — the prior blanket skip incorrectly allowed this.
    #[tokio::test]
    async fn supervised_unconfigured_non_mcp_non_readonly_tool_requires_confirmation() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert_matches!(result, Err(ToolError::ConfirmationRequired { .. }));
    }

    /// Regression test for #5575: `diagnostics` runs `cargo check`/`cargo clippy`, which
    /// executes arbitrary code via `build.rs` scripts and proc-macros — it must require
    /// confirmation in Supervised mode when no explicit rule is configured, not bypass it
    /// via the (now-removed) blanket "no rule => Ok" skip.
    #[tokio::test]
    async fn supervised_unconfigured_diagnostics_requires_confirmation() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("diagnostics")).await;
        assert_matches!(result, Err(ToolError::ConfirmationRequired { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_bash() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_write() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("write")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_edit() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("edit")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_delete_path() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("delete_path")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_fetch() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("fetch")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_memory_save() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("memory_save")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    /// Regression test for #5433: `diagnostics` runs `cargo check`/`cargo clippy`, which
    /// executes arbitrary code via `build.rs` scripts and proc-macros in the target
    /// workspace — equivalent to `bash` for security purposes. Now that #5433 wires
    /// `DiagnosticsExecutor` into the live, `TrustGateExecutor`-gated composite chain, it
    /// must be quarantine-denied like `bash`.
    #[tokio::test]
    async fn quarantined_denies_diagnostics() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("diagnostics")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_allows_read() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        // "read" (file read) is not in QUARANTINE_DENIED — should be allowed
        let result = gate.execute_tool_call(&make_call("read")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn quarantined_allows_file_read() {
        // "file_read" is not in the quarantine-denied list, but (unlike "read") it is also
        // not in `permissions::READONLY_TOOLS`, so an explicit Allow rule is required here
        // to isolate this test from the Supervised-mode Ask default (see #5575) and keep it
        // focused on quarantine-denial behavior only.
        let mut rules = std::collections::HashMap::new();
        rules.insert(
            "file_read".to_owned(),
            vec![crate::permissions::PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            }],
        );
        let policy = crate::permissions::PermissionPolicy::new(rules);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        // file_read is not in quarantine denied list, and the explicit rule allows it => Ok
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn blocked_denies_everything() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn policy_deny_overrides_trust() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo rm"))
            .await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn blocked_denies_execute() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute("some response").await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn blocked_denies_execute_confirmed() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute_confirmed("some response").await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn trusted_allows_execute() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate.execute("some response").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn verified_with_allow_policy_succeeds() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Verified);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "echo hi"))
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn quarantined_denies_web_scrape() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("web_scrape")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    /// Regression test for #5729: the denial message must name the turn's actual co-active
    /// skill set instead of implying `invoke_skill` itself is the untrusted party. The gate
    /// behavior (deny) is unchanged — only the message wording is under test here.
    #[tokio::test]
    async fn quarantined_denial_message_names_active_skills_not_target_tool() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let call =
            make_call_with_skills("invoke_skill", &["disk-usage", "persona-customer-support"]);
        let result = gate.execute_tool_call(&call).await;
        let message = blocked_command(result);

        assert!(
            message.contains("disk-usage") && message.contains("persona-customer-support"),
            "message should name the actual active skills, got: {message}"
        );
        assert_ne!(
            message, "invoke_skill denied (trust=quarantined)",
            "message must not read as if invoke_skill itself is the untrusted party"
        );
    }

    /// Regression test for #5729: when no active skills are recorded on the call
    /// (`skill_name: None`), the denial message must remain exactly the pre-fix format —
    /// this guards backward compatibility for all pre-existing tests in this module, which
    /// all use `make_call`/`make_call_with_cmd` (both hardcode `skill_name: None`).
    #[tokio::test]
    async fn quarantined_denial_message_unchanged_when_no_active_skills() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("invoke_skill")).await;
        let message = blocked_command(result);

        assert_eq!(message, "invoke_skill denied (trust=quarantined)");
    }

    /// Same as `quarantined_denial_message_unchanged_when_no_active_skills`, but with an
    /// explicit empty skill list rather than `None` — both must produce the old message.
    #[tokio::test]
    async fn quarantined_denial_message_unchanged_when_active_skills_empty() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call(&make_call_with_skills("invoke_skill", &[]))
            .await;
        let message = blocked_command(result);

        assert_eq!(message, "invoke_skill denied (trust=quarantined)");
    }

    /// Regression test for #5729 via the `execute_tool_call_confirmed` path, which has its
    /// own duplicate inline Quarantined check rather than routing through `check_trust`.
    #[tokio::test]
    async fn quarantined_denial_message_names_active_skills_confirmed_path() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let call =
            make_call_with_skills("invoke_skill", &["disk-usage", "persona-customer-support"]);
        let result = gate.execute_tool_call_confirmed(&call).await;
        let message = blocked_command(result);

        assert!(
            message.contains("disk-usage") && message.contains("persona-customer-support"),
            "confirmed path message should name the actual active skills, got: {message}"
        );
        assert_ne!(
            message, "invoke_skill denied (trust=quarantined)",
            "confirmed path message must not read as if invoke_skill itself is untrusted"
        );
    }

    /// Regression test for #5729: the message fix must apply uniformly to any
    /// `QUARANTINE_DENIED` tool, not just `invoke_skill` — verified here with `bash`.
    #[tokio::test]
    async fn quarantined_denial_message_names_active_skills_for_non_skill_tool() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let call = make_call_with_skills("bash", &["disk-usage", "persona-customer-support"]);
        let result = gate.execute_tool_call(&call).await;
        let message = blocked_command(result);

        assert!(
            message.contains("disk-usage") && message.contains("persona-customer-support"),
            "message for a non-skill tool should also name the active skills, got: {message}"
        );
        assert_ne!(message, "bash denied (trust=quarantined)");
    }

    #[derive(Debug)]
    struct EnvCapture {
        captured: std::sync::Mutex<Option<std::collections::HashMap<String, String>>>,
    }
    impl EnvCapture {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }
    }
    impl ToolExecutor for EnvCapture {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(&self, _: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
            *self.captured.lock().unwrap() = env;
        }

        crate::tool_executor_no_inner_defaults!();
    }

    #[test]
    fn is_tool_retryable_delegated_to_inner() {
        #[derive(Debug)]
        struct RetryableExecutor;
        impl ToolExecutor for RetryableExecutor {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_retryable(&self, tool_id: &str) -> bool {
                tool_id == "fetch"
            }

            crate::tool_executor_no_inner_defaults!();
        }
        let gate = TrustGateExecutor::new(RetryableExecutor, PermissionPolicy::default());
        assert!(gate.is_tool_retryable("fetch"));
        assert!(!gate.is_tool_retryable("bash"));
    }

    #[test]
    fn checkpoint_methods_delegated_to_inner() {
        #[derive(Debug)]
        struct CheckpointingExecutor;
        impl ToolExecutor for CheckpointingExecutor {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn checkpoint_undo(&self, n: usize) -> crate::executor::CheckpointActionResult {
                crate::executor::CheckpointActionResult {
                    supported: true,
                    message: "stub".into(),
                    reverted_commands: n,
                    ..Default::default()
                }
            }
            fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
                crate::executor::CheckpointActionResult {
                    supported: true,
                    message: "stub".into(),
                    ..Default::default()
                }
            }
            fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
                crate::executor::CheckpointListResult {
                    supported: true,
                    ..Default::default()
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
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
                false
            }
        }
        let gate = TrustGateExecutor::new(CheckpointingExecutor, PermissionPolicy::default());
        let undo_result = gate.checkpoint_undo(7);
        assert!(undo_result.supported);
        assert_eq!(
            undo_result.reverted_commands, 7,
            "n must be forwarded, not hardcoded"
        );
        assert!(gate.checkpoint_redo().supported);
        assert!(gate.checkpoint_list().supported);
    }

    #[test]
    fn set_skill_env_forwarded_to_inner() {
        let inner = EnvCapture::new();
        let gate = TrustGateExecutor::new(inner, PermissionPolicy::default());

        let mut env = std::collections::HashMap::new();
        env.insert("MY_VAR".to_owned(), "42".to_owned());
        gate.set_skill_env(Some(env.clone()));

        let captured = gate.inner.captured.lock().unwrap();
        assert_eq!(*captured, Some(env));
    }

    #[tokio::test]
    async fn mcp_tool_supervised_no_rules_allows() {
        // MCP tool with Supervised mode + from_legacy policy (no rules for MCP tool) => Ok.
        // Registered via `mcp_tool_ids_handle` so `is_mcp_tool` recognizes it as genuinely
        // MCP-sourced — see #5575 (the skip is no longer a blanket "no rule => Ok").
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);
        gate.mcp_tool_ids_handle()
            .write()
            .insert("mcp_filesystem__read_file".to_owned());

        let mut params = serde_json::Map::new();
        params.insert(
            "file_path".into(),
            serde_json::Value::String("/tmp/test.txt".into()),
        );
        let call = ToolCall {
            tool_id: "mcp_filesystem__read_file".into(),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = gate.execute_tool_call(&call).await;
        assert!(
            result.is_ok(),
            "MCP tool should be allowed when no rules exist"
        );
    }

    #[tokio::test]
    async fn bash_with_explicit_deny_rule_blocked() {
        // Bash with explicit Deny rule => Err(ToolCallBlocked)
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo apt install vim"))
            .await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "bash with explicit deny rule should be blocked"
        );
    }

    #[tokio::test]
    async fn bash_with_explicit_allow_rule_succeeds() {
        // Tool with explicit Allow rules => Ok
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "echo hello"))
            .await;
        assert!(
            result.is_ok(),
            "bash with explicit allow rule should succeed"
        );
    }

    #[tokio::test]
    async fn readonly_denies_mcp_tool_not_in_allowlist() {
        // ReadOnly mode must deny tools not in READONLY_TOOLS, even MCP ones.
        let policy =
            crate::permissions::PermissionPolicy::default().with_autonomy(AutonomyLevel::ReadOnly);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call("mcpls_get_diagnostics"))
            .await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "ReadOnly mode must deny non-allowlisted tools"
        );
    }

    #[test]
    fn set_effective_trust_interior_mutability() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        assert_eq!(gate.effective_trust(), SkillTrustLevel::Trusted);

        gate.set_effective_trust(SkillTrustLevel::Quarantined);
        assert_eq!(gate.effective_trust(), SkillTrustLevel::Quarantined);

        gate.set_effective_trust(SkillTrustLevel::Blocked);
        assert_eq!(gate.effective_trust(), SkillTrustLevel::Blocked);

        gate.set_effective_trust(SkillTrustLevel::Trusted);
        assert_eq!(gate.effective_trust(), SkillTrustLevel::Trusted);
    }

    // is_quarantine_denied unit tests

    #[test]
    fn is_quarantine_denied_exact_match() {
        assert!(is_quarantine_denied("bash"));
        assert!(is_quarantine_denied("write"));
        assert!(is_quarantine_denied("fetch"));
        assert!(is_quarantine_denied("memory_save"));
        assert!(is_quarantine_denied("delete_path"));
        assert!(is_quarantine_denied("create_directory"));
        assert!(is_quarantine_denied("diagnostics"));
    }

    #[test]
    fn is_quarantine_denied_suffix_match_mcp_write() {
        // "filesystem_write" ends with "_write" -> denied
        assert!(is_quarantine_denied("filesystem_write"));
        // "filesystem_write_file" ends with "_file", not "_write" -> NOT denied
        assert!(!is_quarantine_denied("filesystem_write_file"));
    }

    #[test]
    fn is_quarantine_denied_suffix_mcp_bash() {
        assert!(is_quarantine_denied("shell_bash"));
        assert!(is_quarantine_denied("mcp_shell_bash"));
    }

    #[test]
    fn is_quarantine_denied_suffix_mcp_fetch() {
        assert!(is_quarantine_denied("http_fetch"));
        // "server_prefetch" ends with "_prefetch", not "_fetch"
        assert!(!is_quarantine_denied("server_prefetch"));
    }

    #[test]
    fn is_quarantine_denied_suffix_mcp_memory_save() {
        assert!(is_quarantine_denied("server_memory_save"));
        // "_save" alone does NOT match the multi-word entry "memory_save"
        assert!(!is_quarantine_denied("server_save"));
    }

    #[test]
    fn is_quarantine_denied_suffix_mcp_delete_path() {
        assert!(is_quarantine_denied("fs_delete_path"));
        // "fs_not_delete_path" ends with "_delete_path" as well — suffix check is correct
        assert!(is_quarantine_denied("fs_not_delete_path"));
    }

    #[test]
    fn is_quarantine_denied_substring_not_suffix() {
        // "write_log" ends with "_log", NOT "_write" — must NOT be denied
        assert!(!is_quarantine_denied("write_log"));
    }

    #[test]
    fn is_quarantine_denied_read_only_tools_allowed() {
        assert!(!is_quarantine_denied("filesystem_read_file"));
        assert!(!is_quarantine_denied("filesystem_list_dir"));
        assert!(!is_quarantine_denied("read"));
        assert!(!is_quarantine_denied("file_read"));
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_write_tool() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("filesystem_write")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_allows_mcp_read_file() {
        // Deliberately NOT registered in `mcp_tool_ids`: a tool registered there is
        // denied outright under Quarantine regardless of read/write (see
        // `mcp_tool_ids` field docs), so this test isolates a different case — a
        // tool that merely looks MCP-like by name but isn't quarantine-denied by
        // `is_quarantine_denied`. An explicit Allow rule keeps it decoupled from the
        // Supervised-mode Ask default for unconfigured tools (see #5575).
        let mut rules = std::collections::HashMap::new();
        rules.insert(
            "filesystem_read_file".to_owned(),
            vec![crate::permissions::PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            }],
        );
        let policy = crate::permissions::PermissionPolicy::new(rules);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call(&make_call("filesystem_read_file"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_bash_tool() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("shell_bash")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_memory_save() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call(&make_call("server_memory_save"))
            .await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_confirmed_path() {
        // execute_tool_call_confirmed also enforces quarantine via is_quarantine_denied
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call_confirmed(&make_call("filesystem_write"))
            .await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    // mcp_tool_ids registry tests

    fn gate_with_mcp_ids(ids: &[&str]) -> TrustGateExecutor<MockExecutor> {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        let handle = gate.mcp_tool_ids_handle();
        let set: std::collections::HashSet<String> = ids.iter().map(ToString::to_string).collect();
        *handle.write() = set;
        gate
    }

    #[tokio::test]
    async fn quarantined_denies_registered_mcp_tool_novel_name() {
        // "github_run_command" has no QUARANTINE_DENIED suffix match, but is registered as MCP.
        let gate = gate_with_mcp_ids(&["github_run_command"]);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call(&make_call("github_run_command"))
            .await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_denies_registered_mcp_tool_execute() {
        // "shell_execute" — no suffix match on "execute", but registered as MCP.
        let gate = gate_with_mcp_ids(&["shell_execute"]);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("shell_execute")).await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[tokio::test]
    async fn quarantined_allows_unregistered_tool_not_in_denied_list() {
        // Tool not in MCP set and not in QUARANTINE_DENIED — allowed.
        let gate = gate_with_mcp_ids(&["other_tool"]);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("read")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn trusted_allows_registered_mcp_tool() {
        // At Trusted level, MCP registry check must NOT fire.
        let gate = gate_with_mcp_ids(&["github_run_command"]);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call("github_run_command"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_tool_via_confirmed_path() {
        // execute_tool_call_confirmed must also check the MCP registry.
        let gate = gate_with_mcp_ids(&["docker_container_exec"]);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call_confirmed(&make_call("docker_container_exec"))
            .await;
        assert_matches!(result, Err(ToolError::Blocked { .. }));
    }

    #[test]
    fn mcp_tool_ids_handle_shared_arc() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        let handle = gate.mcp_tool_ids_handle();
        handle.write().insert("test_tool".to_owned());
        assert!(gate.is_mcp_tool("test_tool"));
        assert!(!gate.is_mcp_tool("other_tool"));
    }

    // M9: document that the suffix matcher applies to MCP tools ending with
    // `_invoke_skill` or `_load_skill`. Future MCP tool authors should be aware.
    #[test]
    fn invoke_skill_and_load_skill_suffix_match_is_intentional() {
        // Exact-match branch: native tool IDs are denied.
        assert!(is_quarantine_denied("invoke_skill"));
        assert!(is_quarantine_denied("load_skill"));
        // Suffix-match branch: hypothetical MCP-prefixed versions are also denied.
        // This is intentional — prevents a renamed MCP wrapper from bypassing the gate.
        assert!(is_quarantine_denied("foo_invoke_skill"));
        assert!(is_quarantine_denied("foo_load_skill"));
    }
}
