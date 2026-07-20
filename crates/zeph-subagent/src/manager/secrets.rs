// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use zeph_common::secret::Secret;
use zeph_sanitizer::secret_mask::SecretCategory;

use super::SubAgentManager;
use crate::error::SubAgentError;
use crate::grants::{GrantKind, GrantedSecret, SecretRequest};

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

        handle.grants_lock().sweep_expired();

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

        handle.grants_lock().grant_secret(secret_key, ttl);
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
    /// The delivered value is stamped with the grant's expiry (see [`GrantedSecret`]) so the
    /// sub-agent loop can keep re-validating the TTL locally on every subsequent tool call,
    /// rather than trusting this one-time gate for the remainder of a long-running turn loop.
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

        let Some(expires_at) = handle
            .grants_lock()
            .expires_at(&GrantKind::Secret(key.to_owned()))
        else {
            tracing::warn!(
                task_id,
                "secret delivery denied: no active grant (missing approval or TTL expired)"
            );
            return Err(SubAgentError::Invalid(
                "no active grant for this secret".to_owned(),
            ));
        };

        // Register the delivered value for masking (#6492): closes the forwarding-path leak
        // (the drain masks against this same registry, which was never populated with the
        // secret it was supposed to catch) and feeds the agent loop's own tool-result masking
        // pass, which consults this registry before content reaches the transcript, LLM
        // context, or debug dump.
        if let Some(registry) = self.secret_registry.as_ref() {
            registry.register(key, value.expose(), SecretCategory::from_key_name(key));
        }

        handle
            .secret_tx
            .try_send(Some(GrantedSecret { value, expires_at }))
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

    /// Try to receive a pending secret request from one specific sub-agent (non-blocking).
    ///
    /// Unlike [`try_recv_secret_request`](Self::try_recv_secret_request), this only polls
    /// `task_id`'s own request channel, so it never pops and discards an unrelated sibling
    /// sub-agent's pending request. Use this when the caller already knows which sub-agent
    /// it wants to act on (e.g. an explicit `/agent approve <id>` command), instead of the
    /// pop-then-filter pattern of polling [`try_recv_secret_request`](Self::try_recv_secret_request)
    /// and discarding non-matching results — a discarded result is popped off the channel
    /// and lost forever, silently starving the sub-agent that actually sent it.
    ///
    /// Returns `None` if `task_id` is unknown or has no pending request.
    pub fn try_recv_secret_request_for(&mut self, task_id: &str) -> Option<SecretRequest> {
        self.agents
            .get_mut(task_id)?
            .pending_secret_rx
            .try_recv()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;
    use zeph_common::secret::Secret;
    use zeph_sanitizer::secret_mask::SecretMaskRegistry;

    use super::make_hook_env;
    use crate::def::SubAgentDef;
    use crate::grants::{GrantedSecret, PermissionGrants};
    use crate::manager::{SubAgentHandle, SubAgentManager, SubAgentStatus};
    use crate::state::SubAgentState;

    /// Builds a [`SubAgentHandle`] with a LIVE `secret_tx`/`secret_rx` pair — unlike
    /// [`SubAgentHandle::for_test`], which immediately drops the receiver (documented as
    /// valid only for metadata inspection, never for a call that actually sends on the
    /// channel). `deliver_secret` calls `secret_tx.try_send(..)`, so a dropped receiver
    /// would make every delivery fail with `Channel("channel closed")` regardless of the
    /// registration logic under test here.
    fn handle_with_live_secret_channel(
        id: &str,
        def: SubAgentDef,
    ) -> (SubAgentHandle, mpsc::Receiver<Option<GrantedSecret>>) {
        let initial_status = SubAgentStatus {
            state: SubAgentState::Working,
            last_message: None,
            turns_used: 0,
            started_at: Instant::now(),
        };
        let (status_tx, status_rx) = watch::channel(initial_status);
        drop(status_tx);
        let (pending_secret_rx_tx, pending_secret_rx) = mpsc::channel(1);
        drop(pending_secret_rx_tx);
        let (secret_tx, secret_rx) = mpsc::channel(1);
        let handle = SubAgentHandle {
            id: id.to_owned(),
            task_id: id.to_owned(),
            def,
            state: SubAgentState::Working,
            join_handle: None,
            cancel: CancellationToken::new(),
            status_rx,
            grants: std::sync::Arc::new(std::sync::Mutex::new(PermissionGrants::default())),
            pending_secret_rx,
            secret_tx,
            started_at_str: String::new(),
            transcript_dir: None,
            mcp_tool_names: Vec::new(),
        };
        (handle, secret_rx)
    }

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

    // --- #6492: deliver_secret registers into the shared secret-mask registry ---

    #[test]
    fn deliver_secret_registers_value_into_secret_mask_registry() {
        let mut mgr = SubAgentManager::new(4);
        let registry = Arc::new(SecretMaskRegistry::new());
        mgr.set_secret_registry(Arc::clone(&registry));

        let (handle, _secret_rx) =
            handle_with_live_secret_channel("task-1", SubAgentDef::for_test("helper"));
        handle
            .grants_lock()
            .grant_secret("SOME_VAULT_KEY", Duration::from_mins(5));
        mgr.insert_handle_for_test("task-1".to_owned(), handle);

        mgr.deliver_secret(
            "task-1",
            "SOME_VAULT_KEY",
            Secret::new("the-secret-value-123"),
        )
        .expect("delivery must succeed: an active grant exists");

        assert!(
            registry.would_mask("value is the-secret-value-123"),
            "a delivered secret must be registered into the shared mask registry — closes \
             the gap where the forwarding drain masked against a registry that was never \
             populated with the secret it was supposed to catch"
        );
    }

    #[test]
    fn deliver_secret_without_registry_still_succeeds() {
        // Regression guard: registering into the mask registry must stay best-effort — a
        // session with no registry wired (the `None` default) must not lose secret delivery.
        let mut mgr = SubAgentManager::new(4);

        let (handle, _secret_rx) =
            handle_with_live_secret_channel("task-1", SubAgentDef::for_test("helper"));
        handle
            .grants_lock()
            .grant_secret("SOME_VAULT_KEY", Duration::from_mins(5));
        mgr.insert_handle_for_test("task-1".to_owned(), handle);

        let result = mgr.deliver_secret(
            "task-1",
            "SOME_VAULT_KEY",
            Secret::new("the-secret-value-123"),
        );
        assert!(
            result.is_ok(),
            "delivery must succeed even with no registry wired"
        );
    }

    #[test]
    fn deliver_secret_without_active_grant_is_denied() {
        // Baseline: delivery must still be refused when there is no active grant, unaffected
        // by the new registration step (which only runs after the grant check succeeds).
        let mut mgr = SubAgentManager::new(4);
        let registry = Arc::new(SecretMaskRegistry::new());
        mgr.set_secret_registry(Arc::clone(&registry));

        let handle = SubAgentHandle::for_test("task-1", SubAgentDef::for_test("helper"));
        mgr.insert_handle_for_test("task-1".to_owned(), handle);

        let result = mgr.deliver_secret(
            "task-1",
            "SOME_VAULT_KEY",
            Secret::new("the-secret-value-123"),
        );
        assert!(
            result.is_err(),
            "delivery without an active grant must be denied"
        );
        assert!(
            !registry.would_mask("value is the-secret-value-123"),
            "a denied delivery must never register the secret"
        );
    }
}
