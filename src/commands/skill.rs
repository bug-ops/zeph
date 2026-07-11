// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::cli::SkillCommand;

#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_skill_command(
    cmd: SkillCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, managed_skills_dir, resolve_config_path};
    use std::collections::HashMap;
    use zeph_skills::manager::SkillManager;

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file);

    let managed_dir = managed_skills_dir();
    std::fs::create_dir_all(&managed_dir)
        .map_err(|e| anyhow::anyhow!("failed to create managed skills dir: {e}"))?;

    let mgr = SkillManager::new(managed_dir.clone());

    let sqlite_path = crate::db_url::resolve_db_url(&config).to_owned();

    match cmd {
        SkillCommand::Install { source } => {
            let result = if source.starts_with("http://")
                || source.starts_with("https://")
                || source.starts_with("git@")
            {
                mgr.install_from_url(&source)
            } else {
                mgr.install_from_path(std::path::Path::new(&source))
            }
            .map_err(|e| anyhow::anyhow!("{e}"))?;

            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            let (source_kind, source_url, source_path) = match &result.source {
                zeph_skills::SkillSource::Hub { url } => (
                    zeph_memory::store::SourceKind::Hub,
                    Some(url.as_str()),
                    None,
                ),
                zeph_skills::SkillSource::File { path } => (
                    zeph_memory::store::SourceKind::File,
                    None,
                    Some(path.to_string_lossy().into_owned()),
                ),
                zeph_skills::SkillSource::Local => {
                    (zeph_memory::store::SourceKind::Local, None, None)
                }
                _ => (zeph_memory::store::SourceKind::Local, None, None),
            };
            store
                .upsert_skill_trust(
                    &result.name,
                    zeph_common::SkillTrustLevel::Quarantined,
                    source_kind,
                    source_url,
                    source_path.as_deref(),
                    &result.blake3_hash,
                )
                .await
                .map_err(|e| anyhow::anyhow!("trust upsert failed: {e}"))?;

            println!(
                "Installed skill \"{}\" (hash: {}..., trust: quarantined)",
                result.name,
                &result.blake3_hash[..8]
            );

            let skill_md = managed_dir.join(&result.name).join("SKILL.md");
            if let Ok(meta) = zeph_skills::loader::load_skill_meta(&skill_md)
                && !meta.requires_secrets.is_empty()
            {
                println!(
                    "  Note: this skill requires secrets: {}",
                    meta.requires_secrets.join(", ")
                );
                println!("  Run `zeph vault set ZEPH_SECRET_<NAME> <value>` for each.");
            }
        }

        SkillCommand::Remove { name } => {
            mgr.remove(&name).map_err(|e| anyhow::anyhow!("{e}"))?;

            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            store
                .delete_skill_trust(&name)
                .await
                .map_err(|e| anyhow::anyhow!("trust delete failed: {e}"))?;
            println!("Removed skill \"{name}\".");
        }

        SkillCommand::List => {
            let installed = mgr.list_installed().map_err(|e| anyhow::anyhow!("{e}"))?;
            if installed.is_empty() {
                println!("No skills installed in {}.", managed_dir.display());
                return Ok(());
            }
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            println!("Installed skills ({}):\n", installed.len());
            for skill in &installed {
                let trust = store
                    .load_skill_trust(&skill.name)
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(
                        || "no trust record".to_owned(),
                        |r| r.trust_level.to_string(),
                    );
                if skill.requires_secrets.is_empty() {
                    println!("  {} — {} [{}]", skill.name, skill.description, trust);
                } else {
                    println!(
                        "  {} — {} [{}] (requires: {})",
                        skill.name,
                        skill.description,
                        trust,
                        skill.requires_secrets.join(", "),
                    );
                }
            }
        }

        SkillCommand::Verify { name } => {
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;

            if let Some(name) = name {
                let current_hash = mgr.verify(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
                let stored = store
                    .load_skill_trust(&name)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.blake3_hash);
                match stored {
                    Some(ref h) if h == &current_hash => {
                        println!("{name}: OK (hash matches)");
                    }
                    Some(_) => {
                        println!("{name}: MISMATCH (hash changed, setting to quarantined)");
                        store
                            .set_skill_trust_level(&name, zeph_common::SkillTrustLevel::Quarantined)
                            .await
                            .map_err(|e| anyhow::anyhow!("trust update failed: {e}"))?;
                        store
                            .update_skill_hash(&name, &current_hash)
                            .await
                            .map_err(|e| anyhow::anyhow!("hash update failed: {e}"))?;
                    }
                    None => {
                        println!("{name}: no stored hash (hash: {}...)", &current_hash[..8]);
                    }
                }
            } else {
                // Verify all.
                let rows = store
                    .load_all_skill_trust()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let stored_hashes: HashMap<String, String> = rows
                    .into_iter()
                    .map(|r| (r.skill_name, r.blake3_hash))
                    .collect();
                let results = mgr
                    .verify_all(&stored_hashes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                for r in &results {
                    match r.stored_hash_matches {
                        Some(true) => println!("{}: OK", r.name),
                        Some(false) => {
                            println!("{}: MISMATCH (setting to quarantined)", r.name);
                            store
                                .set_skill_trust_level(
                                    &r.name,
                                    zeph_common::SkillTrustLevel::Quarantined,
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("trust update failed: {e}"))?;
                            store
                                .update_skill_hash(&r.name, &r.current_hash)
                                .await
                                .map_err(|e| anyhow::anyhow!("hash update failed: {e}"))?;
                        }
                        None => println!("{}: no stored hash", r.name),
                    }
                }
            }
        }

        SkillCommand::Trust {
            name,
            level,
            require_check,
        } => {
            let trust_level = level.parse::<zeph_common::SkillTrustLevel>().map_err(|_| {
                anyhow::anyhow!(
                    "invalid trust level: {level}. Use: trusted, verified, quarantined, blocked"
                )
            })?;

            // REV-003: re-verify hash before promoting to trusted/verified.
            let store = if matches!(
                trust_level,
                zeph_common::SkillTrustLevel::Trusted | zeph_common::SkillTrustLevel::Verified
            ) {
                let managed_dir = crate::bootstrap::managed_skills_dir();
                let mgr = zeph_skills::manager::SkillManager::new(managed_dir.clone());
                let name_clone = name.clone();
                let current_hash = tokio::task::spawn_blocking(move || mgr.verify(&name_clone))
                    .await
                    .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))??;

                let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
                let row = store
                    .load_skill_trust(&name)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                match row {
                    None => anyhow::bail!("skill \"{name}\" not found in trust database"),
                    Some(r) if r.blake3_hash != current_hash => {
                        anyhow::bail!(
                            "hash mismatch for \"{name}\" — run `zeph skill verify {name}` first"
                        );
                    }
                    Some(_) => {}
                }
                store
            } else {
                zeph_memory::store::SqliteStore::new(&sqlite_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?
            };

            let updated = store
                .set_skill_trust_level(&name, trust_level)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if !updated {
                anyhow::bail!("skill \"{name}\" not found in trust database");
            }
            println!("Trust level for \"{name}\" set to {trust_level}.");

            // #6080: expose the previously unreachable `requires_trust_check` setter so the
            // per-invocation blake3 re-check (`SkillTrustGate::resolve_body`) can actually be
            // armed for a skill.
            if require_check {
                store
                    .set_requires_trust_check(&name, true)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Per-invocation integrity re-check enabled for \"{name}\".");
            }
        }

        SkillCommand::Block { name } => {
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            let updated = store
                .set_skill_trust_level(&name, zeph_common::SkillTrustLevel::Blocked)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if updated {
                println!("Skill \"{name}\" blocked.");
            } else {
                anyhow::bail!("skill \"{name}\" not found in trust database");
            }
        }

        SkillCommand::Unblock { name } => {
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            let updated = store
                .set_skill_trust_level(&name, zeph_common::SkillTrustLevel::Quarantined)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if updated {
                println!("Skill \"{name}\" unblocked (set to quarantined).");
            } else {
                anyhow::bail!("skill \"{name}\" not found in trust database");
            }
        }

        SkillCommand::Invoke { name, args } => {
            use std::collections::HashMap;
            use std::sync::Arc;

            use parking_lot::RwLock;
            use zeph_core::{SkillBodyResolution, SkillTrustGate, SkillTrustSnapshot};
            use zeph_skills::prompt::sanitize_skill_text;

            let registry = Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::load(&[
                managed_dir,
            ])));

            // Load the full trust map so the CLI preview observes the exact same trust state
            // (including `requires_trust_check`) as the agent's `load_skill`/`invoke_skill`
            // tools — see `SkillTrustGate` (#6079).
            let trust_snapshot: HashMap<String, SkillTrustSnapshot> = {
                let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
                store
                    .load_all_skill_trust()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .into_iter()
                    .map(|r| {
                        (
                            r.skill_name,
                            SkillTrustSnapshot {
                                trust_level: r.trust_level,
                                requires_trust_check: r.requires_trust_check,
                                blake3_hash: r.blake3_hash,
                            },
                        )
                    })
                    .collect()
            };

            let gate = SkillTrustGate::new(registry, Arc::new(RwLock::new(trust_snapshot)));
            let body = match gate
                .resolve_body(&name)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                SkillBodyResolution::Refused(message) | SkillBodyResolution::NotFound(message) => {
                    anyhow::bail!("{message}");
                }
                SkillBodyResolution::Body(body) => body,
            };

            match args {
                Some(a) => {
                    let args_safe = sanitize_skill_text(&a);
                    println!("{body}\n\n<args>\n{args_safe}\n</args>");
                }
                None => println!("{body}"),
            }
        }

        SkillCommand::PromoteHeuristics { skill } => {
            use zeph_skills::promoter::{build_promotion_prompt, compute_batch_hash};

            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;

            let erl_min_confidence = config.skills.learning.erl_min_confidence;
            let threshold = config.skills.learning.heuristic_promotion_threshold;

            let candidates: Vec<(String, i64)> = if let Some(ref name) = skill {
                // Single skill: load count directly.
                let texts = store
                    .load_heuristic_texts_for_promotion(name, erl_min_confidence)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let count = u32::try_from(texts.len()).unwrap_or(u32::MAX);
                if count < threshold {
                    println!(
                        "Skill \"{name}\" has {} heuristics (threshold: {threshold}), skipping.",
                        texts.len()
                    );
                    return Ok(());
                }
                vec![(name.clone(), i64::try_from(texts.len()).unwrap_or(i64::MAX))]
            } else {
                store
                    .count_heuristics_by_skill(erl_min_confidence, threshold)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            };

            if candidates.is_empty() {
                println!("No skills qualify for heuristic promotion (threshold: {threshold}).");
                return Ok(());
            }

            println!("Qualifying skills: {}", candidates.len());
            for (skill_name, count) in &candidates {
                let heuristics = store
                    .load_heuristic_texts_for_promotion(skill_name, erl_min_confidence)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let batch_hash = compute_batch_hash(&heuristics);

                let already = store
                    .promotion_already_evaluated(skill_name, &batch_hash)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                if already {
                    println!(
                        "  {skill_name}: already evaluated (batch_hash={batch_hash:.8}…), skipping."
                    );
                    continue;
                }

                let skill_md_path = managed_dir.join(skill_name).join("SKILL.md");
                let parent_body = match tokio::fs::read_to_string(&skill_md_path).await {
                    Ok(b) => b,
                    Err(e) => {
                        println!("  {skill_name}: skill file not found ({e}), skipping.");
                        continue;
                    }
                };

                let prompt = build_promotion_prompt(&parent_body, &heuristics);
                println!(
                    "  {skill_name}: {count} heuristics, batch_hash={batch_hash:.8}…\n  Prompt preview (first 200 chars): {}…",
                    &prompt[..prompt.len().min(200)]
                );

                // Dry-run: show what would be evaluated. A live LLM call requires
                // the agent (use `zeph run` with `heuristic_promotion_enabled = true`).
                println!(
                    "  → Use `zeph run` with heuristic_promotion_enabled=true to trigger live LLM evaluation."
                );
            }
        }

        SkillCommand::Search { query } => {
            #[cfg(feature = "registry")]
            {
                registry_search(&config, &query).await?;
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = &query;
                println!(
                    "This zeph build was compiled without the `registry` feature; rebuild \
                     with `--features registry` (or `full`) to use `zeph skill search`."
                );
            }
        }

        SkillCommand::Get { registry_id } => {
            #[cfg(feature = "registry")]
            {
                registry_get(&config, &mgr, &managed_dir, &sqlite_path, &registry_id).await?;
            }
            #[cfg(not(feature = "registry"))]
            {
                let _ = &registry_id;
                println!(
                    "This zeph build was compiled without the `registry` feature; rebuild \
                     with `--features registry` (or `full`) to use `zeph skill get`."
                );
            }
        }
    }

    Ok(())
}

