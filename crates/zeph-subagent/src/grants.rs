// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Zero-trust TTL-bounded permission grants for sub-agents.
//!
//! [`PermissionGrants`] tracks active grants (vault secrets or runtime tool access)
//! for a running sub-agent. All grants are time-limited; expired grants are swept
//! lazily by [`PermissionGrants::is_active`] and eagerly by
//! [`PermissionGrants::sweep_expired`].
//!
//! Grants are revoked on drop and on agent completion/cancellation. Secret key names
//! are never logged above DEBUG level; the `Display` impl for [`GrantKind::Secret`]
//! always prints `"Secret(<redacted>)"`.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeph_common::secret::Secret;

/// Metadata sent by a sub-agent when it needs a secret from the vault.
///
/// Carried in an `InputRequired` A2A status update as structured metadata.
/// The parent agent surfaces this to the user as an approval prompt; the user can
/// then call [`SubAgentManager::approve_secret`][crate::SubAgentManager] or
/// [`SubAgentManager::deny_secret`][crate::SubAgentManager].
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::grants::SecretRequest;
///
/// let req = SecretRequest {
///     secret_key: "OPENAI_API_KEY".to_owned(),
///     reason: Some("needed for embeddings".to_owned()),
/// };
/// assert_eq!(req.secret_key, "OPENAI_API_KEY");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRequest {
    /// The vault key name the sub-agent is requesting.
    pub secret_key: String,
    /// Human-readable reason (shown to the user in the approval prompt).
    pub reason: Option<String>,
}

/// Identifies the kind of permission that was granted to a sub-agent.
///
/// `GrantKind` is intentionally NOT serializable — grant metadata should never
/// leave the in-memory security boundary. Key names are logged only at DEBUG
/// level to avoid leaking grant enumeration to centralized log systems.
///
/// The [`Display`][std::fmt::Display] implementation always redacts `Secret` payloads,
/// printing `Secret(<redacted>)` instead of the actual key name.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::grants::GrantKind;
///
/// let secret = GrantKind::Secret("my-key".to_owned());
/// assert!(!secret.to_string().contains("my-key"), "key must be redacted");
///
/// let tool = GrantKind::Tool("shell".to_owned());
/// assert_eq!(tool.to_string(), "Tool(shell)");
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantKind {
    /// A vault secret key granted for in-memory access.
    Secret(String),
    /// A tool name granted at runtime beyond the definition's static policy.
    Tool(String),
}

impl std::fmt::Display for GrantKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Secret(_) => write!(f, "Secret(<redacted>)"),
            Self::Tool(name) => write!(f, "Tool({name})"),
        }
    }
}

/// A single permission grant with a TTL.
///
/// Created via [`PermissionGrants::add`] and swept automatically by
/// [`PermissionGrants::sweep_expired`].
#[derive(Debug)]
pub struct Grant {
    pub(crate) kind: GrantKind,
    pub(crate) granted_at: Instant,
    pub(crate) ttl: Duration,
}

impl Grant {
    /// Create a new grant for `kind` that expires after `ttl`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_subagent::grants::{Grant, GrantKind};
    ///
    /// let grant = Grant::new(GrantKind::Tool("shell".to_owned()), Duration::from_mins(1));
    /// assert!(!grant.is_expired());
    /// ```
    #[must_use]
    pub fn new(kind: GrantKind, ttl: Duration) -> Self {
        Self {
            kind,
            granted_at: Instant::now(),
            ttl,
        }
    }

    /// Returns `true` if the grant's TTL has elapsed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_subagent::grants::{Grant, GrantKind};
    ///
    /// let grant = Grant::new(GrantKind::Tool("web".to_owned()), Duration::from_mins(5));
    /// // A brand-new grant is not yet expired.
    /// assert!(!grant.is_expired());
    /// ```
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.granted_at.elapsed() >= self.ttl
    }
}

