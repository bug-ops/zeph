// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared helpers for `zeph skill search`/`get` and `zeph plugin search`/`get` (spec-045,
//! #5869): vault token resolution and [`RegistryClient`] construction from config.
//!
//! Gated by the `registry` Cargo feature (mirrors `zeph_plugins::marketplace`) — this module
//! is the only place in the binary crate that names a `zeph_plugins::marketplace::` path, so
//! `src/commands/skill.rs`/`plugin.rs` stay compilable with the feature off (M1, critic
//! handoff `.local/handoff/2026-07-10T19-29-13-critic.md`).

use std::path::Path;

use zeph_config::RegistryBackendKind;
use zeph_core::config::Config;
use zeph_core::vault::{AgeVaultProvider, EnvVaultProvider, Secret, VaultProvider};
use zeph_plugins::marketplace::RegistryClient;
use zeph_plugins::marketplace::skills_sh::{DEFAULT_BASE_URL, SkillsShClient};

/// Resolve the registry's bearer credential from the vault, honoring the opt-in gate.
///
/// Returns `Ok(None)` without touching the vault when the registry is disabled or no
/// `auth_vault_key` is configured — this is what keeps `skills.registry.enabled = false` (the
/// default) a true zero-network-and-zero-vault-access state (NFR-001). Returns a [`Secret`]
/// (Debug/Display-redacted), matching the newtype used for every other vault-resolved
/// credential in this workspace (review fix #7) — the CLI marketplace commands resolve their
/// own token independently of `Config::resolve_secrets_masked`'s startup path (that path is
/// only run for the main agent bootstrap, not these lightweight one-shot CLI verbs), so this is
/// not registered with the LLM-context secret-mask registry; low risk, since a one-shot CLI
/// invocation never feeds this value into an LLM-bound context.
///
/// `vault_override`/`vault_key_override`/`vault_path_override` are the `--vault`/`--vault-key`/
/// `--vault-path` CLI overrides, threaded through via [`crate::bootstrap::resolve_vault_paths`]
/// (#6591) — without them the `Age` backend silently ignored the overrides and always loaded
/// `default_vault_dir()`, diverging from every other vault-backed CLI command.
///
/// # Errors
///
/// Returns an error if the configured vault backend cannot be loaded (e.g. missing age key
/// file) or the lookup itself fails.
#[tracing::instrument(name = "registry_client.resolve_token", skip(config))]
pub(crate) async fn resolve_registry_token(
    config: &Config,
    vault_override: Option<&str>,
    vault_key_override: Option<&Path>,
    vault_path_override: Option<&Path>,
) -> anyhow::Result<Option<Secret>> {
    let reg = &config.skills.registry;
    if !reg.enabled {
        return Ok(None);
    }
    let Some(key_name) = reg.auth_vault_key.as_deref() else {
        return Ok(None);
    };

    let vault: Box<dyn VaultProvider> = match config.vault.backend {
        zeph_config::VaultBackend::Env => Box::new(EnvVaultProvider),
        zeph_config::VaultBackend::Age => {
            let (vault_key_path, vault_secrets_path) = crate::bootstrap::resolve_vault_paths(
                config,
                vault_override,
                vault_key_override,
                vault_path_override,
            );
            let provider = AgeVaultProvider::load_async(&vault_key_path, &vault_secrets_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            Box::new(provider)
        }
        zeph_config::VaultBackend::Keyring => {
            anyhow::bail!("keyring vault backend is not yet implemented");
        }
        // VaultBackend is #[non_exhaustive]; future backends fail loudly here rather than
        // silently resolving no token.
        other => anyhow::bail!("unsupported vault backend: {other}"),
    };

    let value = vault
        .get_secret(key_name)
        .await
        .map_err(|e| anyhow::anyhow!("vault error resolving {key_name}: {e}"))?;
    Ok(value.map(Secret::new))
}

/// Build the configured [`RegistryClient`] backend.
///
/// Currently the only backend is [`RegistryBackendKind::SkillsSh`] (FR-005's pluggability is
/// proven by `MockRegistryClient` in `zeph-plugins`' test suite, not by a second shipped
/// production backend — see spec-045 Open Questions).
pub(crate) fn build_registry_client(
    config: &Config,
    token: Option<Secret>,
) -> Box<dyn RegistryClient> {
    let reg = &config.skills.registry;
    match reg.backend_kind {
        RegistryBackendKind::SkillsSh => {
            let base_url = reg
                .backend_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
            Box::new(SkillsShClient::new(
                base_url,
                token,
                reg.registry_timeout_secs,
            ))
        }
        // RegistryBackendKind is #[non_exhaustive]; fall back to the default (only shipped)
        // backend rather than failing to compile when a future variant is added upstream.
        _ => Box::new(SkillsShClient::new(
            DEFAULT_BASE_URL.to_owned(),
            token,
            reg.registry_timeout_secs,
        )),
    }
}

/// The standard "registry not configured" message for FR-004.
///
/// Fires when `enabled = false` (the default).
pub(crate) const REGISTRY_NOT_CONFIGURED_MSG: &str = "no skill/plugin registry is configured. Add `[skills.registry] enabled = true` (and a \
     backend_kind/backend_url) to config.toml, or run `zeph --init` to configure it \
     interactively. See `zeph skill search --help`.";

/// Print search results in the shared format used by both `skill search` and `plugin search`
/// (NFR-006 — one registry client implementation, one result-rendering path).
pub(crate) fn print_search_results(entries: &[zeph_plugins::marketplace::RegistryEntry]) {
    if entries.is_empty() {
        println!("No results.");
        return;
    }
    println!("Found {} result(s):\n", entries.len());
    for entry in entries {
        if entry.tags.is_empty() {
            println!("  {} — {}", entry.registry_id, entry.description);
        } else {
            println!(
                "  {} — {} [{}]",
                entry.registry_id,
                entry.description,
                entry.tags.join(", ")
            );
        }
        println!("      name: {}", entry.name);
        if let Some(author) = &entry.author {
            println!("      author: {author}");
        }
        if let Some(status) = &entry.security_audit_status {
            println!("      security audit: {status}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_plugins::marketplace::RegistryEntry;

    #[tokio::test]
    async fn resolve_registry_token_returns_none_without_vault_access_when_disabled() {
        // FR-004/NFR-001: the architect's stated highest-priority test — disabled registry must
        // short-circuit before any vault backend is even constructed. `config.vault.backend`
        // defaults to `Env`, which never touches disk, so a `None` result together with a
        // config that has no auth_vault_key set proves the early return fired, not that the
        // (trivial) Env lookup happened to also return None.
        let mut config = Config::default();
        config.skills.registry.enabled = false;
        config.skills.registry.auth_vault_key = Some("ZEPH_SKILL_REGISTRY_TOKEN".to_owned());

        let result = resolve_registry_token(&config, None, None, None)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_registry_token_returns_none_when_no_auth_key_configured() {
        let mut config = Config::default();
        config.skills.registry.enabled = true;
        config.skills.registry.auth_vault_key = None;

        let result = resolve_registry_token(&config, None, None, None)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_registry_token_resolves_from_env_backend() {
        let key = "ZEPH_TEST_REGISTRY_TOKEN_RESOLVE";
        #[allow(unsafe_code)] // scoped env mutation; nextest isolates tests per-process
        unsafe {
            std::env::set_var(key, "test-token-value");
        }

        let mut config = Config::default();
        config.skills.registry.enabled = true;
        config.skills.registry.auth_vault_key = Some(key.to_owned());
        config.vault.backend = zeph_config::VaultBackend::Env;

        let result = resolve_registry_token(&config, None, None, None)
            .await
            .unwrap();

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var(key);
        }

        assert_eq!(
            result.map(|s| s.expose().to_owned()),
            Some("test-token-value".to_owned())
        );
    }

    /// #6591: proves the `Age` backend actually reads from the CLI `--vault-key`/`--vault-path`
    /// override, not from `default_vault_dir()`. `XDG_CONFIG_HOME` is pointed at an empty
    /// tempdir with no vault files, so a silent fallback to the default dir would surface as an
    /// `Err` here rather than resolving the secret — proving the override path was actually
    /// used to read it, not merely accepted and ignored.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_registry_token_age_backend_uses_vault_override_paths() {
        let override_dir = tempfile::tempdir().expect("override tempdir");
        let default_dir = tempfile::tempdir().expect("default tempdir");
        let key_path = override_dir.path().join("vault-key.txt");
        let vault_path = override_dir.path().join("secrets.age");

        AgeVaultProvider::init_vault_at(&key_path, &vault_path, false).expect("init vault");
        let mut vault = AgeVaultProvider::load_async(&key_path, &vault_path)
            .await
            .expect("load fresh vault");
        vault
            .set_secret_mut(
                "ZEPH_TEST_REGISTRY_TOKEN_AGE".to_owned(),
                "override-token-value".to_owned(),
                false,
            )
            .expect("seed secret");
        vault.save_async().await.expect("persist vault");

        #[allow(unsafe_code)] // scoped env mutation; #[serial] avoids cross-test races
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", default_dir.path());
        }

        let mut config = Config::default();
        config.skills.registry.enabled = true;
        config.skills.registry.auth_vault_key = Some("ZEPH_TEST_REGISTRY_TOKEN_AGE".to_owned());
        config.vault.backend = zeph_config::VaultBackend::Age;

        let result =
            resolve_registry_token(&config, None, Some(&key_path), Some(&vault_path)).await;

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        let secret = result
            .expect("override vault must load successfully")
            .expect("secret must resolve from the override path");
        assert_eq!(secret.expose(), "override-token-value");
    }

    /// Companion sanity check: with no `--vault-key`/`--vault-path` override, resolution must
    /// still attempt `default_vault_dir()` (pointed at an empty tempdir via `XDG_CONFIG_HOME`)
    /// rather than silently succeeding some other way — a missing vault there must surface as
    /// an `Err`, matching pre-#6591 default-path behavior.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_registry_token_age_backend_falls_back_to_default_vault_dir_without_override() {
        let default_dir = tempfile::tempdir().expect("default tempdir");

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", default_dir.path());
        }

        let mut config = Config::default();
        config.skills.registry.enabled = true;
        config.skills.registry.auth_vault_key = Some("ZEPH_TEST_REGISTRY_TOKEN_AGE".to_owned());
        config.vault.backend = zeph_config::VaultBackend::Age;

        let result = resolve_registry_token(&config, None, None, None).await;

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }

        assert!(
            result.is_err(),
            "no vault at default_vault_dir() must surface as an error, proving the default \
             path (not a silent override bypass) was exercised"
        );
    }

    #[test]
    fn print_search_results_handles_empty_slice() {
        // Pure formatting — assert it doesn't panic on the empty-results path (review
        // suggestion #13). No stdout capture; this is a crash-safety smoke test.
        print_search_results(&[]);
    }

    #[test]
    fn print_search_results_handles_populated_slice() {
        let entries = vec![RegistryEntry {
            registry_id: "acme/x".to_owned(),
            name: "X".to_owned(),
            description: "a tool".to_owned(),
            tags: vec!["tag".to_owned()],
            author: Some("acme".to_owned()),
            security_audit_status: Some("pass".to_owned()),
        }];
        print_search_results(&entries);
    }
}