/// `zeph skill search <query>` (spec-045, #5869, FR-001).
///
/// Prints [`crate::commands::registry_client::REGISTRY_NOT_CONFIGURED_MSG`] and makes zero
/// network calls when `skills.registry.enabled = false` (FR-004, NFR-001). Thin wrapper around
/// [`registry_search_with`] so the fetch/search logic itself is testable in isolation from the
/// config-gate and client construction — see that fn's tests for `MockRegistryClient`-driven
/// coverage.
#[cfg(feature = "registry")]
#[tracing::instrument(name = "skill.registry_search", skip(config), fields(query))]
async fn registry_search(config: &zeph_core::config::Config, query: &str) -> anyhow::Result<()> {
    use crate::commands::registry_client::{
        REGISTRY_NOT_CONFIGURED_MSG, build_registry_client, resolve_registry_token,
    };

    if !config.skills.registry.enabled {
        println!("{REGISTRY_NOT_CONFIGURED_MSG}");
        anyhow::bail!("skill registry is not configured");
    }

    let token = resolve_registry_token(config).await?;
    let client = build_registry_client(config, token);
    registry_search_with(client.as_ref(), query).await
}

/// Search logic parameterized over a [`zeph_plugins::marketplace::RegistryClient`] — split out
/// of [`registry_search`] so tests can drive it with `MockRegistryClient` without network or a
/// real `Config`/vault (review fix #4).
#[cfg(feature = "registry")]
async fn registry_search_with(
    client: &dyn zeph_plugins::marketplace::RegistryClient,
    query: &str,
) -> anyhow::Result<()> {
    use crate::commands::registry_client::print_search_results;

    let results = client
        .search(query)
        .await
        .map_err(|e| anyhow::anyhow!("registry search failed: {e}"))?;
    print_search_results(&results);
    Ok(())
}

