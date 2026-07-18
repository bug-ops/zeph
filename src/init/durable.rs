// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wizard step for the durable execution layer (spec-064, #4949).
//!
//! Prompts whether to enable durable execution, the backend, and optional retention overrides, and
//! generates a fresh AEAD `ZEPH_DURABLE_KEY`. The key is stored in the age vault during the review
//! step ([`store_durable_key`]), never written inline in the config TOML (vault contract spec-038).
//!
//! A pre-existing `ZEPH_DURABLE_KEY` is reused by default: replacing it here is a **destructive
//! reset** — it renders every payload already sealed in `durable_journal` unrecoverable, since
//! the AEAD key is required to authenticate them on replay, and opens no rotation window.
//! Confirming requires typing an explicit phrase (#5874). For a safe rotation that keeps
//! previously-sealed payloads readable during a drain window, use `zeph durable rotate-key`
//! (#6447) instead of this wizard step.

use dialoguer::{Confirm, Input, Select};
use zeph_core::config::DurableBackend;

use super::WizardState;

/// Confirmation phrase the user must type to rotate an existing `ZEPH_DURABLE_KEY` (#5874).
const ROTATE_CONFIRMATION_PHRASE: &str = "rotate";

/// Whether typed `input` matches the confirmation phrase required to rotate an existing
/// `ZEPH_DURABLE_KEY` — the safety-critical check behind the #5874 fix, extracted as a pure
/// function so it can be unit-tested without a `dialoguer` prompt.
fn wants_rotation(input: &str) -> bool {
    input.trim() == ROTATE_CONFIRMATION_PHRASE
}

/// Collect durable-execution settings and generate the AEAD key when enabled.
///
/// # Errors
///
/// Returns an error if a prompt cannot be read, or if an existing `ZEPH_DURABLE_KEY` cannot be
/// detected because the age vault exists but fails to load.
pub(super) fn step_durable(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Durable Execution ==\n");

    state.durable.enabled = Confirm::new()
        .with_prompt("Enable durable execution (crash-resumable agent turns)?")
        .default(false)
        .interact()?;

    if !state.durable.enabled {
        println!();
        return Ok(());
    }

    let backends = ["local (dedicated durable.db)", "restate (external server)"];
    let backend = Select::new()
        .with_prompt("Durable backend")
        .items(backends)
        .default(0)
        .interact()?;
    state.durable.backend = if backend == 1 {
        DurableBackend::Restate
    } else {
        DurableBackend::Local
    };

    // INV-8 (encryption_gate, #5996): the gate forbids encrypt_payload = false whenever this
    // deployment's journal database is reachable by more than one process/client.
    state.durable.shared_db = Confirm::new()
        .with_prompt(
            "Is this durable journal database shared across multiple processes/containers \
             (e.g. a network-shared volume, or a shared Postgres server)?",
        )
        .default(false)
        .interact()?;

    let customize = Confirm::new()
        .with_prompt("Customize retention (TTL and size caps)?")
        .default(false)
        .interact()?;
    if customize {
        state.durable.retention.ttl_completed_secs = Input::new()
            .with_prompt("Completed-execution TTL (seconds)")
            .default(state.durable.retention.ttl_completed_secs)
            .interact_text()?;
        state.durable.retention.ttl_failed_secs = Input::new()
            .with_prompt("Failed/aborted-execution TTL (seconds)")
            .default(state.durable.retention.ttl_failed_secs)
            .interact_text()?;
        state.durable.retention.max_executions = Input::new()
            .with_prompt("Maximum stored executions")
            .default(state.durable.retention.max_executions)
            .interact_text()?;
    }

    // The key is generated here and stored in the vault during review — never inline in the TOML.
    // A pre-existing key must be preserved by default: it is the AEAD key sealing every payload
    // already written to durable_journal, and replacing it silently orphans them all
    // irrecoverably ("replay integrity check failed: sealed payload did not authenticate", #5874).
    if state.vault_backend == "age"
        && vault_has_durable_key(&zeph_core::vault::default_vault_dir())?
    {
        println!(
            "A ZEPH_DURABLE_KEY already exists in the age vault. Reusing it by default — \
             replacing it here is a DESTRUCTIVE RESET (discards existing sealed payloads, no \
             rotation window): it PERMANENTLY and IRRECOVERABLY orphans every durable payload \
             already sealed under the old key. For a safe rotation that keeps old payloads \
             readable during a drain window, run `zeph durable rotate-key` instead of this \
             wizard step."
        );
        let confirmation: String = Input::new()
            .with_prompt(format!(
                "Type \"{ROTATE_CONFIRMATION_PHRASE}\" to perform the destructive reset and \
                 discard all existing sealed payloads, or leave blank to keep the existing key"
            ))
            .allow_empty(true)
            .interact_text()?;
        if !wants_rotation(&confirmation) {
            println!("Keeping the existing ZEPH_DURABLE_KEY.\n");
            return Ok(());
        }
        println!(
            "Performing destructive reset of ZEPH_DURABLE_KEY — previously sealed durable \
             payloads will be lost."
        );
    }

    state.durable_key_b64 = Some(zeph_core::durable::generate_durable_key_b64());
    println!("Generated a new ZEPH_DURABLE_KEY (stored in the age vault during review).");
    println!();
    Ok(())
}

