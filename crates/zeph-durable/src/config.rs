// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable configuration and the AEAD enforcement gate.
//!
//! The pure-data configuration types ([`DurableConfig`], [`RetentionPolicy`], [`DurableBackend`])
//! live in `zeph-config` so the aggregate [`Config`](zeph_config::Config) can hold them without
//! pulling this crate's `zeph-db`/`sqlx` dependency tree onto the config layer. They are re-exported
//! here for ergonomic access from the engine APIs that consume them ([`DurableContext`] and
//! [`JournalWriter`]).
//!
//! On top of the data, this module owns the **security policy**: [`encryption_gate`] evaluates the
//! INV-8 AEAD requirement for a deployment. The policy lives next to [`DurableError`] and the cipher
//! contract (in this crate), not with the pure data.
//!
//! [`DurableContext`]: crate::DurableContext
//! [`JournalWriter`]: crate::JournalWriter

pub use zeph_config::{DurableBackend, DurableConfig, RetentionPolicy};

use crate::error::DurableError;

/// Outcome of evaluating the INV-8 AEAD requirement for a deployment.
///
/// Returned by [`encryption_gate`]. The error case ([`DurableError::EncryptionRequired`]) covers the
/// forbidden combinations; this enum distinguishes the two *permitted* outcomes so the caller can act
/// on the development-override warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionGate {
    /// AEAD payload encryption is enabled — proceed normally.
    Enabled,
    /// AEAD is disabled on a single-user local backend (a development override). The caller MUST
    /// emit a startup `WARN` so the weakened posture is visible in the logs.
    DisabledLocalWarn,
}

/// Evaluate whether the configured `encrypt_payload` setting is permitted for this deployment
/// (INV-8).
///
/// AEAD is default-on. Disabling it is a development-only override that is permitted **only** for a
/// single-user local backend on a non-shared database. `shared_db` MUST be `true` whenever the
/// journal lives on a multi-client database (Postgres, or any file shared across processes), where
/// the DB-file trust boundary does not hold.
///
/// # Errors
///
/// Returns [`DurableError::EncryptionRequired`] when `encrypt_payload = false` is combined with a
/// non-local backend or a shared database.
///
/// # Examples
///
/// ```
/// use zeph_durable::{DurableBackend, DurableConfig, EncryptionGate, encryption_gate};
///
/// // Default config keeps AEAD on regardless of deployment.
/// let cfg = DurableConfig::default();
/// assert_eq!(encryption_gate(&cfg, true).unwrap(), EncryptionGate::Enabled);
///
/// // Disabling AEAD is tolerated only on a single-user local backend.
/// let dev = DurableConfig { encrypt_payload: false, ..DurableConfig::default() };
/// assert_eq!(encryption_gate(&dev, false).unwrap(), EncryptionGate::DisabledLocalWarn);
/// assert!(encryption_gate(&dev, true).is_err(), "forbidden on a shared database");
/// ```
pub fn encryption_gate(
    cfg: &DurableConfig,
    shared_db: bool,
) -> Result<EncryptionGate, DurableError> {
    if cfg.encrypt_payload {
        return Ok(EncryptionGate::Enabled);
    }
    if cfg.backend != DurableBackend::Local {
        return Err(DurableError::EncryptionRequired { context: "restate" });
    }
    if shared_db {
        return Err(DurableError::EncryptionRequired {
            context: "shared-database",
        });
    }
    Ok(EncryptionGate::DisabledLocalWarn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_gate_passes_when_aead_enabled() {
        let cfg = DurableConfig::default();
        assert!(cfg.encrypt_payload);
        assert_eq!(
            encryption_gate(&cfg, false).unwrap(),
            EncryptionGate::Enabled
        );
        assert_eq!(
            encryption_gate(&cfg, true).unwrap(),
            EncryptionGate::Enabled
        );
        let restate = DurableConfig {
            backend: DurableBackend::Restate,
            ..DurableConfig::default()
        };
        assert_eq!(
            encryption_gate(&restate, true).unwrap(),
            EncryptionGate::Enabled
        );
    }

    #[test]
    fn encryption_gate_warns_for_local_single_user_override() {
        let cfg = DurableConfig {
            encrypt_payload: false,
            backend: DurableBackend::Local,
            ..DurableConfig::default()
        };
        assert_eq!(
            encryption_gate(&cfg, false).unwrap(),
            EncryptionGate::DisabledLocalWarn
        );
    }

    #[test]
    fn encryption_gate_rejects_disabled_aead_on_shared_or_restate() {
        let local_shared = DurableConfig {
            encrypt_payload: false,
            backend: DurableBackend::Local,
            ..DurableConfig::default()
        };
        assert!(matches!(
            encryption_gate(&local_shared, true),
            Err(DurableError::EncryptionRequired {
                context: "shared-database"
            })
        ));

        let restate = DurableConfig {
            encrypt_payload: false,
            backend: DurableBackend::Restate,
            ..DurableConfig::default()
        };
        assert!(matches!(
            encryption_gate(&restate, false),
            Err(DurableError::EncryptionRequired { context: "restate" })
        ));
    }
}