/// Tracks active zero-trust permission grants for a sub-agent.
///
/// All grants are TTL-bounded. [`is_active`](Self::is_active) automatically
/// sweeps expired grants before checking, so callers do not need to call
/// [`sweep_expired`](Self::sweep_expired) manually.
#[derive(Debug, Default)]
pub struct PermissionGrants {
    grants: Vec<Grant>,
}

impl Drop for PermissionGrants {
    fn drop(&mut self) {
        // Defense-in-depth: revoke all grants on drop even if revoke_all()
        // was not explicitly called (e.g., on panic or early return).
        if !self.grants.is_empty() {
            tracing::warn!(
                count = self.grants.len(),
                "PermissionGrants dropped with active grants — revoking"
            );
            self.grants.clear();
        }
    }
}

impl PermissionGrants {
    /// Add a new grant with the given `kind` and `ttl`.
    ///
    /// The grant is immediately tracked. Expired grants are not swept here;
    /// call [`sweep_expired`][Self::sweep_expired] or [`is_active`][Self::is_active]
    /// to remove stale entries.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_subagent::grants::{GrantKind, PermissionGrants};
    ///
    /// let mut grants = PermissionGrants::default();
    /// grants.add(GrantKind::Tool("shell".to_owned()), Duration::from_mins(1));
    /// assert!(grants.is_active(&GrantKind::Tool("shell".to_owned())));
    /// ```
    pub fn add(&mut self, kind: GrantKind, ttl: Duration) {
        // Log tool grants at DEBUG; for secrets log only the redacted display form.
        tracing::debug!(kind = %kind, ?ttl, "permission grant added");
        self.grants.push(Grant::new(kind, ttl));
    }

    /// Remove all expired grants.
    pub fn sweep_expired(&mut self) {
        let expired: Vec<_> = self.grants.extract_if(.., |g| g.is_expired()).collect();
        for g in &expired {
            tracing::debug!(kind = %g.kind, "permission grant expired and revoked");
        }
        if !expired.is_empty() {
            tracing::debug!(removed = expired.len(), "swept expired grants");
        }
    }

    /// Check if a specific grant is still active (not expired).
    ///
    /// Automatically sweeps expired grants before checking.
    #[must_use]
    pub fn is_active(&mut self, kind: &GrantKind) -> bool {
        self.sweep_expired();
        self.grants.iter().any(|g| &g.kind == kind)
    }

    /// Returns the absolute instant at which the active grant for `kind` expires.
    ///
    /// Automatically sweeps expired grants before checking, so a `None` result means
    /// there is no active grant for `kind` (never granted, already expired, or revoked).
    /// Used by [`SubAgentManager::deliver_secret`][crate::manager::SubAgentManager::deliver_secret]
    /// to stamp the delivered value with its expiry so the sub-agent loop can re-validate the
    /// TTL locally on every subsequent tool call, without needing further access to this
    /// `PermissionGrants` instance (which stays on the manager side, not the spawned loop task).
    ///
    /// If duplicate grants exist for the same `kind`, this returns the *first* match's
    /// expiry rather than the latest (max) one. This is intentionally fail-safe: it can
    /// only cause an earlier-than-necessary secret eviction in the sub-agent loop, never
    /// a later one, so it is not a security concern — just a minor inefficiency in the
    /// rare duplicate-grant case.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_subagent::grants::{GrantKind, PermissionGrants};
    ///
    /// let mut grants = PermissionGrants::default();
    /// let kind = GrantKind::Secret("api-key".to_owned());
    /// assert!(grants.expires_at(&kind).is_none());
    ///
    /// grants.add(kind.clone(), Duration::from_mins(5));
    /// assert!(grants.expires_at(&kind).is_some());
    /// ```
    #[must_use]
    pub fn expires_at(&mut self, kind: &GrantKind) -> Option<Instant> {
        self.sweep_expired();
        self.grants
            .iter()
            .find(|g| &g.kind == kind)
            .map(|g| g.granted_at + g.ttl)
    }

