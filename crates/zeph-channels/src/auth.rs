// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared allowlist authorization primitives used by every channel adapter
//! that gates access by sender identity (Telegram, Discord, Slack).
//!
//! Two responsibilities are centralized here so a future channel adapter
//! cannot silently diverge from the fail-closed policy:
//!
//! * [`require_configured_allowlist`] is the **startup gate**: call it once
//!   when the adapter is constructed, before spawning any listener or making
//!   any network call. It refuses to start (returns `Err`) when every
//!   allowlist relevant to the adapter is empty, rather than silently
//!   running open to any sender.
//! * [`is_identity_allowed`] and [`all_lists_empty`] back the **per-message
//!   check** reused both by `Channel::recv`/`try_recv`-adjacent
//!   `is_authorized` helpers and by
//!   [`ConfirmLoop::confirm_accepts`](crate::confirm::ConfirmLoop::confirm_accepts).
//!   An empty list is treated as "unrestricted" at this layer only because
//!   the startup gate above guarantees the list is never actually empty at
//!   call time in a correctly constructed adapter.

use zeph_core::channel::ChannelError;

/// Returns `true` when every list in `lists` is empty.
///
/// Shared by [`require_configured_allowlist`] (to refuse startup) and by
/// adapters with more than one identity list — e.g. Discord's users + roles
/// — that need the same "no restriction configured" short-circuit in their
/// per-message check, so the two empty-checks cannot drift apart.
#[must_use]
pub fn all_lists_empty(lists: &[&[String]]) -> bool {
    lists.iter().all(|list| list.is_empty())
}

/// Refuses to start a channel adapter whose allowlists are all empty.
///
/// Mirrors Telegram's original fail-closed startup check (`allowed_users`
/// must not be empty): an unconfigured allowlist is treated as a
/// misconfiguration to reject, never as "allow everyone". Call this before
/// any listener is spawned or network call is made, so a misconfigured
/// adapter has no observable side effect.
///
/// # Errors
///
/// Returns [`ChannelError::Other`] when every list in `lists` is empty.
pub fn require_configured_allowlist(
    channel: &str,
    lists: &[&[String]],
) -> Result<(), ChannelError> {
    if all_lists_empty(lists) {
        tracing::error!("{channel}: allowlist is empty; refusing to start an open channel");
        return Err(ChannelError::Other(format!(
            "{channel} allowlist must not be empty"
        )));
    }
    Ok(())
}

/// Returns `true` when `identity` appears in `allowed`.
///
/// An empty `allowed` list is treated as "no restriction" and always returns
/// `true` — callers MUST pair this with [`require_configured_allowlist`] at
/// startup so that fallback never actually triggers at call time.
#[must_use]
pub fn is_identity_allowed(identity: Option<&str>, allowed: &[String]) -> bool {
    allowed.is_empty() || identity.is_some_and(|id| allowed.iter().any(|a| a == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_empty_true_when_all_empty() {
        let a: Vec<String> = vec![];
        let b: Vec<String> = vec![];
        assert!(all_lists_empty(&[&a, &b]));
    }

    #[test]
    fn all_lists_empty_false_when_any_non_empty() {
        let a: Vec<String> = vec![];
        let b = vec!["x".to_string()];
        assert!(!all_lists_empty(&[&a, &b]));
    }

    #[test]
    fn require_configured_allowlist_errs_when_all_empty() {
        let a: Vec<String> = vec![];
        let b: Vec<String> = vec![];
        let result = require_configured_allowlist("test", &[&a, &b]);
        assert!(matches!(result, Err(ChannelError::Other(_))));
    }

    #[test]
    fn require_configured_allowlist_ok_when_one_list_configured() {
        let a: Vec<String> = vec![];
        let b = vec!["user1".to_string()];
        let result = require_configured_allowlist("test", &[&a, &b]);
        assert!(result.is_ok());
    }

    #[test]
    fn is_identity_allowed_permits_all_when_empty() {
        assert!(is_identity_allowed(None, &[]));
        assert!(is_identity_allowed(Some("anyone"), &[]));
    }

    #[test]
    fn is_identity_allowed_known_identity_is_permitted() {
        let allowed = vec!["alice".to_string(), "bob".to_string()];
        assert!(is_identity_allowed(Some("alice"), &allowed));
        assert!(is_identity_allowed(Some("bob"), &allowed));
    }

    #[test]
    fn is_identity_allowed_unknown_identity_is_rejected() {
        let allowed = vec!["alice".to_string()];
        assert!(!is_identity_allowed(Some("eve"), &allowed));
        assert!(!is_identity_allowed(None, &allowed));
    }
}
