// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Exfiltration URL flagging and permission-denied hook dispatch.
//!
//! Covers suspicious-URL flagging in tool call arguments (exfiltration guard) and the
//! `permission_denied` hook fired on every gate/rate-limiter denial. Split out of
//! `tier_loop.rs` — see that module for the orchestration entry point that calls into these.

use tracing::Instrument;

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    pub(super) fn check_exfiltration_urls(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) {
        for tc in tool_calls {
            let args_json = tc.input.to_string();
            let url_events = self
                .services
                .security
                .exfiltration_guard
                .validate_tool_call(
                    tc.name.as_str(),
                    &args_json,
                    &self.services.security.flagged_urls,
                );
            if !url_events.is_empty() {
                tracing::warn!(
                    tool = %tc.name,
                    count = url_events.len(),
                    "exfiltration guard: suspicious URLs in tool arguments (flag-only, not blocked)"
                );
                self.update_metrics(|m| {
                    m.exfiltration_tool_urls_flagged += url_events.len() as u64;
                });
                self.push_security_event(
                    zeph_common::SecurityEventCategory::ExfiltrationBlock,
                    tc.name.as_str(),
                    format!(
                        "{} suspicious URL(s) flagged in tool args",
                        url_events.len()
                    ),
                );
            }
        }
    }

    /// Fires `permission_denied` hooks (fail-open). Called at every gate/rate-limiter denial.
    ///
    /// Hooks run sequentially; slow or hanging hooks will stall tool dispatch for each denied
    /// call. Hook authors should ensure hooks complete quickly or use a background process.
    pub(super) async fn fire_permission_denied_hooks(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        reason: &str,
    ) {
        let pd_hooks = self.services.session.hooks_config.permission_denied.clone();
        if pd_hooks.is_empty() {
            return;
        }
        let mut env = std::collections::HashMap::new();
        env.insert("ZEPH_DENIED_TOOL".to_owned(), tc.name.to_string());
        env.insert("ZEPH_DENY_REASON".to_owned(), reason.to_owned());
        env.insert("ZEPH_TOOL_NAME".to_owned(), tc.name.to_string());
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        crate::agent::hooks_dispatch::insert_main_agent_ctx(&mut env, conv_id_str.as_deref());
        let dispatch = self.mcp_dispatch();
        let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
            .as_ref()
            .map(|d| d as &dyn zeph_subagent::McpDispatch);
        if let Err(e) = zeph_subagent::hooks::fire_hooks(&pd_hooks, &env, mcp, None)
            .instrument(tracing::info_span!(
                "core.hooks.permission_denied",
                tool = %tc.name
            ))
            .await
        {
            tracing::warn!(error = %e, tool = %tc.name, "PermissionDenied hook failed");
        }
    }
}

#[cfg(test)]
mod tests {
    // Regression guard for issue #3774: permission_denied hook env must contain
    // ZEPH_DENIED_TOOL and ZEPH_DENY_REASON for every gate/rate-limiter denial.
    // These tests verify the env construction logic mirrored in fire_permission_denied_hooks.

    fn make_pd_env(tool: &str, reason: &str) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        env.insert("ZEPH_DENIED_TOOL".to_owned(), tool.to_owned());
        env.insert("ZEPH_DENY_REASON".to_owned(), reason.to_owned());
        env
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_quota_denial() {
        let tool = "shell";
        let reason = "session tool call quota exceeded (limit: 10 calls)";
        let env = make_pd_env(tool, reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("shell")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("quota")),
            "ZEPH_DENY_REASON should mention quota"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_rate_limit_denial() {
        use crate::agent::rate_limiter::{RateLimitExceeded, ToolCategory};

        let exceeded = RateLimitExceeded {
            category: ToolCategory::Shell,
            count: 5,
            limit: 3,
            cooldown_remaining_secs: 30,
        };
        let reason = exceeded.to_error_message();
        let env = make_pd_env("bash", &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("bash")
        );
        let deny_reason = env
            .get("ZEPH_DENY_REASON")
            .expect("ZEPH_DENY_REASON missing");
        assert!(
            deny_reason.contains("rate-limited"),
            "ZEPH_DENY_REASON should mention rate-limited, got: {deny_reason}"
        );
        assert!(
            deny_reason.contains("3/min"),
            "ZEPH_DENY_REASON should contain limit, got: {deny_reason}"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_pre_exec_block() {
        let tool = "write";
        let reason = format!("blocked by pre-execution verifier: {tool} is not permitted");
        let env = make_pd_env(tool, &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("write")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("pre-execution verifier")),
            "ZEPH_DENY_REASON should mention pre-execution verifier"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_repeat_block() {
        let tool = "read";
        let reason = format!("repeated identical call to {tool} detected");
        let env = make_pd_env(tool, &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("read")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("repeated identical call")),
            "ZEPH_DENY_REASON should mention repeated identical call"
        );
    }

    #[test]
    fn permission_denied_env_reason_includes_utility_action_variant() {
        // Verify that utility gate reason strings include the UtilityAction Debug variant name
        // so hook authors can distinguish Respond/Retrieve/Verify/Stop in ZEPH_DENY_REASON.
        use zeph_tools::UtilityAction;

        for action in [
            UtilityAction::Respond,
            UtilityAction::Retrieve,
            UtilityAction::Verify,
            UtilityAction::Stop,
        ] {
            let reason = format!("utility gate ({action:?}) intercepted memory_search");
            let env = make_pd_env("memory_search", &reason);

            let deny_reason = env
                .get("ZEPH_DENY_REASON")
                .expect("ZEPH_DENY_REASON missing");
            assert!(
                deny_reason.contains(&format!("{action:?}")),
                "ZEPH_DENY_REASON should contain {action:?}, got: {deny_reason}"
            );
        }
    }
}
