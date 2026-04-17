// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::cli::SkillCommand;

#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_skill_command(
    cmd: SkillCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{managed_skills_dir, resolve_config_path};
    use std::collections::HashMap;
    use zeph_skills::manager::SkillManager;

    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();

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
            };
            store
                .upsert_skill_trust(
                    &result.name,
                    "quarantined",
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
                    .map_or_else(|| "no trust record".to_owned(), |r| r.trust_level);
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
                            .set_skill_trust_level(&name, "quarantined")
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
                                .set_skill_trust_level(&r.name, "quarantined")
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

        SkillCommand::Trust { name, level } => {
            let valid = matches!(
                level.as_str(),
                "trusted" | "verified" | "quarantined" | "blocked"
            );
            if !valid {
                anyhow::bail!(
                    "invalid trust level: {level}. Use: trusted, verified, quarantined, blocked"
                );
            }

            // REV-003: re-verify hash before promoting to trusted/verified.
            if matches!(level.as_str(), "trusted" | "verified") {
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

                let updated = store
                    .set_skill_trust_level(&name, &level)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if updated {
                    println!("Trust level for \"{name}\" set to {level}.");
                } else {
                    anyhow::bail!("skill \"{name}\" not found in trust database");
                }
            } else {
                let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
                let updated = store
                    .set_skill_trust_level(&name, &level)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if updated {
                    println!("Trust level for \"{name}\" set to {level}.");
                } else {
                    anyhow::bail!("skill \"{name}\" not found in trust database");
                }
            }
        }

        SkillCommand::Block { name } => {
            let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                .await
                .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
            let updated = store
                .set_skill_trust_level(&name, "blocked")
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
                .set_skill_trust_level(&name, "quarantined")
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if updated {
                println!("Skill \"{name}\" unblocked (set to quarantined).");
            } else {
                anyhow::bail!("skill \"{name}\" not found in trust database");
            }
        }

        SkillCommand::Invoke { name, args } => {
            use std::str::FromStr;

            use zeph_common::SkillTrustLevel;
            use zeph_skills::prompt::{sanitize_skill_text, wrap_quarantined};

            let registry = zeph_skills::registry::SkillRegistry::load(&[managed_dir]);

            // Resolve persisted trust from SQLite. No trust row → Quarantined (fail-closed,
            // matches `SkillTrustLevel::default`).
            let trust = {
                let store = zeph_memory::store::SqliteStore::new(&sqlite_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
                store
                    .load_skill_trust(&name)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .and_then(|r| SkillTrustLevel::from_str(&r.trust_level).ok())
                    .unwrap_or_default()
            };

            if trust == SkillTrustLevel::Blocked {
                anyhow::bail!("skill is blocked by policy: {name}");
            }

            let raw = registry
                .get_body(&name)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .to_owned();

            let sanitized = if trust == SkillTrustLevel::Trusted {
                raw
            } else {
                sanitize_skill_text(&raw)
            };
            let body = if trust == SkillTrustLevel::Quarantined {
                wrap_quarantined(&name, &sanitized)
            } else {
                sanitized
            };

            match args {
                Some(a) => {
                    let args_safe = sanitize_skill_text(&a);
                    println!("{body}\n\n<args>\n{args_safe}\n</args>");
                }
                None => println!("{body}"),
            }
        }
    }

    Ok(())
}
