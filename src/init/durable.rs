// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wizard step for the durable execution layer (spec-064, #4949).
//!
//! Prompts whether to enable durable execution, the backend, and optional retention overrides, and
//! generates a fresh AEAD `ZEPH_DURABLE_KEY`. The key is stored in the age vault during the review
//! step ([`store_durable_key`]), never written inline in the config TOML (vault contract spec-038).

use dialoguer::{Confirm, Input, Select};
use zeph_core::config::DurableBackend;

use super::WizardState;

/// Collect durable-execution settings and generate the AEAD key when enabled.
///
/// # Errors
///
/// Returns an error if a prompt cannot be read.
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
    state.durable_key_b64 = Some(zeph_core::durable::generate_durable_key_b64());
    println!("Generated a new ZEPH_DURABLE_KEY (stored in the age vault during review).");
    println!();
    Ok(())
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
    provider.set_secret_mut("ZEPH_DURABLE_KEY".to_owned(), key.to_owned());
    provider
        .save()
        .map_err(|e| anyhow::anyhow!("failed to save age vault: {e}"))?;
    println!(
        "Stored ZEPH_DURABLE_KEY in the age vault ({}).",
        vault_path.display()
    );
    Ok(())
}
