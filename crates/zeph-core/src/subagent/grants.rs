// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Metadata sent by a sub-agent when it needs a secret from the vault.
///
/// Carried in an `InputRequired` A2A status update as structured metadata.
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

#[derive(Debug)]
pub struct Grant {
    pub(crate) kind: GrantKind,
    pub(crate) granted_at: Instant,
    pub(crate) ttl: Duration,
}

impl Grant {
    #[must_use]
    pub fn new(kind: GrantKind, ttl: Duration) -> Self {
        Self {
            kind,
            granted_at: Instant::now(),
            ttl,
        }
    }

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
    /// Add a new grant.
    pub fn add(&mut self, kind: GrantKind, ttl: Duration) {
        // Log tool grants at DEBUG; for secrets log only the redacted display form.
        tracing::debug!(kind = %kind, ?ttl, "permission grant added");
        self.grants.push(Grant::new(kind, ttl));
    }

    /// Remove all expired grants.
    pub fn sweep_expired(&mut self) {
        let before = self.grants.len();
        self.grants.retain(|g| {
            let expired = g.is_expired();
            if expired {
                tracing::debug!(kind = %g.kind, "permission grant expired and revoked");
            }
            !expired
        });
        let removed = before - self.grants.len();
        if removed > 0 {
            tracing::debug!(removed, "swept expired grants");
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_is_active_before_expiry() {
        let mut pg = PermissionGrants::default();
        pg.add(
            GrantKind::Secret("api-key".into()),
            Duration::from_secs(300),
        );
        assert!(pg.is_active(&GrantKind::Secret("api-key".into())));
    }

    #[test]
    fn sweep_expired_removes_instant_ttl() {
        let mut pg = PermissionGrants::default();
        pg.grants.push(Grant {
            kind: GrantKind::Tool("shell".into()),
            granted_at: Instant::now() - Duration::from_secs(10),
            ttl: Duration::from_secs(1), // already expired
        });
        // is_active internally sweeps
        assert!(!pg.is_active(&GrantKind::Tool("shell".into())));
        assert!(pg.grants.is_empty());
    }

    #[test]
    fn revoke_all_clears_all_grants() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Secret("token".into()), Duration::from_secs(60));
        pg.add(GrantKind::Tool("web".into()), Duration::from_secs(60));
        pg.revoke_all();
        assert!(pg.grants.is_empty());
    }

    #[test]
    fn grant_secret_is_active() {
        let mut pg = PermissionGrants::default();
        pg.grant_secret("db-password", Duration::from_secs(120));
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
            granted_at: Instant::now() - Duration::from_secs(10),
            ttl: Duration::from_secs(1),
        });

        // Add one live grant with long TTL.
        pg.add(
            GrantKind::Secret("live-key".into()),
            Duration::from_secs(300),
        );

        pg.sweep_expired();

        assert_eq!(pg.grants.len(), 1, "only live grant should remain");
        assert_eq!(pg.grants[0].kind, GrantKind::Secret("live-key".into()));
    }

    #[test]
    fn duplicate_grant_for_same_key_both_tracked() {
        let mut pg = PermissionGrants::default();
        pg.add(GrantKind::Secret("my-key".into()), Duration::from_secs(60));
        pg.add(GrantKind::Secret("my-key".into()), Duration::from_secs(60));

        // Both grants are stored; is_active just checks any match.
        assert_eq!(pg.grants.len(), 2);
        assert!(pg.is_active(&GrantKind::Secret("my-key".into())));

        // After revoking all, none remain.
        pg.revoke_all();
        assert!(pg.grants.is_empty());
    }
}
