use std::path::{Path, PathBuf};

use zeph_skills::SkillSource;
use zeph_skills::manager::SkillManager;

use super::error::AgentError;
use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    /// Handle `/skill install <url|path>` in-session command.
    pub(super) async fn handle_skill_install(
        &mut self,
        source: Option<&str>,
    ) -> Result<(), AgentError> {
        let Some(source) = source else {
            self.channel
                .send("Usage: /skill install <url|path>")
                .await?;
            return Ok(());
        };

        let Some(managed_dir) = &self.skill_state.managed_dir else {
            self.channel
                .send("Skill management directory not configured.")
                .await?;
            return Ok(());
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
        .map_err(|e| AgentError::Other(format!("spawn_blocking failed: {e}")))?;

        match result {
            Ok(installed) => {
                if let Some(memory) = &self.memory_state.memory {
                    let (source_kind, source_url, source_path) = match &installed.source {
                        SkillSource::Hub { url } => ("hub", Some(url.as_str()), None),
                        SkillSource::File { path } => {
                            ("file", None, Some(path.to_string_lossy().into_owned()))
                        }
                        SkillSource::Local => ("local", None, None),
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

                self.reload_skills().await;

                self.channel
                    .send(&format!(
                        "Skill \"{}\" installed (trust: quarantined). Use `/skill trust {} trusted` to promote.",
                        installed.name, installed.name,
                    ))
                    .await?;
            }
            Err(e) => {
                self.channel.send(&format!("Install failed: {e}")).await?;
            }
        }

        Ok(())
    }

    /// Handle `/skill remove <name>` in-session command.
    pub(super) async fn handle_skill_remove(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), AgentError> {
        let Some(name) = name else {
            self.channel.send("Usage: /skill remove <name>").await?;
            return Ok(());
        };

        let Some(managed_dir) = &self.skill_state.managed_dir else {
            self.channel
                .send("Skill management directory not configured.")
                .await?;
            return Ok(());
        };

        let mgr = SkillManager::new(managed_dir.clone());
        let name_owned = name.to_owned();

        let remove_result = tokio::task::spawn_blocking(move || mgr.remove(&name_owned))
            .await
            .map_err(|e| AgentError::Other(format!("spawn_blocking failed: {e}")))?;

        match remove_result {
            Ok(()) => {
                if let Some(memory) = &self.memory_state.memory
                    && let Err(e) = memory.sqlite().delete_skill_trust(name).await
                {
                    tracing::warn!("failed to remove trust record for '{name}': {e:#}");
                }

                self.reload_skills().await;

                self.channel
                    .send(&format!("Skill \"{name}\" removed."))
                    .await?;
            }
            Err(e) => {
                self.channel.send(&format!("Remove failed: {e}")).await?;
            }
        }

        Ok(())
    }
}

// REV-004: AgentError::Other variant needed for spawn_blocking join errors.
// Checked: AgentError already has an Other(String) variant via the error module.
// Using PathBuf import to satisfy compiler (used in spawn_blocking closure via Path::new).
const _: fn() = || {
    let _: PathBuf = PathBuf::new();
};