    /// Grant access to a vault secret with the given TTL.
    ///
    /// Sweeps expired grants first. Logs an audit event at DEBUG (key is redacted
    /// in the log output to avoid leaking grant enumeration to log aggregators).
    pub fn grant_secret(&mut self, key: impl Into<String>, ttl: Duration) {
        self.sweep_expired();
        let key = key.into();
        tracing::debug!("vault secret granted to sub-agent (key redacted), ttl={ttl:?}");
        self.add(GrantKind::Secret(key), ttl);
    }

    /// Returns `true` if there are any grants currently tracked (expired or not).
    ///
    /// Used by [`Drop`] to emit a warning when handles are dropped without cleanup.
    #[must_use]
    pub fn is_empty_grants(&self) -> bool {
        self.grants.is_empty()
    }

    /// Revoke all grants immediately (called on sub-agent completion or cancellation).
    pub fn revoke_all(&mut self) {
        let count = self.grants.len();
        self.grants.clear();
        if count > 0 {
            tracing::debug!(count, "all permission grants revoked");
        }
    }

    /// Check whether a `GrantKind::Tool` grant permits dispatching `tool_name`.
    ///
    /// This is the enforcement entry point mirrored after the already-shipped
    /// `GrantKind::Secret` TTL re-check in the sub-agent loop's `handle_tool_step`
    /// (`granted_secrets.retain(|_, granted| !granted.is_expired())`): the check is
    /// evaluated fresh against
    /// [`Grant::is_expired`] rather than relying on a prior [`sweep_expired`](Self::sweep_expired)
    /// call having already run, so a grant that lapsed since the last sweep is still caught
    /// here (no time-of-check-to-time-of-use window).
    ///
    /// Distinguishing [`ToolGrantCheck::NoGrant`] from [`ToolGrantCheck::Expired`] lets the
    /// caller apply fail-closed rejection only where a grant was actually issued and has
    /// since lapsed, while leaving the — currently universal, since no production caller
    /// creates `GrantKind::Tool` grants yet — no-grant case unrestricted.
    ///
    /// # Semantics: default-permit, time-box only — NOT an allow-list (confirmed, issue #6567)
    ///
    /// A `GrantKind::Tool` grant only *time-boxes* a tool the sub-agent is already permitted
    /// to call under its static `ToolPolicy`/`AutonomyLevel` — it is an additional, narrower
    /// restriction layered on top of an existing permission, never an independent grant of
    /// access. Concretely:
    /// - Absence of any grant for `tool_name` always permits the call (matches the current,
    ///   universal zero-grant production state — no observable behavior change).
    /// - A grant existing for *one* tool name never restricts any *other* tool name, even for
    ///   the same sub-agent. There is no implicit "some grants exist, so everything else is
    ///   denied" allow-list mode, and this method must never be extended to add one without a
    ///   deliberate, separately-specified design change.
    /// - Matching is exact tool-name string equality only — no prefix/glob support for tool
    ///   families (e.g. MCP server-scoped names). A grant for `"mcp:server-a"` does not cover
    ///   `"mcp:server-a:tool-x"` or any other name.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use zeph_subagent::grants::{GrantKind, PermissionGrants, ToolGrantCheck};
    ///
    /// let mut grants = PermissionGrants::default();
    /// assert_eq!(grants.check_tool_grant("shell"), ToolGrantCheck::NoGrant);
    ///
    /// grants.add(GrantKind::Tool("shell".to_owned()), Duration::from_mins(1));
    /// assert_eq!(grants.check_tool_grant("shell"), ToolGrantCheck::Active);
    /// ```
    #[must_use]
    pub fn check_tool_grant(&mut self, tool_name: &str) -> ToolGrantCheck {
        let kind = GrantKind::Tool(tool_name.to_owned());
        let had_record = self.grants.iter().any(|g| g.kind == kind);
        if !had_record {
            return ToolGrantCheck::NoGrant;
        }
        if self.is_active(&kind) {
            ToolGrantCheck::Active
        } else {
            ToolGrantCheck::Expired
        }
    }
}

