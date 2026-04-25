// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use zeph_memory::store::SourceKind;
use zeph_skills::SkillSource;
use zeph_skills::manager::SkillManager;

use super::error::AgentError;
use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    pub(super) async fn handle_skill_install_as_string(
        &mut self,
        source: Option<&str>,
    ) -> Result<String, AgentError> {
        let Some(source) = source else {
            return Ok("Usage: /skill install <url|path>".to_owned());
        };

        let Some(managed_dir) = self.skill_state.managed_dir.clone() else {
            return Ok("Skill management directory not configured.".to_owned());
        };

        let mgr = SkillManager::new(managed_dir.clone());
        let source_owned = source.to_owned();

        // REV-004: run blocking I/O (git clone / fs::copy) off the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            if source_owned.starts_with("http://")
                || source_owned.starts_with("https://")
                || source_owned.starts_with("git@")
            {
                mgr.install_from_url(&source_owned)
            } else {
                mgr.install_from_path(Path::new(&source_owned))
            }
        })
        .await
        .map_err(AgentError::SpawnBlocking)?;

        match result {
            Ok(installed) => {
                if let Some(memory) = self.memory_state.persistence.memory.clone() {
                    let (source_kind, source_url, source_path) = match &installed.source {
                        SkillSource::Hub { url } => (SourceKind::Hub, Some(url.as_str()), None),
                        SkillSource::File { path } => (
                            SourceKind::File,
                            None,
                            Some(path.to_string_lossy().into_owned()),
                        ),
                        SkillSource::Local => (SourceKind::Local, None, None),
                    };
                    if let Err(e) = memory
                        .sqlite()
                        .upsert_skill_trust(
                            &installed.name,
                            "quarantined",
                            source_kind,
                            source_url,
                            source_path.as_deref(),
                            &installed.blake3_hash,
                        )
                        .await
                    {
                        tracing::warn!("failed to record trust for '{}': {e:#}", installed.name);
                    }
                }

                // Note: reload_skills() is not called here because it calls rebuild_skill_matcher
                // which contains closures with HRTB lifetime constraints that make the future
                // non-Send. Hot-reload will pick up the new skill on the next cycle.
                tracing::info!(skill = %installed.name, "installed — hot-reload will activate it");

                // Check if installed skill requires secrets that are missing.
                let skill_md = managed_dir.join(&installed.name).join("SKILL.md");
                let missing_secrets: Vec<String> =
                    if let Ok(meta) = zeph_skills::loader::load_skill_meta(&skill_md) {
                        meta.requires_secrets
                            .iter()
                            .filter(|s| {
                                !self
                                    .skill_state
                                    .available_custom_secrets
                                    .contains_key(s.as_str())
                            })
                            .cloned()
                            .collect()
                    } else {
                        Vec::new()
                    };

                let mut msg = format!(
                    "Skill \"{}\" installed (trust: quarantined). Use `/skill trust {} trusted` to promote.",
                    installed.name, installed.name,
                );
                if !missing_secrets.is_empty() {
                    use std::fmt::Write;
                    let _ = write!(
                        msg,
                        "\n⚠ Missing secrets: {}. Run `zeph vault set ZEPH_SECRET_<NAME> <value>` for each.",
                        missing_secrets.join(", ")
                    );
                }

                Ok(msg)
            }
            Err(e) => Ok(format!("Install failed: {e}")),
        }
    }

    pub(super) async fn handle_skill_remove_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill remove <name>".to_owned());
        };

        let Some(managed_dir) = &self.skill_state.managed_dir else {
            return Ok("Skill management directory not configured.".to_owned());
        };

        let mgr = SkillManager::new(managed_dir.clone());
        let name_owned = name.to_owned();

        let remove_result = tokio::task::spawn_blocking(move || mgr.remove(&name_owned))
            .await
            .map_err(AgentError::SpawnBlocking)?;

        match remove_result {
            Ok(()) => {
                if let Some(memory) = self.memory_state.persistence.memory.clone()
                    && let Err(e) = memory.sqlite().delete_skill_trust(name).await
                {
                    tracing::warn!("failed to remove trust record for '{name}': {e:#}");
                }

                // Note: reload_skills() is not called here — same HRTB constraint as install.
                // Hot-reload will deactivate the removed skill on the next cycle.

                Ok(format!("Skill \"{name}\" removed."))
            }
            Err(e) => Ok(format!("Remove failed: {e}")),
        }
    }
}
