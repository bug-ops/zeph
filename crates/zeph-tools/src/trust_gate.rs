// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust-level enforcement layer for tool execution.

use std::collections::HashSet;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU8, Ordering},
};

use crate::SkillTrustLevel;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::permissions::{AutonomyLevel, PermissionAction, PermissionPolicy};
use crate::registry::ToolDef;

/// Tools denied when a Quarantined skill is active.
///
/// Uses the actual tool IDs registered by `FileExecutor` and other executors.
/// Previously contained `"file_write"` which matched nothing (dead rule).
///
/// MCP tools use a server-prefixed ID (e.g. `filesystem_write_file`). The
/// `is_quarantine_denied` predicate checks both exact matches and `_{entry}`
/// suffix matches to cover MCP-wrapped versions of these native tool IDs.
/// False positives (a safe tool whose name ends with a denied suffix) are
/// acceptable at the Quarantined trust level.
///
/// Public so that `zeph-skills::scanner::check_capability_escalation` can use
/// this as the single source of truth for quarantine-denied tools.
pub const QUARANTINE_DENIED: &[&str] = &[
    // Shell execution
    "bash",
    // File write/mutation tools (FileExecutor IDs)
    "write",
    "edit",
    "delete_path",
    "move_path",
    "copy_path",
    "create_directory",
    // Web access
    "web_scrape",
    "fetch",
    // Memory persistence
    "memory_save",
];

fn is_quarantine_denied(tool_id: &str) -> bool {
    QUARANTINE_DENIED
        .iter()
        .any(|denied| tool_id == *denied || tool_id.ends_with(&format!("_{denied}")))
}

fn trust_to_u8(level: SkillTrustLevel) -> u8 {
    match level {
        SkillTrustLevel::Trusted => 0,
        SkillTrustLevel::Verified => 1,
        SkillTrustLevel::Quarantined => 2,
        SkillTrustLevel::Blocked => 3,
    }
}

fn u8_to_trust(v: u8) -> SkillTrustLevel {
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
        self.mcp_tool_ids
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(tool_id)
    }

    fn check_trust(&self, tool_id: &str, input: &str) -> Result<(), ToolError> {
        match self.effective_trust() {
            SkillTrustLevel::Blocked => {
                return Err(ToolError::Blocked {
                    command: "all tools blocked (trust=blocked)".to_owned(),
                });
            }
            SkillTrustLevel::Quarantined => {
                if is_quarantine_denied(tool_id) || self.is_mcp_tool(tool_id) {
                    return Err(ToolError::Blocked {
                        command: format!("{tool_id} denied (trust=quarantined)"),
                    });
                }
            }
            SkillTrustLevel::Trusted | SkillTrustLevel::Verified => {}
        }

        // PermissionPolicy was designed for the bash tool. In Supervised mode, tools
        // without explicit rules default to Ask, which incorrectly blocks MCP/LSP tools.
        // Skip the policy check for such tools — trust-level enforcement above is sufficient.
        // ReadOnly mode is excluded: its allowlist is enforced inside policy.check().
        if self.policy.autonomy_level() == AutonomyLevel::Supervised
            && self.policy.rules().get(tool_id).is_none()
        {
            return Ok(());
        }

        match self.policy.check(tool_id, input) {
            PermissionAction::Allow => Ok(()),
            PermissionAction::Ask => Err(ToolError::ConfirmationRequired {
                command: input.to_owned(),
            }),
            PermissionAction::Deny => Err(ToolError::Blocked {
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
            SkillTrustLevel::Trusted | SkillTrustLevel::Verified => {}
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
            SkillTrustLevel::Trusted | SkillTrustLevel::Verified => {}
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
        self.check_trust(&call.tool_id, input)?;
        self.inner.execute_tool_call(call).await
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        // Bypass check_trust: caller already obtained user approval.
        // Still enforce Blocked/Quarantined trust level constraints.
        match self.effective_trust() {
            SkillTrustLevel::Blocked => {
                return Err(ToolError::Blocked {
                    command: "all tools blocked (trust=blocked)".to_owned(),
                });
            }
            SkillTrustLevel::Quarantined => {
                if is_quarantine_denied(&call.tool_id) || self.is_mcp_tool(&call.tool_id) {
                    return Err(ToolError::Blocked {
                        command: format!("{} denied (trust=quarantined)", call.tool_id),
                    });
                }
            }
            SkillTrustLevel::Trusted | SkillTrustLevel::Verified => {}
        }
        self.inner.execute_tool_call_confirmed(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
        }
    }

    fn make_call_with_cmd(tool_id: &str, cmd: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("command".into(), serde_json::Value::String(cmd.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
            caller_id: None,
        }
    }

    #[tokio::test]
    async fn trusted_allows_all() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        // Default policy has no rules for bash => skip policy check => Ok
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn quarantined_denies_bash() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_write() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("write")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_edit() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("edit")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_delete_path() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("delete_path")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_fetch() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("fetch")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_memory_save() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("memory_save")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
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
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        // file_read is not in quarantine denied list, and policy has no rules for file_read => Ok
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn blocked_denies_everything() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn policy_deny_overrides_trust() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo rm"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute_confirmed() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Blocked);

        let result = gate.execute_confirmed("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
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
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
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
        }
        let gate = TrustGateExecutor::new(RetryableExecutor, PermissionPolicy::default());
        assert!(gate.is_tool_retryable("fetch"));
        assert!(!gate.is_tool_retryable("bash"));
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
        // MCP tool with Supervised mode + from_legacy policy (no rules for MCP tool) => Ok
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(SkillTrustLevel::Trusted);

        let mut params = serde_json::Map::new();
        params.insert(
            "file_path".into(),
            serde_json::Value::String("/tmp/test.txt".into()),
        );
        let call = ToolCall {
            tool_id: "mcp_filesystem__read_file".into(),
            params,
            caller_id: None,
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
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_allows_mcp_read_file() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
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
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_memory_save() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call(&make_call("server_memory_save"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_mcp_confirmed_path() {
        // execute_tool_call_confirmed also enforces quarantine via is_quarantine_denied
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate
            .execute_tool_call_confirmed(&make_call("filesystem_write"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    // mcp_tool_ids registry tests

    fn gate_with_mcp_ids(ids: &[&str]) -> TrustGateExecutor<MockExecutor> {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        let handle = gate.mcp_tool_ids_handle();
        let set: std::collections::HashSet<String> = ids.iter().map(ToString::to_string).collect();
        *handle.write().unwrap() = set;
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
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_registered_mcp_tool_execute() {
        // "shell_execute" — no suffix match on "execute", but registered as MCP.
        let gate = gate_with_mcp_ids(&["shell_execute"]);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("shell_execute")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
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
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[test]
    fn mcp_tool_ids_handle_shared_arc() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        let handle = gate.mcp_tool_ids_handle();
        handle.write().unwrap().insert("test_tool".to_owned());
        assert!(gate.is_mcp_tool("test_tool"));
        assert!(!gate.is_mcp_tool("other_tool"));
    }
}