/// Result of checking whether a `GrantKind::Tool` grant permits a tool dispatch.
///
/// Returned by [`PermissionGrants::check_tool_grant`]. See that method's doc for the full
/// default-permit / time-box-only semantics (confirmed, issue #6567) — in short: this is a
/// narrowing restriction on top of an already-permitted tool, never an allow-list, and
/// [`NoGrant`](Self::NoGrant) always means "unrestricted by this mechanism," not "denied."
///
/// # Examples
///
/// ```rust
/// use std::time::Duration;
/// use zeph_subagent::grants::{GrantKind, PermissionGrants, ToolGrantCheck};
///
/// let mut grants = PermissionGrants::default();
/// grants.add(GrantKind::Tool("web".to_owned()), Duration::from_mins(5));
/// assert_eq!(grants.check_tool_grant("web"), ToolGrantCheck::Active);
/// assert_eq!(grants.check_tool_grant("shell"), ToolGrantCheck::NoGrant);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGrantCheck {
    /// No `GrantKind::Tool` grant record exists for this tool name — dispatch is
    /// unrestricted by this mechanism (today's universal production state, since no
    /// caller creates `GrantKind::Tool` grants yet).
    NoGrant,
    /// A `GrantKind::Tool` grant exists for this tool name and has not expired.
    Active,
    /// A `GrantKind::Tool` grant existed for this tool name but its TTL has elapsed; the
    /// stale entry is evicted as a side effect of this check.
    Expired,
}

#[cfg(test)]
impl PermissionGrants {
    /// Insert a grant with an explicit `granted_at`, bypassing `Instant::now()`, so
    /// crate-internal tests outside this module (e.g. `agent_loop`'s enforcement tests) can
    /// deterministically construct an already-expired grant without a real sleep — the
    /// `grants` field itself is module-private, so this is the supported way in from outside
    /// `grants.rs`.
    pub(crate) fn add_test_grant(&mut self, kind: GrantKind, granted_at: Instant, ttl: Duration) {
        self.grants.push(Grant {
            kind,
            granted_at,
            ttl,
        });
    }
}

/// A resolved secret value delivered to a sub-agent loop, paired with the absolute
/// instant its originating grant expires.
///
/// Sent over the `secret_tx`/`secret_rx` channel
/// (see [`SubAgentHandle::secret_tx`][crate::manager::SubAgentHandle::secret_tx]) instead of a
/// bare [`Secret`] so the spawned agent loop task — which has no further access to the
/// manager-side [`PermissionGrants`] once the value is delivered — can still re-validate the
/// TTL locally before every tool call and evict the value once it expires.
///
/// # Examples
///
/// ```rust
/// use std::time::{Duration, Instant};
/// use zeph_common::secret::Secret;
/// use zeph_subagent::grants::GrantedSecret;
///
/// let granted = GrantedSecret {
///     value: Secret::new("sekrit"),
///     expires_at: Instant::now() + Duration::from_mins(5),
/// };
/// assert!(!granted.is_expired());
/// ```
#[derive(Debug)]
pub struct GrantedSecret {
    /// The resolved vault secret value.
    pub value: Secret,
    /// The absolute instant after which this value must no longer be used.
    pub expires_at: Instant,
}

