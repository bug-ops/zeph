// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use zeph_common::secret::Secret;

use super::SubAgentManager;
use crate::error::SubAgentError;
use crate::grants::{GrantKind, SecretRequest};

/// Build the standard hook environment for a sub-agent lifecycle event.
pub(crate) fn make_hook_env(
    task_id: &str,
    agent_name: &str,
    tool_name: &str,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("ZEPH_AGENT_ID".to_owned(), task_id.to_owned());
    env.insert("ZEPH_AGENT_NAME".to_owned(), agent_name.to_owned());
    env.insert("ZEPH_AGENT_TYPE".to_owned(), "subagent".to_owned());
    env.insert("ZEPH_TOOL_NAME".to_owned(), tool_name.to_owned());
    env
}

impl SubAgentManager {
    /// Approve a secret request for a running sub-agent.
    ///
    /// Called after the user approves a vault secret access prompt. The secret
    /// key must appear in the sub-agent definition's allowed `secrets` list;
    /// otherwise the request is auto-denied.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Invalid`] if the key is not in the definition's allowed list.
    pub fn approve_secret(
        &mut self,
        task_id: &str,
        secret_key: &str,
        ttl: Duration,
    ) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        handle.grants.sweep_expired();

        if !handle
            .def
            .permissions
            .secrets
            .iter()
            .any(|k| k == secret_key)
        {
            tracing::warn!(task_id, "secret request denied: key not in allowed list");
            return Err(SubAgentError::Invalid(format!(
                "secret is not in the allowed secrets list for '{}'",
                handle.def.name
            )));
        }

        handle.grants.grant_secret(secret_key, ttl);
        Ok(())
    }

    /// Deliver a resolved secret value to a waiting sub-agent loop.
    ///
    /// Should be called after the user approves the request and the caller has resolved
    /// `key` to its actual vault value (see [`approve_secret`](Self::approve_secret)).
    /// Requires an active grant for `key` — delivery is refused if
    /// [`approve_secret`](Self::approve_secret) was never called or the grant's TTL has
    /// already elapsed, making
    /// [`PermissionGrants::is_active`][crate::grants::PermissionGrants::is_active]
    /// load-bearing rather than unused bookkeeping.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown, or
    /// [`SubAgentError::Invalid`] if there is no active grant for `key`.
    pub fn deliver_secret(
        &mut self,
        task_id: &str,
        key: &str,
        value: Secret,
    ) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        if !handle.grants.is_active(&GrantKind::Secret(key.to_owned())) {
            tracing::warn!(
                task_id,
                "secret delivery denied: no active grant (missing approval or TTL expired)"
            );
            return Err(SubAgentError::Invalid(
                "no active grant for this secret".to_owned(),
            ));
        }

        handle
            .secret_tx
            .try_send(Some(value))
            .map_err(|e| SubAgentError::Channel(e.to_string()))
    }

    /// Deny a pending secret request — sends `None` to unblock the waiting sub-agent loop.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Channel`] if the channel is full or closed.
    pub fn deny_secret(&mut self, task_id: &str) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;
        handle
            .secret_tx
            .try_send(None)
            .map_err(|e| SubAgentError::Channel(e.to_string()))
    }

    /// Try to receive a pending secret request from any sub-agent (non-blocking).
    ///
    /// Polls each active agent's request channel once. Returns `Some((task_id, request))`
    /// if any agent has a pending request, or `None` if all channels are empty.
    /// Call this from the main agent loop to surface approval prompts to the user.
    pub fn try_recv_secret_request(&mut self) -> Option<(String, SecretRequest)> {
        for handle in self.agents.values_mut() {
            if let Ok(req) = handle.pending_secret_rx.try_recv() {
                return Some((handle.task_id.clone(), req));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::make_hook_env;

    #[test]
    fn make_hook_env_sets_agent_type_subagent() {
        let env = make_hook_env("task-42", "my-agent", "Shell");
        assert_eq!(
            env.get("ZEPH_AGENT_TYPE").map(String::as_str),
            Some("subagent")
        );
        assert_eq!(
            env.get("ZEPH_AGENT_ID").map(String::as_str),
            Some("task-42")
        );
        assert_eq!(
            env.get("ZEPH_AGENT_NAME").map(String::as_str),
            Some("my-agent")
        );
        assert_eq!(env.get("ZEPH_TOOL_NAME").map(String::as_str), Some("Shell"));
    }
}