/// `zeph skill get <registry-id>` (spec-045, #5869, FR-002).
///
/// Fetches the package then routes it through the exact same
/// [`zeph_skills::manager::SkillManager::install_from_path`] + Quarantined-trust-upsert flow
/// as `zeph skill install <local-path>` — no bypass of frontmatter validation or the
/// injection-pattern scan (NFR-002). Thin wrapper around [`registry_get_with`] — see that fn's
/// tests for `MockRegistryClient`-driven coverage.
#[cfg(feature = "registry")]
#[tracing::instrument(name = "skill.registry_get", skip(config, mgr), fields(registry_id))]
async fn registry_get(
    config: &zeph_core::config::Config,
    mgr: &zeph_skills::manager::SkillManager,
    managed_dir: &std::path::Path,
    sqlite_path: &str,
    registry_id: &str,
) -> anyhow::Result<()> {
    use crate::commands::registry_client::{
        REGISTRY_NOT_CONFIGURED_MSG, build_registry_client, resolve_registry_token,
    };

    if !config.skills.registry.enabled {
        println!("{REGISTRY_NOT_CONFIGURED_MSG}");
        anyhow::bail!("skill registry is not configured");
    }

    let token = resolve_registry_token(config).await?;
    let client = build_registry_client(config, token);
    registry_get_with(client.as_ref(), mgr, managed_dir, sqlite_path, registry_id).await
}