impl GrantedSecret {
    /// Returns `true` if `expires_at` has already passed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::time::{Duration, Instant};
    /// use zeph_common::secret::Secret;
    /// use zeph_subagent::grants::GrantedSecret;
    ///
    /// let expired = GrantedSecret {
    ///     value: Secret::new("sekrit"),
    ///     expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
    /// };
    /// assert!(expired.is_expired());
    /// ```
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_is_active_before_expiry() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Secret("api-key".into()), Duration::from_mins(5));
        assert!(pg.is_active(&GrantKind::Secret("api-key".into())));
    }

    #[test]
    fn sweep_expired_removes_instant_ttl() {
        let mut pg = PermissionGrants::default();
        pg.grants.push(Grant {
            kind: GrantKind::Tool("shell".into()),
            granted_at: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
            ttl: Duration::from_secs(1), // already expired
        });
        // is_active internally sweeps
        assert!(!pg.is_active(&GrantKind::Tool("shell".into())));
        assert!(pg.grants.is_empty());
    }

    #[test]
    fn revoke_all_clears_all_grants() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Secret("token".into()), Duration::from_mins(1));
        pg.add(GrantKind::Tool("web".into()), Duration::from_mins(1));
        pg.revoke_all();
        assert!(pg.grants.is_empty());
    }

    #[test]
    fn grant_secret_is_active() {
        let mut pg = PermissionGrants::default();
        pg.grant_secret("db-password", Duration::from_mins(2));
        assert!(pg.is_active(&GrantKind::Secret("db-password".into())));
    }

    #[test]
    fn whitespace_description_invalid() {
        // Verify grant kind display redacts secrets
        let k = GrantKind::Secret("my-secret-key".into());
        let display = k.to_string();
        assert!(
            !display.contains("my-secret-key"),
            "secret key must be redacted in Display"
        );
        assert!(display.contains("redacted"));
    }

    #[test]
    fn tool_grant_display_shows_name() {
        let k = GrantKind::Tool("shell".into());
        assert_eq!(k.to_string(), "Tool(shell)");
    }

    #[test]
    fn partial_sweep_keeps_non_expired_grants() {
        let mut pg = PermissionGrants::default();

        // Add one already-expired grant.
        pg.grants.push(Grant {
            kind: GrantKind::Tool("expired-tool".into()),
            granted_at: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
            ttl: Duration::from_secs(1),
        });

        // Add one live grant with long TTL.
        pg.add(GrantKind::Secret("live-key".into()), Duration::from_mins(5));

        pg.sweep_expired();

        assert_eq!(pg.grants.len(), 1, "only live grant should remain");
        assert_eq!(pg.grants[0].kind, GrantKind::Secret("live-key".into()));
    }

    #[test]
    fn check_tool_grant_no_record_returns_no_grant() {
        let mut pg = PermissionGrants::default();
        assert_eq!(pg.check_tool_grant("shell"), ToolGrantCheck::NoGrant);
    }

    #[test]
    fn check_tool_grant_active_returns_active() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Tool("shell".into()), Duration::from_mins(5));
        assert_eq!(pg.check_tool_grant("shell"), ToolGrantCheck::Active);
    }

    #[test]
    fn check_tool_grant_unrelated_name_returns_no_grant() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Tool("shell".into()), Duration::from_mins(5));
        assert_eq!(pg.check_tool_grant("web"), ToolGrantCheck::NoGrant);
    }

    #[test]
    fn check_tool_grant_expired_returns_expired_and_evicts() {
        let mut pg = PermissionGrants::default();
        pg.grants.push(Grant {
            kind: GrantKind::Tool("shell".into()),
            granted_at: Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
            ttl: Duration::from_secs(1),
        });
        assert_eq!(pg.check_tool_grant("shell"), ToolGrantCheck::Expired);
        assert!(pg.grants.is_empty(), "expired grant must be evicted");
    }

    #[test]
    fn duplicate_grant_for_same_key_both_tracked() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Secret("my-key".into()), Duration::from_mins(1));
        pg.add(GrantKind::Secret("my-key".into()), Duration::from_mins(1));

        // Both grants are stored; is_active just checks any match.
        assert_eq!(pg.grants.len(), 2);
        assert!(pg.is_active(&GrantKind::Secret("my-key".into())));

        // After revoking all, none remain.
        pg.revoke_all();
        assert!(pg.grants.is_empty());
    }
}