/// Whether the age vault under `dir` already holds a `ZEPH_DURABLE_KEY` entry.
///
/// Returns `false` when the vault has not been initialized yet, in which case there is nothing
/// to preserve and a fresh key is safe to generate.
///
/// # Errors
///
/// Returns an error if the vault exists but cannot be loaded (corrupt file, wrong key, etc.).
fn vault_has_durable_key(dir: &std::path::Path) -> anyhow::Result<bool> {
    let key_path = dir.join("vault-key.txt");
    let vault_path = dir.join("secrets.age");
    if !key_path.exists() || !vault_path.exists() {
        return Ok(false);
    }
    let provider = zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path)
        .map_err(|e| anyhow::anyhow!("failed to load age vault: {e}"))?;
    Ok(provider.list_keys().contains(&"ZEPH_DURABLE_KEY"))
}

/// Store the generated `ZEPH_DURABLE_KEY` in the age vault (INV-5 / vault contract spec-038).
///
/// Initializes the vault if it does not yet exist. A no-op unless durable execution is enabled with
/// a generated key. When the `env` secrets backend is selected, prints guidance instead of writing,
/// since the durable key must live in the age vault.
///
/// # Errors
///
/// Returns an error if the vault cannot be initialized, loaded, or saved.
pub(super) fn store_durable_key(state: &WizardState) -> anyhow::Result<()> {
    let Some(key) = state.durable_key_b64.as_deref() else {
        return Ok(());
    };
    if !state.durable.enabled {
        return Ok(());
    }
    if state.vault_backend != "age" {
        println!(
            "Durable execution stores its AEAD key in the age vault. Re-run `zeph --init` with the \
             age backend, then the key will be stored automatically."
        );
        return Ok(());
    }

    let dir = zeph_core::vault::default_vault_dir();
    let key_path = dir.join("vault-key.txt");
    let vault_path = dir.join("secrets.age");
    if !key_path.exists() || !vault_path.exists() {
        zeph_core::vault::AgeVaultProvider::init_vault(&dir)
            .map_err(|e| anyhow::anyhow!("failed to initialize age vault: {e}"))?;
    }
    let mut provider = zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path)
        .map_err(|e| anyhow::anyhow!("failed to load age vault: {e}"))?;
    // Reaching this point already implies the caller's own rotation gate approved an
    // overwrite (no pre-existing key, or the user typed the "rotate" confirmation above).
    provider
        .set_secret_mut("ZEPH_DURABLE_KEY".to_owned(), key.to_owned(), true)
        .map_err(|e| anyhow::anyhow!("failed to set ZEPH_DURABLE_KEY: {e}"))?;
    provider
        .save()
        .map_err(|e| anyhow::anyhow!("failed to save age vault: {e}"))?;
    println!(
        "Stored ZEPH_DURABLE_KEY in the age vault ({}).",
        vault_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::{WizardState, store_durable_key, vault_has_durable_key, wants_rotation};

    /// RAII guard that points `zeph_core::vault::default_vault_dir()` at a temp dir for the
    /// duration of a test (via `XDG_CONFIG_HOME`) and restores the prior value on drop, mirroring
    /// the pattern in `src/commands/durable.rs`. Tests using this guard must be `#[serial]` since
    /// `XDG_CONFIG_HOME` is process-global.
    struct VaultDirGuard {
        _dir: tempfile::TempDir,
        prev_xdg: Option<String>,
    }

    impl VaultDirGuard {
        #[allow(unsafe_code)]
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", dir.path());
            }
            Self {
                _dir: dir,
                prev_xdg,
            }
        }
    }

    impl Drop for VaultDirGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            unsafe {
                match &self.prev_xdg {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    fn durable_state(durable_key_b64: Option<String>) -> WizardState {
        let durable = zeph_core::config::DurableConfig {
            enabled: true,
            ..zeph_core::config::DurableConfig::default()
        };
        WizardState {
            vault_backend: "age".into(),
            durable,
            durable_key_b64,
            ..WizardState::default()
        }
    }

    /// Regression for #5874: when the rotate confirmation is declined, `step_durable` leaves
    /// `durable_key_b64` as `None`, which must make `store_durable_key` a true no-op — the
    /// pre-existing vault entry is never touched.
    #[test]
    #[serial]
    fn store_durable_key_noop_preserves_existing_key_when_b64_none() {
        let _guard = VaultDirGuard::new();
        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).expect("init vault");
        let key_path = vault_root.join("vault-key.txt");
        let vault_path = vault_root.join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("load vault");
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY".to_owned(),
                "existing-key".to_owned(),
                false,
            )
            .expect("set secret");
        provider.save().expect("save vault");

        let state = durable_state(None);
        store_durable_key(&state).expect("store_durable_key must not error");

        let reloaded =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("reload vault");
        assert_eq!(reloaded.get("ZEPH_DURABLE_KEY"), Some("existing-key"));
    }

    /// Regression for #5874: when the rotate confirmation is typed, `step_durable` sets
    /// `durable_key_b64` to a freshly generated key, and `store_durable_key` must overwrite the
    /// pre-existing vault entry with it.
    #[test]
    #[serial]
    fn store_durable_key_overwrites_existing_key_when_b64_some() {
        let _guard = VaultDirGuard::new();
        let vault_root = zeph_core::vault::default_vault_dir();
        zeph_core::vault::AgeVaultProvider::init_vault(&vault_root).expect("init vault");
        let key_path = vault_root.join("vault-key.txt");
        let vault_path = vault_root.join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("load vault");
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY".to_owned(),
                "existing-key".to_owned(),
                false,
            )
            .expect("set secret");
        provider.save().expect("save vault");

        let new_key = zeph_core::durable::generate_durable_key_b64();
        let state = durable_state(Some(new_key.clone()));
        store_durable_key(&state).expect("store_durable_key must not error");

        let reloaded =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("reload vault");
        assert_eq!(reloaded.get("ZEPH_DURABLE_KEY"), Some(new_key.as_str()));
    }

    /// Regression for #5874: fresh setup (no vault initialized yet) must proceed straight to
    /// generate+store without regressing on the no-existing-key happy path.
    #[test]
    #[serial]
    fn store_durable_key_generates_fresh_key_when_no_existing_vault() {
        let _guard = VaultDirGuard::new();
        let vault_root = zeph_core::vault::default_vault_dir();
        assert!(!vault_has_durable_key(&vault_root).expect("vault must not exist yet"));

        let new_key = zeph_core::durable::generate_durable_key_b64();
        let state = durable_state(Some(new_key.clone()));
        store_durable_key(&state).expect("store_durable_key must not error");

        let key_path = vault_root.join("vault-key.txt");
        let vault_path = vault_root.join("secrets.age");
        let reloaded =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("load vault");
        assert_eq!(reloaded.get("ZEPH_DURABLE_KEY"), Some(new_key.as_str()));
    }

    /// Regression for #5874: only the exact lowercase phrase confirms rotation.
    #[test]
    fn wants_rotation_true_for_exact_phrase() {
        assert!(wants_rotation("rotate"));
    }

    /// Regression for #5874: surrounding whitespace from the prompt is trimmed before comparison.
    #[test]
    fn wants_rotation_true_for_phrase_with_surrounding_whitespace() {
        assert!(wants_rotation("  rotate  "));
    }

    /// Regression for #5874: blank input (the documented "leave blank to keep the existing key"
    /// path) must decline rotation.
    #[test]
    fn wants_rotation_false_for_blank_input() {
        assert!(!wants_rotation(""));
        assert!(!wants_rotation("   "));
    }

    /// Regression for #5874: the match is case-sensitive — "Rotate"/"ROTATE" must NOT confirm,
    /// since a typo-prone case-insensitive match would weaken the intentional friction of the
    /// confirmation phrase.
    #[test]
    fn wants_rotation_false_for_wrong_case() {
        assert!(!wants_rotation("Rotate"));
        assert!(!wants_rotation("ROTATE"));
    }

    /// Regression for #5874: any other input (typos, unrelated text) must decline rotation.
    #[test]
    fn wants_rotation_false_for_unrelated_input() {
        assert!(!wants_rotation("yes"));
        assert!(!wants_rotation("rotate key"));
    }

    #[test]
    fn vault_has_durable_key_false_when_vault_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!vault_has_durable_key(dir.path()).expect("should not error"));
    }

    #[test]
    fn vault_has_durable_key_true_when_key_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).expect("init vault");
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("load vault");
        provider
            .set_secret_mut(
                "ZEPH_DURABLE_KEY".to_owned(),
                "existing-key".to_owned(),
                false,
            )
            .expect("set secret");
        provider.save().expect("save vault");

        assert!(vault_has_durable_key(dir.path()).expect("should not error"));
    }

    #[test]
    fn vault_has_durable_key_false_when_other_secrets_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).expect("init vault");
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        let mut provider =
            zeph_core::vault::AgeVaultProvider::load(&key_path, &vault_path).expect("load vault");
        provider
            .set_secret_mut("ZEPH_OTHER_KEY".to_owned(), "value".to_owned(), false)
            .expect("set secret");
        provider.save().expect("save vault");

        assert!(!vault_has_durable_key(dir.path()).expect("should not error"));
    }
}