/// Fetch-and-install logic parameterized over a [`zeph_plugins::marketplace::RegistryClient`] —
/// split out of [`registry_get`] so tests can drive it with `MockRegistryClient` (review fix #4).
#[cfg(feature = "registry")]
async fn registry_get_with(
    client: &dyn zeph_plugins::marketplace::RegistryClient,
    mgr: &zeph_skills::manager::SkillManager,
    managed_dir: &std::path::Path,
    sqlite_path: &str,
    registry_id: &str,
) -> anyhow::Result<()> {
    let archive = client
        .fetch(registry_id)
        .await
        .map_err(|e| anyhow::anyhow!("registry fetch failed: {e}"))?;

    if archive.has_plugin_manifest {
        anyhow::bail!(
            "package {registry_id:?} is a full plugin bundle (contains plugin.toml); use \
             `zeph plugin get {registry_id}` instead"
        );
    }

    // install_from_path infers `SkillSource::File { path: <tempdir> }` since it only sees a
    // local directory — that path is meaningless (deleted with the TempDir on drop). Record
    // the real provenance as Hub{registry_id} instead (FR-008, architect-recommended minimal
    // scope: reuse the existing Hub source kind rather than adding a dedicated Registry variant).
    //
    // Pass `install_dir`, not `extracted_dir.path()` directly: SkillManager::install_from_path
    // requires its source directory to already be named after the skill's declared name (see
    // PackageArchive::install_dir docs) — extracted_dir's basename is an OS-assigned random
    // string.
    let result = mgr
        .install_from_path(&archive.install_dir)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let store = zeph_memory::store::SqliteStore::new(sqlite_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
    store
        .upsert_skill_trust(
            &result.name,
            zeph_common::SkillTrustLevel::Quarantined,
            zeph_memory::store::SourceKind::Hub,
            Some(registry_id),
            None,
            &result.blake3_hash,
        )
        .await
        .map_err(|e| anyhow::anyhow!("trust upsert failed: {e}"))?;

    println!(
        "Installed skill \"{}\" from registry {registry_id:?} (hash: {}..., trust: quarantined)",
        result.name,
        &result.blake3_hash[..8]
    );

    let skill_md = managed_dir.join(&result.name).join("SKILL.md");
    if let Ok(meta) = zeph_skills::loader::load_skill_meta(&skill_md)
        && !meta.requires_secrets.is_empty()
    {
        println!(
            "  Note: this skill requires secrets: {}",
            meta.requires_secrets.join(", ")
        );
        println!("  Run `zeph vault set ZEPH_SECRET_<NAME> <value>` for each.");
    }

    Ok(())
}

#[cfg(all(test, feature = "registry"))]
mod registry_tests {
    use super::*;
    use zeph_plugins::marketplace::RegistryEntry;
    use zeph_plugins::marketplace::mock::MockRegistryClient;

    fn sample_entry(id: &str, name: &str) -> RegistryEntry {
        RegistryEntry {
            registry_id: id.to_owned(),
            name: name.to_owned(),
            description: "a test skill".to_owned(),
            tags: vec![],
            author: None,
            security_audit_status: None,
        }
    }

    #[tokio::test]
    async fn registry_search_with_calls_client_and_succeeds() {
        let mock = MockRegistryClient::new().with_entry(sample_entry("acme/x", "X Tool"));
        registry_search_with(&mock, "x").await.unwrap();
    }

    #[tokio::test]
    async fn registry_search_with_propagates_client_error() {
        let mock = MockRegistryClient::new().failing("boom");
        let err = registry_search_with(&mock, "x").await.unwrap_err();
        assert!(err.to_string().contains("registry search failed"));
    }

    #[tokio::test]
    async fn registry_get_with_installs_bare_skill_package() {
        let dir = tempfile::tempdir().unwrap();
        let managed_dir = dir.path().join("managed");
        let sqlite_path = dir.path().join("test.db");
        let mgr = zeph_skills::manager::SkillManager::new(managed_dir.clone());

        let mock = MockRegistryClient::new().with_package(
            "acme/x",
            vec![(
                "SKILL.md".to_owned(),
                "---\nname: x\ndescription: a test skill\n---\nbody".to_owned(),
            )],
        );

        registry_get_with(
            &mock,
            &mgr,
            &managed_dir,
            sqlite_path.to_str().unwrap(),
            "acme/x",
        )
        .await
        .unwrap();

        assert!(managed_dir.join("x").join("SKILL.md").is_file());
    }

    #[tokio::test]
    async fn registry_get_with_rejects_plugin_bundle_with_pointer_to_plugin_get() {
        let dir = tempfile::tempdir().unwrap();
        let managed_dir = dir.path().join("managed");
        let sqlite_path = dir.path().join("test.db");
        let mgr = zeph_skills::manager::SkillManager::new(managed_dir.clone());

        let mock = MockRegistryClient::new().with_package(
            "acme/full-plugin",
            vec![("plugin.toml".to_owned(), "[plugin]\nname=\"x\"".to_owned())],
        );

        let err = registry_get_with(
            &mock,
            &mgr,
            &managed_dir,
            sqlite_path.to_str().unwrap(),
            "acme/full-plugin",
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("zeph plugin get acme/full-plugin"));
        assert!(!managed_dir.exists() || managed_dir.read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn registry_get_with_propagates_fetch_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let managed_dir = dir.path().join("managed");
        let sqlite_path = dir.path().join("test.db");
        let mgr = zeph_skills::manager::SkillManager::new(managed_dir.clone());
        let mock = MockRegistryClient::new();

        let err = registry_get_with(
            &mock,
            &mgr,
            &managed_dir,
            sqlite_path.to_str().unwrap(),
            "missing/id",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("registry fetch failed"));
    }
}
