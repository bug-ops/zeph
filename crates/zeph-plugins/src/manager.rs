// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin lifecycle management: add, remove, list.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zeph_skills::bundled::bundled_skill_names;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::scanner::scan_skill_body;

use crate::PluginError;
use crate::manifest::{PluginManifest, PluginMcpServer};

/// Maximum number of entries allowed in `plugin.dependencies`.
///
/// Prevents a malicious manifest from triggering a fan-out `DoS` via recursive `enable()` calls
/// across an unbounded dependency graph.
const MAX_DEPENDENCIES: usize = 64;

/// The tighten-only config overlay safelist. Any key outside this list causes
/// [`PluginError::UnsafeOverlay`] at install time.
const CONFIG_SAFELIST: &[&str] = &[
    "tools.blocked_commands",
    "tools.allowed_commands",
    "skills.disambiguation_threshold",
];

/// Result of a successful `plugin add` operation.
#[derive(Debug)]
pub struct AddResult {
    /// Installed plugin name.
    pub name: String,
    /// Absolute path to the installed plugin root.
    ///
    /// Callers should pass each entry in `installed_skill_dirs` to
    /// [`zeph_skills::registry::SkillRegistry::register_hub_dir`] so the registry treats plugin
    /// subtrees as non-bundled regardless of any residual `.bundled` markers (S2 defense).
    pub plugin_root: PathBuf,
    /// Skill names registered from this plugin.
    pub installed_skills: Vec<String>,
    /// MCP server IDs declared by this plugin (require agent restart).
    pub mcp_server_ids: Vec<String>,
    /// Non-fatal warnings produced at install time.
    ///
    /// Currently populated when a plugin's `allowed_commands` overlay will
    /// have no effect because the host's base `tools.shell.allowed_commands`
    /// is empty (see issue #3149 — tighten-only semantics mean plugins
    /// cannot widen an empty base allowlist). Callers should surface these
    /// to the user alongside the success message (`eprintln!` on the CLI,
    /// appended to the output string on the TUI).
    pub warnings: Vec<String>,
}

/// Result of a successful `plugin remove` operation.
#[derive(Debug, Default)]
pub struct RemoveResult {
    /// Skill names unregistered.
    pub removed_skills: Vec<String>,
    /// MCP server IDs that were declared (require agent restart).
    pub removed_mcp_ids: Vec<String>,
}

/// Result of a successful `plugin disable` operation.
///
/// When `--force` is used and dependents exist, the disable proceeds and the list of
/// overridden dependents is returned so callers can surface a warning to the user.
#[derive(Debug, Default)]
pub struct DisableResult {
    /// Names of enabled plugins that depended on the disabled plugin.
    ///
    /// Non-empty only when the operation was forced past a dependency guard.
    /// Callers should warn the user that these plugins may misbehave until re-enabled.
    pub forced_over_dependents: Vec<String>,
}

/// Installed plugin metadata as returned by `plugin list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// Plugin description.
    pub description: String,
    /// Absolute path to the installed plugin root.
    pub path: PathBuf,
    /// Skill names provided by this plugin (collected at list time to avoid re-reading manifests).
    pub skill_names: Vec<String>,
    /// Whether automatic updates are enabled for this plugin.
    ///
    /// Mirrors `plugin.auto_update` from the installed manifest. Populated at list time so
    /// [`PluginManager::check_auto_updates`] can filter candidates without re-reading manifests.
    pub auto_update: bool,
}

/// Install-time source metadata persisted as `.plugin-source.toml` alongside `.plugin.toml`.
///
/// Separating this from [`crate::manifest::PluginMeta`] keeps the author-facing `plugin.toml`
/// schema clean: plugin authors never set these fields; they are written exclusively by
/// [`PluginManager::add_remote`] and read by [`PluginManager::check_auto_updates`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginSource {
    /// URL from which the plugin archive was originally downloaded.
    ///
    /// `None` for plugins installed from local paths via [`PluginManager::add`].
    pub url: Option<String>,
    /// Lowercase hex SHA-256 of the installed archive bytes.
    ///
    /// Used by [`PluginManager::check_auto_updates`] to skip reinstalls when the remote
    /// archive has not changed.
    pub sha256: Option<String>,
}

/// Outcome of a single auto-update attempt.
///
/// One `AutoUpdateResult` is returned per plugin that had `auto_update = true`
/// when [`PluginManager::check_auto_updates`] ran.
#[derive(Debug)]
pub struct AutoUpdateResult {
    /// Plugin name.
    pub name: String,
    /// Specific outcome for this plugin.
    pub status: AutoUpdateStatus,
}

/// Status of an individual auto-update attempt.
#[derive(Debug)]
pub enum AutoUpdateStatus {
    /// Plugin was successfully updated.
    Updated {
        /// Version before the update.
        old_version: String,
        /// Version after the update.
        new_version: String,
    },
    /// Remote archive SHA-256 matches the installed copy — no action taken.
    UpToDate,
    /// Plugin has no persisted source URL (installed from a local path).
    NoSource,
    /// Update failed; plugin remains at its current version.
    Failed(String),
}

/// Manages plugin lifecycle: install, remove, list.
///
/// All operations are synchronous. Plugin watchers and agent config overlays are
/// applied separately by the agent bootstrap layer.
pub struct PluginManager {
    /// Root directory where plugins are installed (`~/.local/share/zeph/plugins/`).
    plugins_dir: PathBuf,
    /// Directory where managed (user-installed) skills live.
    managed_skills_dir: PathBuf,
    /// `mcp.allowed_commands` from the agent config. Used to validate plugin MCP entries.
    mcp_allowed_commands: Vec<String>,
    /// Host's base `tools.shell.allowed_commands`. Used to warn when a
    /// plugin overlay will be silently dropped because the base is empty
    /// (see issue #3149).
    base_allowed_commands: Vec<String>,
    /// Path to the integrity registry file. Injected so tests can use isolated paths.
    integrity_registry_path: PathBuf,
    /// Timeout in seconds for each HTTP phase of [`Self::add_remote`] (connect + body read).
    download_timeout_secs: u64,
}

impl PluginManager {
    /// Returns the canonical default plugins directory: `~/.local/share/zeph/plugins/`.
    ///
    /// Both the CLI and TUI must use this helper so they always point to the same directory.
    #[must_use]
    pub fn default_plugins_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("zeph")
            .join("plugins")
    }

    /// Create a new manager.
    ///
    /// # Parameters
    ///
    /// - `plugins_dir` — root installation directory for plugins.
    /// - `managed_skills_dir` — directory for user-managed skills (conflict detection).
    /// - `mcp_allowed_commands` — allowlist for MCP server commands from agent config.
    /// - `base_allowed_commands` — host's `tools.shell.allowed_commands`.
    ///   Used to emit a non-fatal warning when a plugin overlay would be
    ///   silently dropped at load time (tighten-only invariant).
    #[must_use]
    pub fn new(
        plugins_dir: PathBuf,
        managed_skills_dir: PathBuf,
        mcp_allowed_commands: Vec<String>,
        base_allowed_commands: Vec<String>,
    ) -> Self {
        let integrity_registry_path = crate::integrity::IntegrityRegistry::default_path();
        Self {
            plugins_dir,
            managed_skills_dir,
            mcp_allowed_commands,
            base_allowed_commands,
            integrity_registry_path,
            download_timeout_secs: 30,
        }
    }

    /// Override the HTTP download timeout used by [`Self::add_remote`].
    ///
    /// Each phase (connect and body read) is independently bounded by this value.
    /// The default is 30 seconds.
    #[must_use]
    pub fn with_download_timeout_secs(mut self, secs: u64) -> Self {
        self.download_timeout_secs = secs;
        self
    }

    /// Override the integrity registry path. Intended for tests only.
    #[cfg(test)]
    #[must_use]
    pub fn with_integrity_registry_path(mut self, path: PathBuf) -> Self {
        self.integrity_registry_path = path;
        self
    }

    /// Install a plugin from a local directory path.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the manifest is invalid, the source cannot be read,
    /// there are skill name conflicts, MCP commands are not allowlisted, or config
    /// overlay keys are not in the tighten-only safelist.
    pub fn add(&self, source: &str) -> Result<AddResult, PluginError> {
        let _span = tracing::info_span!("plugins.manager.add", plugin.source = %source).entered();
        let source_path = PathBuf::from(source);
        if !source_path.exists() {
            return Err(PluginError::InvalidSource {
                path: source.to_owned(),
                reason: "path does not exist".to_owned(),
            });
        }

        let manifest_path = source_path.join("plugin.toml");
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| PluginError::Io {
            path: manifest_path.clone(),
            source: e,
        })?;
        let manifest_str = String::from_utf8(manifest_bytes).map_err(|_| {
            PluginError::InvalidManifest("plugin.toml is not valid UTF-8".to_owned())
        })?;
        let manifest: PluginManifest = toml::from_str(&manifest_str)
            .map_err(|e| PluginError::InvalidManifest(format!("{e}")))?;

        // Validate plugin name.
        validate_plugin_name(&manifest.plugin.name)?;

        // Validate dependency list: enforce count limit and name format.
        if manifest.plugin.dependencies.len() > MAX_DEPENDENCIES {
            return Err(PluginError::InvalidManifest(format!(
                "plugin declares {} dependencies; maximum allowed is {MAX_DEPENDENCIES}",
                manifest.plugin.dependencies.len()
            )));
        }
        for dep in &manifest.plugin.dependencies {
            validate_plugin_name(dep)?;
        }

        // Validate each [[skills]] entry: path must stay within source root and SKILL.md must exist.
        for entry in &manifest.skills {
            let skill_path = source_path.join(&entry.path);
            // Reject path traversal: resolved path must be inside source_path.
            let canonical_source = source_path.canonicalize().map_err(|e| PluginError::Io {
                path: source_path.clone(),
                source: e,
            })?;
            let canonical_skill = skill_path.canonicalize().map_err(|e| PluginError::Io {
                path: skill_path.clone(),
                source: e,
            })?;
            if !canonical_skill.starts_with(&canonical_source) {
                return Err(PluginError::InvalidSource {
                    path: entry.path.clone(),
                    reason: "skill path escapes plugin source root".to_owned(),
                });
            }
            // Ensure the skill directory contains a SKILL.md file.
            if !skill_path.join("SKILL.md").is_file() {
                return Err(PluginError::SkillEntryMissing { path: skill_path });
            }
        }

        // Validate config overlay keys.
        validate_overlay_keys(&manifest.config)?;

        // Stage-1: advisory regex scan over each SKILL.md before copying files.
        // Results are warnings only — they never block installation.
        scan_skill_entries(
            source_path.as_path(),
            &manifest.skills,
            &manifest.plugin.name,
        );

        let mut warnings: Vec<String> = Vec::new();
        if let Some(msg) = check_allowed_commands_overlay_effect(
            &manifest.config,
            &self.base_allowed_commands,
            &manifest.plugin.name,
        ) {
            tracing::warn!(plugin = %manifest.plugin.name, "{msg}");
            warnings.push(msg);
        }

        // Validate MCP command allowlist.
        validate_mcp_commands(&manifest.mcp.servers, &self.mcp_allowed_commands)?;

        // Collect skill names from the plugin source.
        let skill_names = collect_skill_names(&source_path, &manifest);

        // Check for name conflicts.
        self.check_skill_conflicts(&skill_names, &manifest.plugin.name)?;

        let dest = self.plugins_dir.join(&manifest.plugin.name);

        // Copy source to destination.
        copy_dir_all(&source_path, &dest)?;

        // Recursively strip all .bundled markers from the installed tree.
        strip_bundled_markers(&dest);

        // Write manifest copy at plugin root for future reference.
        let installed_manifest_path = dest.join(".plugin.toml");
        let manifest_str = toml::to_string(&manifest)?;
        std::fs::write(&installed_manifest_path, &manifest_str).map_err(|e| PluginError::Io {
            path: installed_manifest_path.clone(),
            source: e,
        })?;

        // Record integrity digest. Crash between the write above and the save here leaves the
        // plugin with no registry entry — it will load unverified until reinstalled (M4).
        let mut registry = crate::integrity::IntegrityRegistry::load(&self.integrity_registry_path);
        if let Err(e) = registry
            .record(&manifest.plugin.name, &installed_manifest_path)
            .and_then(|()| registry.save(&self.integrity_registry_path))
        {
            tracing::warn!(plugin = %manifest.plugin.name, error = %e, "failed to update integrity registry after install");
        }

        let mcp_server_ids: Vec<String> =
            manifest.mcp.servers.iter().map(|s| s.id.clone()).collect();

        tracing::info!(
            plugin = %manifest.plugin.name,
            skills = ?skill_names,
            mcp_servers = ?mcp_server_ids,
            "plugin installed"
        );

        Ok(AddResult {
            name: manifest.plugin.name,
            plugin_root: dest,
            installed_skills: skill_names,
            mcp_server_ids,
            warnings,
        })
    }

    /// Download and install a plugin from a remote URL with optional SHA-256 integrity pinning.
    ///
    /// Downloads the archive at `url`, verifies its SHA-256 digest against `expected_sha256`
    /// (when provided), extracts it to a temporary directory, and delegates to [`Self::add`].
    ///
    /// # Integrity check
    ///
    /// When `expected_sha256` is `Some`, the raw archive bytes are hashed with SHA-256 and
    /// compared against the expected lowercase hex string. If the digests do not match,
    /// [`PluginError::IntegrityCheckFailed`] is returned and the archive is never extracted.
    ///
    /// When `expected_sha256` is `None`, the archive is extracted without verification. Callers
    /// are encouraged to always supply the expected hash; unverified installs are permitted by
    /// default for backward compatibility but should be avoided in production.
    ///
    /// # Errors
    ///
    /// - [`PluginError::DownloadFailed`] — HTTP request failed or returned a non-2xx status.
    /// - [`PluginError::IntegrityCheckFailed`] — SHA-256 digest mismatch.
    /// - [`PluginError::InvalidSource`] — archive cannot be extracted.
    /// - Any error that [`Self::add`] can return.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use zeph_plugins::PluginManager;
    /// # async fn example() -> Result<(), zeph_plugins::PluginError> {
    /// let mgr = PluginManager::new(
    ///     "/tmp/plugins".into(),
    ///     "/tmp/managed".into(),
    ///     vec![],
    ///     vec![],
    /// );
    /// let result = mgr.add_remote(
    ///     "https://example.com/my-plugin.tar.gz",
    ///     Some("abc123def456..."),
    /// ).await?;
    /// println!("installed: {}", result.name);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_remote(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
    ) -> Result<AddResult, PluginError> {
        let span = tracing::info_span!("plugins.manager.add_remote", %url);
        let _guard = span.enter();

        // Reject non-HTTP(S) schemes to prevent SSRF via file:// or other transports.
        validate_url_scheme(url)?;

        let timeout = std::time::Duration::from_secs(self.download_timeout_secs);

        let response = tokio::time::timeout(timeout, reqwest::get(url))
            .await
            .map_err(|_| PluginError::DownloadFailed {
                url: url.to_owned(),
                reason: format!("download timed out after {}s", self.download_timeout_secs),
            })?
            .map_err(|e| PluginError::DownloadFailed {
                url: url.to_owned(),
                reason: e.to_string(),
            })?;

        if !response.status().is_success() {
            return Err(PluginError::DownloadFailed {
                url: url.to_owned(),
                reason: format!("HTTP {}", response.status()),
            });
        }

        let bytes = tokio::time::timeout(timeout, response.bytes())
            .await
            .map_err(|_| PluginError::DownloadFailed {
                url: url.to_owned(),
                reason: format!("download timed out after {}s", self.download_timeout_secs),
            })?
            .map_err(|e| PluginError::DownloadFailed {
                url: url.to_owned(),
                reason: format!("failed to read response body: {e}"),
            })?;

        // Verify SHA-256 before extracting anything.
        if let Some(expected) = expected_sha256 {
            let actual = crate::integrity::sha256_hex(&bytes);
            if actual != expected.to_ascii_lowercase() {
                return Err(PluginError::IntegrityCheckFailed {
                    expected: expected.to_ascii_lowercase(),
                    actual,
                });
            }
            tracing::debug!(url, "archive SHA-256 verified");
        } else {
            tracing::warn!(url, "installing remote plugin without integrity check");
        }

        // Compute the actual SHA-256 for source persistence (even when expected_sha256 is None).
        let actual_sha256 = crate::integrity::sha256_hex(&bytes);

        // Extract archive to a temporary directory and delegate to `add`.
        let tmp = tempfile::tempdir().map_err(|e| PluginError::Io {
            path: std::path::PathBuf::from(url),
            source: e,
        })?;
        extract_archive(&bytes, tmp.path(), url)?;

        let plugins_dir = self.plugins_dir.clone();
        let managed_skills_dir = self.managed_skills_dir.clone();
        let mcp_allowed_commands = self.mcp_allowed_commands.clone();
        let base_allowed_commands = self.base_allowed_commands.clone();
        let integrity_registry_path = self.integrity_registry_path.clone();
        let source_str = tmp.path().to_str().unwrap_or(url).to_owned();

        let result = tokio::task::spawn_blocking(move || {
            let mgr = PluginManager {
                plugins_dir,
                managed_skills_dir,
                mcp_allowed_commands,
                base_allowed_commands,
                integrity_registry_path,
                download_timeout_secs: 0, // add() does not perform network I/O
            };
            mgr.add(&source_str)
        })
        .await
        .map_err(|e| PluginError::Io {
            path: std::path::PathBuf::from(url),
            source: std::io::Error::other(e),
        })??;

        // Persist source metadata so check_auto_updates can re-fetch this plugin.
        let source = PluginSource {
            url: Some(url.to_owned()),
            sha256: Some(actual_sha256),
        };
        let source_path = self
            .plugins_dir
            .join(&result.name)
            .join(".plugin-source.toml");
        match toml::to_string(&source) {
            Ok(toml_str) => {
                if let Err(e) = std::fs::write(&source_path, toml_str) {
                    tracing::warn!(
                        plugin = %result.name,
                        error = %e,
                        "failed to persist plugin source metadata; auto_update will be skipped"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = %result.name,
                    error = %e,
                    "failed to serialize plugin source metadata; auto_update will be skipped"
                );
            }
        }

        Ok(result)
    }

    /// Remove an installed plugin by name.
    ///
    /// Refuses to remove the plugin if any enabled plugin depends on it. The caller receives a
    /// [`PluginError::DependencyRequired`] error with a formatted hint listing the dependents.
    ///
    /// # Note on TOCTOU
    ///
    /// The dependent check and directory removal are not atomic. In a multi-process environment a
    /// concurrent enable of a dependent could race with this remove. This is acceptable for a
    /// single-user CLI tool where plugin operations are manual.
    ///
    /// # Errors
    ///
    /// - [`PluginError::NotFound`] — plugin is not installed.
    /// - [`PluginError::DependencyRequired`] — at least one enabled plugin depends on this one.
    /// - [`PluginError::Io`] — the plugin directory cannot be removed.
    pub fn remove(&self, name: &str) -> Result<RemoveResult, PluginError> {
        validate_plugin_name(name)?;
        let plugin_dir = self.plugins_dir.join(name);
        if !plugin_dir.exists() {
            return Err(PluginError::NotFound {
                name: name.to_owned(),
            });
        }

        self.guard_no_dependents(name)?;

        let manifest_path = plugin_dir.join(".plugin.toml");
        let (removed_skills, removed_mcp_ids) = if manifest_path.exists() {
            let bytes = std::fs::read(&manifest_path).map_err(|e| PluginError::Io {
                path: manifest_path,
                source: e,
            })?;
            let text = String::from_utf8(bytes).map_err(|_| {
                PluginError::InvalidManifest(".plugin.toml is not valid UTF-8".to_owned())
            })?;
            let manifest: PluginManifest =
                toml::from_str(&text).map_err(|e| PluginError::InvalidManifest(format!("{e}")))?;
            let skills = collect_skill_names(&plugin_dir, &manifest);
            let mcp = manifest.mcp.servers.iter().map(|s| s.id.clone()).collect();
            (skills, mcp)
        } else {
            (Vec::new(), Vec::new())
        };

        std::fs::remove_dir_all(&plugin_dir).map_err(|e| PluginError::Io {
            path: plugin_dir,
            source: e,
        })?;

        // Remove integrity entry; non-fatal if registry cannot be updated.
        let mut registry = crate::integrity::IntegrityRegistry::load(&self.integrity_registry_path);
        registry.remove(name);
        if let Err(e) = registry.save(&self.integrity_registry_path) {
            tracing::warn!(plugin = %name, error = %e, "failed to update integrity registry after remove");
        }

        tracing::info!(plugin = %name, "plugin removed");

        Ok(RemoveResult {
            removed_skills,
            removed_mcp_ids,
        })
    }

    /// List all installed plugins.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the plugins directory cannot be read.
    pub fn list_installed(&self) -> Result<Vec<InstalledPlugin>, PluginError> {
        if !self.plugins_dir.exists() {
            return Ok(Vec::new());
        }

        let mut plugins = Vec::new();
        let entries = std::fs::read_dir(&self.plugins_dir).map_err(|e| PluginError::Io {
            path: self.plugins_dir.clone(),
            source: e,
        })?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join(".plugin.toml");
            if !manifest_path.exists() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&manifest_path) else {
                continue;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let Ok(manifest): Result<PluginManifest, _> = toml::from_str(&text) else {
                continue;
            };
            let skill_names = collect_skill_names(&path, &manifest);
            let auto_update = manifest.plugin.auto_update;
            plugins.push(InstalledPlugin {
                name: manifest.plugin.name,
                version: manifest.plugin.version,
                description: manifest.plugin.description,
                path,
                skill_names,
                auto_update,
            });
        }

        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(plugins)
    }

    /// Returns all skill directory paths from installed plugins.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the plugins directory cannot be read.
    #[tracing::instrument(name = "plugins.manager.collect_skill_dirs", skip_all)]
    pub fn collect_skill_dirs(&self) -> Result<Vec<PathBuf>, PluginError> {
        if !self.plugins_dir.exists() {
            return Ok(Vec::new());
        }

        let mut dirs = Vec::new();
        let plugins = self.list_installed()?;
        for plugin in &plugins {
            // Skip disabled plugins — their skills must not be loaded.
            if plugin.path.join(".disabled").exists() {
                continue;
            }
            let manifest_path = plugin.path.join(".plugin.toml");
            if let Ok(bytes) = std::fs::read(&manifest_path)
                && let Ok(text) = String::from_utf8(bytes)
                && let Ok(manifest) = toml::from_str::<PluginManifest>(&text)
            {
                for entry in &manifest.skills {
                    let skill_dir = plugin.path.join(&entry.path);
                    // Reject traversal: dir must stay within the installed plugin root.
                    let ok = skill_dir
                        .canonicalize()
                        .is_ok_and(|c| c.starts_with(&plugin.path));
                    if ok {
                        dirs.push(skill_dir);
                    } else {
                        tracing::warn!(
                            plugin = %plugin.name,
                            path = %entry.path,
                            "skipping skill path that escapes plugin root"
                        );
                    }
                }
            }
        }
        Ok(dirs)
    }

    /// Check and apply updates for all installed plugins with `auto_update = true`.
    ///
    /// For each eligible plugin the method:
    /// 1. Reads `.plugin-source.toml` to retrieve the original download URL and SHA-256.
    /// 2. Downloads the archive from that URL.
    /// 3. Compares the downloaded archive's SHA-256 with the stored value.
    /// 4. If the hashes differ, stages the new version in a temporary directory adjacent to the
    ///    plugin root, then atomically swaps via `rename` — an interrupted update leaves the
    ///    plugin intact rather than half-deleted.
    /// 5. Returns a result per plugin; failures are warnings and never abort startup.
    ///
    /// Plugins installed from local paths (no `.plugin-source.toml` or no URL) are skipped
    /// with [`AutoUpdateStatus::NoSource`].
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use zeph_plugins::PluginManager;
    /// # async fn run() {
    /// let mgr = PluginManager::new(
    ///     "/tmp/plugins".into(),
    ///     "/tmp/managed".into(),
    ///     vec![],
    ///     vec![],
    /// );
    /// let results = mgr.check_auto_updates().await;
    /// for r in &results {
    ///     println!("{}: {:?}", r.name, r.status);
    /// }
    /// # }
    /// ```
    pub async fn check_auto_updates(&self) -> Vec<AutoUpdateResult> {
        use futures::stream::{self, StreamExt as _};
        use tracing::Instrument as _;

        async {
            let candidates = match self.list_installed() {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(error = %e, "check_auto_updates: failed to list installed plugins");
                    return Vec::new();
                }
            };

            stream::iter(candidates.into_iter().filter(|p| p.auto_update))
                .map(|plugin| async move {
                    let status = self.update_one_plugin(&plugin).await;
                    AutoUpdateResult {
                        name: plugin.name,
                        status,
                    }
                })
                .buffer_unordered(4)
                .collect()
                .await
        }
        .instrument(tracing::info_span!("plugins.manager.check_auto_updates"))
        .await
    }

    /// Attempt to update a single plugin. Returns the update status without aborting on error.
    #[tracing::instrument(name = "plugins.manager.update_one", skip_all, fields(plugin = %plugin.name))]
    async fn update_one_plugin(&self, plugin: &InstalledPlugin) -> AutoUpdateStatus {
        let source_path = plugin.path.join(".plugin-source.toml");
        let Some(source) = read_plugin_source(&source_path).await else {
            return AutoUpdateStatus::NoSource;
        };

        let (Some(url), Some(stored_sha256)) = (source.url, source.sha256) else {
            return AutoUpdateStatus::NoSource;
        };

        // Reject non-HTTP(S) schemes to prevent SSRF via file:// or other transports.
        if let Err(e) = validate_url_scheme(&url) {
            tracing::warn!(plugin = %plugin.name, %url, error = %e, "auto-update: invalid URL scheme");
            return AutoUpdateStatus::Failed(format!("invalid URL scheme: {e}"));
        }

        tracing::debug!(plugin = %plugin.name, %url, "checking for updates");

        let timeout = std::time::Duration::from_secs(self.download_timeout_secs);

        let bytes = match self.download_archive(&url, timeout).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(plugin = %plugin.name, %url, error = %e, "auto-update download failed");
                return AutoUpdateStatus::Failed(e);
            }
        };

        let new_sha256 = crate::integrity::sha256_hex(&bytes);
        if new_sha256 == stored_sha256 {
            tracing::debug!(plugin = %plugin.name, "auto-update: archive unchanged (SHA-256 match)");
            return AutoUpdateStatus::UpToDate;
        }

        tracing::info!(
            plugin = %plugin.name,
            old_sha256 = %stored_sha256,
            new_sha256 = %new_sha256,
            "auto-update: new archive detected, applying update"
        );

        let old_version = plugin.version.clone();

        // Extract to a staging directory adjacent to the plugin root.
        let staging = self.plugins_dir.join(format!(".staging-{}", plugin.name));
        let backup = self.plugins_dir.join(format!(".backup-{}", plugin.name));
        let dest = plugin.path.clone();
        let plugin_name = plugin.name.clone();

        // Offload all blocking filesystem operations to a dedicated thread.
        let mcp_allowed = self.mcp_allowed_commands.clone();
        let managed_skills_dir = self.managed_skills_dir.clone();
        let plugins_dir = self.plugins_dir.clone();
        let integrity_registry_path = self.integrity_registry_path.clone();
        let url_clone = url.clone();
        let base_allowed_commands = self.base_allowed_commands.clone();

        let result = tokio::task::spawn_blocking(move || {
            apply_staged_update(
                &bytes,
                &url_clone,
                &dest,
                &staging,
                &backup,
                &plugin_name,
                &mcp_allowed,
                &managed_skills_dir,
                &plugins_dir,
                &integrity_registry_path,
                &base_allowed_commands,
            )
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(plugin = %plugin.name, error = %e, "auto-update: staged swap failed, original preserved");
                return AutoUpdateStatus::Failed(e);
            }
            Err(e) => {
                tracing::warn!(plugin = %plugin.name, error = %e, "auto-update: blocking task panicked");
                return AutoUpdateStatus::Failed(format!("update task panicked: {e}"));
            }
        }

        // Persist updated source metadata (new SHA-256).
        let new_source = PluginSource {
            url: Some(url),
            sha256: Some(new_sha256),
        };
        let source_dest = plugin.path.join(".plugin-source.toml");
        if let Ok(toml_str) = toml::to_string(&new_source) {
            let _ = tokio::fs::write(&source_dest, toml_str).await;
        }

        // Read the new version from the updated manifest.
        let new_version = tokio::fs::read_to_string(plugin.path.join(".plugin.toml"))
            .await
            .ok()
            .and_then(|s| toml::from_str::<crate::manifest::PluginManifest>(&s).ok())
            .map_or_else(|| old_version.clone(), |m| m.plugin.version);

        tracing::info!(
            plugin = %plugin.name,
            %old_version,
            %new_version,
            "auto-update: plugin updated successfully"
        );

        AutoUpdateStatus::Updated {
            old_version,
            new_version,
        }
    }

    /// Download an archive from `url` respecting the per-phase timeout.
    #[tracing::instrument(name = "plugins.manager.download_archive", skip_all, fields(url = %url))]
    async fn download_archive(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<Vec<u8>, String> {
        let response = tokio::time::timeout(timeout, reqwest::get(url))
            .await
            .map_err(|_| format!("download timed out after {}s", timeout.as_secs()))?
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let raw = tokio::time::timeout(timeout, response.bytes())
            .await
            .map_err(|_| format!("body read timed out after {}s", timeout.as_secs()))?
            .map_err(|e| format!("failed to read body: {e}"))?;

        Ok(raw.to_vec())
    }

    /// Enable an installed plugin by removing its `.disabled` marker file.
    ///
    /// Before enabling the target, all plugins listed in `plugin.dependencies` are enabled
    /// recursively (depth-first). The method detects dependency cycles and returns
    /// [`PluginError::DependencyCycle`] before touching the filesystem.
    ///
    /// A plugin with no `.disabled` marker is considered already enabled; this method is a no-op
    /// for such plugins (idempotent).
    ///
    /// # Errors
    ///
    /// - [`PluginError::NotFound`] — plugin is not installed.
    /// - [`PluginError::MissingDependency`] — a declared dependency is not installed.
    /// - [`PluginError::DependencyCycle`] — the dependency graph contains a cycle.
    /// - [`PluginError::Io`] — the `.disabled` marker cannot be removed.
    pub fn enable(&self, name: &str) -> Result<(), PluginError> {
        validate_plugin_name(name)?;
        let mut visiting: Vec<String> = Vec::new();
        self.enable_recursive(name, &mut visiting)
    }

    /// Recursive implementation of [`Self::enable`]; `visiting` tracks the DFS path for cycle
    /// detection.
    fn enable_recursive(&self, name: &str, visiting: &mut Vec<String>) -> Result<(), PluginError> {
        if visiting.iter().any(|v| v == name) {
            // Build a readable cycle description: A → B → A
            let mut path = visiting.clone();
            path.push(name.to_owned());
            return Err(PluginError::DependencyCycle {
                name: name.to_owned(),
                cycle: path.join(" → "),
            });
        }

        let plugin_dir = self.plugins_dir.join(name);
        if !plugin_dir.exists() {
            return Err(PluginError::NotFound {
                name: name.to_owned(),
            });
        }

        // Already enabled — nothing to do.
        let disabled_marker = plugin_dir.join(".disabled");
        if !disabled_marker.exists() {
            return Ok(());
        }

        // Load manifest to discover dependencies.
        let manifest = load_installed_manifest(&plugin_dir)?;

        visiting.push(name.to_owned());
        for dep in &manifest.plugin.dependencies {
            let dep_dir = self.plugins_dir.join(dep);
            if !dep_dir.exists() {
                visiting.pop();
                return Err(PluginError::MissingDependency {
                    name: name.to_owned(),
                    dependency: dep.clone(),
                });
            }
            self.enable_recursive(dep, visiting)?;
        }
        visiting.pop();

        // When re-enabling a plugin as a transitive dependency (visiting is non-empty), warn so
        // the operator knows that something they explicitly disabled was brought back.
        if let Some(requested_by) = visiting.last() {
            tracing::warn!(
                plugin = %name,
                requested_by = %requested_by,
                "auto-enabling previously-disabled plugin as transitive dependency"
            );
        }

        // Remove the `.disabled` marker to enable this plugin.
        std::fs::remove_file(&disabled_marker).map_err(|e| PluginError::Io {
            path: disabled_marker.clone(),
            source: e,
        })?;

        tracing::info!(plugin = %name, "plugin enabled");
        Ok(())
    }

    /// Disable an installed plugin by creating a `.disabled` marker file.
    ///
    /// Refuses to disable the plugin if any *enabled* plugin depends on it, unless `force` is
    /// `true`. When `force` is `true` the operation proceeds regardless, and the returned
    /// [`DisableResult`] lists the dependents that were overridden so callers can warn the user.
    ///
    /// Disabling an already-disabled plugin is a no-op (idempotent).
    ///
    /// # Note on TOCTOU
    ///
    /// The dependent check and marker creation are not atomic. In a multi-process
    /// environment a concurrent enable of a dependent could race with this disable. This is
    /// acceptable for a single-user CLI tool where plugin operations are manual.
    ///
    /// # Errors
    ///
    /// - [`PluginError::NotFound`] — plugin is not installed.
    /// - [`PluginError::DependencyRequired`] — at least one enabled plugin depends on this one
    ///   and `force` is `false`.
    /// - [`PluginError::Io`] — the `.disabled` marker cannot be written.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use zeph_plugins::PluginManager;
    /// # fn main() -> Result<(), zeph_plugins::PluginError> {
    /// let mgr = PluginManager::new(
    ///     "/tmp/plugins".into(),
    ///     "/tmp/managed".into(),
    ///     vec![],
    ///     vec![],
    /// );
    /// // Normal disable — fails if any enabled plugin depends on "my-plugin".
    /// mgr.disable("my-plugin", false)?;
    ///
    /// // Forced disable — proceeds even if dependents exist.
    /// let result = mgr.disable("my-plugin", true)?;
    /// if !result.forced_over_dependents.is_empty() {
    ///     eprintln!("Warning: disabled despite dependents: {:?}", result.forced_over_dependents);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn disable(&self, name: &str, force: bool) -> Result<DisableResult, PluginError> {
        validate_plugin_name(name)?;
        let plugin_dir = self.plugins_dir.join(name);
        if !plugin_dir.exists() {
            return Err(PluginError::NotFound {
                name: name.to_owned(),
            });
        }

        let forced_over_dependents = if force {
            let dependents = self.dependents_of(name);
            if !dependents.is_empty() {
                tracing::warn!(
                    plugin = %name,
                    dependents = ?dependents,
                    "force-disabling plugin that has enabled dependents"
                );
            }
            dependents
        } else {
            self.guard_no_dependents(name)?;
            Vec::new()
        };

        // Already disabled — nothing to do.
        let disabled_marker = plugin_dir.join(".disabled");
        if disabled_marker.exists() {
            return Ok(DisableResult {
                forced_over_dependents,
            });
        }

        std::fs::write(&disabled_marker, b"").map_err(|e| PluginError::Io {
            path: disabled_marker.clone(),
            source: e,
        })?;

        tracing::info!(plugin = %name, force, "plugin disabled");
        Ok(DisableResult {
            forced_over_dependents,
        })
    }

    /// Returns the names of all **enabled** plugins that declare `name` as a dependency.
    ///
    /// Scans every installed plugin's manifest; a plugin is considered enabled if it has no
    /// `.disabled` marker file in its directory. The check is O(N) in the number of installed
    /// plugins and performs one filesystem read per plugin. For typical plugin counts (<50) this
    /// is negligible.
    fn dependents_of(&self, name: &str) -> Vec<String> {
        if !self.plugins_dir.exists() {
            return Vec::new();
        }

        let Ok(entries) = std::fs::read_dir(&self.plugins_dir) else {
            return Vec::new();
        };

        let mut dependents = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Skip disabled plugins.
            if path.join(".disabled").exists() {
                continue;
            }
            let Ok(manifest) = load_installed_manifest(&path) else {
                continue;
            };
            if manifest.plugin.name == name {
                continue;
            }
            if manifest.plugin.dependencies.iter().any(|d| d == name) {
                dependents.push(manifest.plugin.name);
            }
        }
        dependents.sort();
        dependents
    }

    /// Check that no enabled plugin depends on `name`; return [`PluginError::DependencyRequired`]
    /// if any do.
    fn guard_no_dependents(&self, name: &str) -> Result<(), PluginError> {
        let dependents = self.dependents_of(name);
        if dependents.is_empty() {
            return Ok(());
        }
        let hints = dependents
            .iter()
            .map(|d| format!("  zeph plugin disable {d}"))
            .collect::<Vec<_>>()
            .join("\n");
        Err(PluginError::DependencyRequired {
            name: name.to_owned(),
            dependents: dependents.join(", "),
            hints,
        })
    }

    /// Like [`Self::check_skill_conflicts`] but skips the plugin currently being updated.
    ///
    /// Used by the auto-update path to validate the staged replacement without false conflicts
    /// with the plugin's own currently-installed skills (which are about to be replaced).
    pub(crate) fn check_skill_conflicts_for_update(
        &self,
        skill_names: &[String],
        this_plugin: &str,
    ) -> Result<(), PluginError> {
        self.check_skill_conflicts(skill_names, this_plugin)
    }

    fn check_skill_conflicts(
        &self,
        skill_names: &[String],
        this_plugin: &str,
    ) -> Result<(), PluginError> {
        let bundled = bundled_skill_names();

        // Managed skills: any name in the managed skills dir.
        let managed_registry = {
            let dirs: Vec<PathBuf> = if self.managed_skills_dir.exists() {
                vec![self.managed_skills_dir.clone()]
            } else {
                vec![]
            };
            SkillRegistry::load(&dirs)
        };
        let managed_names: std::collections::HashSet<String> = managed_registry
            .all_meta()
            .iter()
            .map(|m| m.name.clone())
            .collect();

        // Other installed plugins' skill names — already collected by list_installed.
        let installed = self.list_installed().unwrap_or_default();
        let mut other_plugin_skills: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for plugin in &installed {
            if plugin.name == this_plugin {
                continue;
            }
            for name in &plugin.skill_names {
                other_plugin_skills.insert(name.clone(), plugin.name.clone());
            }
        }

        for name in skill_names {
            if bundled.contains(name) {
                return Err(PluginError::SkillNameConflictWithBundled { name: name.clone() });
            }
            if managed_names.contains(name) {
                return Err(PluginError::SkillNameConflictWithManaged { name: name.clone() });
            }
            if let Some(other) = other_plugin_skills.get(name) {
                return Err(PluginError::SkillNameConflictWithPlugin {
                    name: name.clone(),
                    plugin: other.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Read `.plugin-source.toml` from `path` and return it, or `None` on any failure.
///
/// Failures are logged as debug — missing sidecar is normal for local-install plugins.
async fn read_plugin_source(path: &std::path::Path) -> Option<PluginSource> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    match toml::from_str::<PluginSource>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "cannot parse .plugin-source.toml");
            None
        }
    }
}

/// Validate that `url` uses an `http` or `https` scheme.
///
/// Rejects `file://`, `data:`, and any other scheme that could be used for SSRF
/// or local filesystem exfiltration when fetching plugin archives.
///
/// # Errors
///
/// Returns [`PluginError::InvalidSource`] when the URL is unparseable or uses a
/// disallowed scheme.
pub(crate) fn validate_url_scheme(url: &str) -> Result<(), PluginError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| PluginError::InvalidSource {
        path: url.to_owned(),
        reason: "URL is not valid".to_owned(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(PluginError::InvalidSource {
            path: url.to_owned(),
            reason: format!(
                "URL scheme {:?} is not allowed; only http and https are permitted",
                parsed.scheme()
            ),
        });
    }
    Ok(())
}

/// Extract archive to `staging`, run all security validations, then swap `staging` with `dest`.
///
/// Strategy: extract → staging, validate, rename dest → backup, rename staging → dest, delete
/// backup. If any step after the extract fails, staging is removed and backup (if present) is
/// restored so the installed plugin is never left in a partial state.
///
/// Note: `rename(2)` is atomic only within the same filesystem. Across different mounts this
/// returns `EXDEV`; the backup is restored in that case. For most deployments `plugins_dir` and
/// the temp path share the same mount, so `EXDEV` is unlikely in practice.
///
/// # Errors
///
/// Returns a human-readable error string on any failure. The caller logs this as a warning.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_staged_update(
    bytes: &[u8],
    url: &str,
    dest: &std::path::Path,
    staging: &std::path::Path,
    backup: &std::path::Path,
    installed_plugin_name: &str,
    mcp_allowed_commands: &[String],
    managed_skills_dir: &std::path::Path,
    plugins_dir: &std::path::Path,
    integrity_registry_path: &std::path::Path,
    base_allowed_commands: &[String],
) -> Result<(), String> {
    // Clean up any leftover staging/backup dirs from a previous interrupted attempt.
    let _ = std::fs::remove_dir_all(staging);
    let _ = std::fs::remove_dir_all(backup);

    std::fs::create_dir_all(staging).map_err(|e| format!("failed to create staging dir: {e}"))?;

    // Extract archive into staging directory (with tar-slip protection).
    extract_archive_safe(bytes, staging, url).map_err(|e| e.to_string())?;

    // Parse and validate the staged manifest before touching the installed plugin.
    let staging_manifest = staging.join("plugin.toml");
    if !staging_manifest.exists() {
        let _ = std::fs::remove_dir_all(staging);
        return Err("extracted archive does not contain plugin.toml".into());
    }
    let manifest_str = std::fs::read_to_string(&staging_manifest)
        .map_err(|e| format!("cannot read staged plugin.toml: {e}"))?;
    let manifest: crate::manifest::PluginManifest =
        toml::from_str(&manifest_str).map_err(|e| format!("staged plugin.toml invalid: {e}"))?;

    // Reject name changes: an update must not rename the plugin.
    if let Err(e) = validate_plugin_name(&manifest.plugin.name) {
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!("staged manifest has invalid plugin name: {e}"));
    }
    if manifest.plugin.name != installed_plugin_name {
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!(
            "staged manifest changes plugin name from {:?} to {:?}; update rejected",
            installed_plugin_name, manifest.plugin.name
        ));
    }

    // Run the full validation pipeline — same checks as `add()`.
    if let Err(e) = validate_overlay_keys(&manifest.config) {
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!(
            "staged manifest failed config overlay validation: {e}"
        ));
    }
    if let Err(e) = validate_mcp_commands(&manifest.mcp.servers, mcp_allowed_commands) {
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!(
            "staged manifest failed MCP command validation: {e}"
        ));
    }

    // Skill conflict check: build a temporary manager against the staging tree.
    let tmp_mgr = crate::manager::PluginManager::new(
        plugins_dir.to_path_buf(),
        managed_skills_dir.to_path_buf(),
        mcp_allowed_commands.to_vec(),
        base_allowed_commands.to_vec(),
    );
    let staged_skill_names = collect_skill_names(staging, &manifest);
    if let Err(e) =
        tmp_mgr.check_skill_conflicts_for_update(&staged_skill_names, installed_plugin_name)
    {
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!("staged manifest failed skill conflict check: {e}"));
    }

    // Advisory SKILL.md scan (non-blocking — logs warnings only).
    scan_skill_entries(staging, &manifest.skills, &manifest.plugin.name);

    // Write the normalised .plugin.toml and strip bundled markers.
    let installed_manifest_toml =
        toml::to_string(&manifest).map_err(|e| format!("cannot serialize staged manifest: {e}"))?;
    std::fs::write(staging.join(".plugin.toml"), &installed_manifest_toml)
        .map_err(|e| format!("cannot write staged .plugin.toml: {e}"))?;
    strip_bundled_markers(staging);

    // Atomic swap: rename dest → backup, rename staging → dest.
    if dest.exists() {
        std::fs::rename(dest, backup)
            .map_err(|e| format!("failed to rename plugin dir to backup: {e}"))?;
    }
    if let Err(e) = std::fs::rename(staging, dest) {
        // Restore from backup.
        if backup.exists() {
            let _ = std::fs::rename(backup, dest);
        }
        return Err(format!("failed to rename staging dir to dest: {e}"));
    }

    // Update integrity registry for the new manifest.
    let installed_manifest_path = dest.join(".plugin.toml");
    let mut registry = crate::integrity::IntegrityRegistry::load(integrity_registry_path);
    if let Err(e) = registry
        .record(&manifest.plugin.name, &installed_manifest_path)
        .and_then(|()| registry.save(integrity_registry_path))
    {
        tracing::warn!(
            plugin = %manifest.plugin.name,
            error = %e,
            "auto-update: failed to update integrity registry after swap"
        );
    }

    let _ = std::fs::remove_dir_all(backup);
    Ok(())
}

/// Validate that a plugin name is a safe identifier: `[a-z][a-z0-9-]*`, max 64 chars.
pub(crate) fn validate_plugin_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty() {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must not be empty".to_owned(),
        });
    }
    if name.len() > 64 {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must not exceed 64 characters".to_owned(),
        });
    }
    if name.contains('/') || name.contains('\\') || name.contains('.') {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must not contain path separators or dots".to_owned(),
        });
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must start with a lowercase ASCII letter [a-z]".to_owned(),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must match [a-z][a-z0-9-]*".to_owned(),
        });
    }
    Ok(())
}

/// Returns a warning message if the plugin's `allowed_commands` overlay
/// will be silently dropped because the host's base allowlist is empty.
///
/// Returns `None` when the overlay is absent or empty, or when the base
/// allowlist is non-empty (in which case the overlay will narrow it and
/// the existing `tracing::info!` in `apply_resolved` already signals the
/// transition at load time).
fn check_allowed_commands_overlay_effect(
    config: &toml::Value,
    base_allowed: &[String],
    plugin_name: &str,
) -> Option<String> {
    let overlay_has_entries = config
        .as_table()
        .and_then(|t| t.get("tools"))
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("allowed_commands"))
        .and_then(toml::Value::as_array)
        .is_some_and(|arr| arr.iter().any(toml::Value::is_str));

    if !overlay_has_entries {
        return None;
    }
    if !base_allowed.is_empty() {
        return None;
    }
    Some(format!(
        "plugin {plugin_name:?} declares allowed_commands overlay but the host \
         has no tools.shell.allowed_commands configured; overlay will have no effect \
         at load time (tighten-only: plugins cannot widen an empty base allowlist). \
         Install proceeds. To use this overlay, set tools.shell.allowed_commands \
         in your base config."
    ))
}

/// Validate all keys in the `[config]` overlay are in the tighten-only safelist.
pub(crate) fn validate_overlay_keys(config: &toml::Value) -> Result<(), PluginError> {
    let table = match config.as_table() {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(()),
    };

    for (section, inner) in table {
        let inner_table = inner.as_table().ok_or_else(|| PluginError::UnsafeOverlay {
            key: section.clone(),
        })?;
        for key in inner_table.keys() {
            let dotted = format!("{section}.{key}");
            if !CONFIG_SAFELIST.contains(&dotted.as_str()) {
                return Err(PluginError::UnsafeOverlay { key: dotted });
            }
        }
    }
    Ok(())
}

/// Validate that all plugin MCP servers declare commands that are in the allowlist.
fn validate_mcp_commands(
    servers: &[PluginMcpServer],
    allowed: &[String],
) -> Result<(), PluginError> {
    for server in servers {
        if let Some(cmd) = &server.command {
            // Compare the full command string verbatim — no file_name() fallback.
            // Basename matching would allow `/tmp/evil/npx` when allowlist contains `npx`.
            let ok = allowed.iter().any(|a| a == cmd);
            if !ok {
                return Err(PluginError::DisallowedMcpCommand {
                    id: server.id.clone(),
                    command: cmd.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Stage-1 advisory scan: run injection/exfiltration regex patterns over each `SKILL.md`.
///
/// Matches are logged as `WARN` and never block installation. The Stage-2 LLM semantic
/// scan (when configured) is the blocking gate.
fn scan_skill_entries(
    source_root: &Path,
    entries: &[crate::manifest::SkillEntry],
    plugin_name: &str,
) {
    let span = tracing::info_span!("plugins.manager.skill_scan", plugin = %plugin_name);
    let _guard = span.enter();
    for entry in entries {
        let skill_md_path = source_root.join(&entry.path).join("SKILL.md");
        match std::fs::read_to_string(&skill_md_path) {
            Ok(content) => {
                let result = scan_skill_body(&content);
                if result.has_matches() {
                    tracing::warn!(
                        plugin = %plugin_name,
                        skill = %entry.path,
                        patterns = ?result.matched_patterns,
                        "SKILL.md matched injection/exfiltration patterns (advisory)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = %plugin_name,
                    skill = %entry.path,
                    error = %e,
                    "could not read SKILL.md for scan"
                );
            }
        }
    }
}

/// Collect skill names from a plugin source tree according to the manifest's `[[skills]]` entries.
///
/// Each `[[skills]] path` entry points to a single skill directory that directly contains
/// `SKILL.md`. `SkillRegistry::load` expects *parent* directories, so we pass each entry's
/// parent and collect only the skills whose directory matches the declared path.
fn collect_skill_names(root: &Path, manifest: &PluginManifest) -> Vec<String> {
    // Collect unique parent directories so we can batch-load.
    let mut parent_dirs: Vec<PathBuf> = manifest
        .skills
        .iter()
        .filter_map(|e| {
            let p = root.join(&e.path);
            p.parent().map(Path::to_path_buf)
        })
        .collect();
    parent_dirs.sort();
    parent_dirs.dedup();

    if parent_dirs.is_empty() {
        return Vec::new();
    }

    // Allowed skill directories (resolved absolute paths).
    let allowed: std::collections::HashSet<PathBuf> =
        manifest.skills.iter().map(|e| root.join(&e.path)).collect();

    let registry = SkillRegistry::load(&parent_dirs);
    registry
        .all_meta()
        .iter()
        .filter(|m| allowed.contains(&m.skill_dir))
        .map(|m| m.name.clone())
        .collect()
}

/// Recursively copy `src` directory to `dst`, creating `dst` if needed.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), PluginError> {
    if dst.exists() {
        std::fs::remove_dir_all(dst).map_err(|e| PluginError::Io {
            path: dst.to_path_buf(),
            source: e,
        })?;
    }
    std::fs::create_dir_all(dst).map_err(|e| PluginError::Io {
        path: dst.to_path_buf(),
        source: e,
    })?;

    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry.map_err(|e| PluginError::Io {
            path: src.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        })?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .expect("walkdir yields paths under src");
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| PluginError::Io {
                path: target,
                source: e,
            })?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| PluginError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            std::fs::copy(entry.path(), &target).map_err(|e| PluginError::Io {
                path: target,
                source: e,
            })?;
        }
    }
    Ok(())
}

/// Extract a `.tar.gz` plugin archive into `dest`.
///
/// Only gzip-compressed tar archives are supported. The format is detected by the gzip magic
/// bytes (`0x1f 0x8b`); any other format returns [`PluginError::InvalidSource`].
///
/// # Errors
///
/// Returns [`PluginError::InvalidSource`] when the archive format is unrecognized or extraction
/// fails.
fn extract_archive(bytes: &[u8], dest: &Path, url: &str) -> Result<(), PluginError> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(PluginError::InvalidSource {
            path: url.to_owned(),
            reason: "unsupported archive format: only .tar.gz is supported".to_owned(),
        });
    }
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(dest)
        .map_err(|e| PluginError::InvalidSource {
            path: url.to_owned(),
            reason: format!("tar.gz extraction failed: {e}"),
        })
}

/// Extract a `.tar.gz` archive with tar-slip protection.
///
/// Unlike [`extract_archive`], this function rejects:
/// - Absolute paths inside the archive.
/// - Entries with `..` path components.
/// - Symbolic link entries (prevent symlink-based traversal).
///
/// # Errors
///
/// Returns [`PluginError::InvalidSource`] when the archive format is unrecognised, extraction
/// fails, or a dangerous entry is found.
fn extract_archive_safe(bytes: &[u8], dest: &Path, url: &str) -> Result<(), PluginError> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Err(PluginError::InvalidSource {
            path: url.to_owned(),
            reason: "unsupported archive format: only .tar.gz is supported".to_owned(),
        });
    }
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|e| PluginError::InvalidSource {
        path: url.to_owned(),
        reason: format!("cannot read tar entries: {e}"),
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| PluginError::InvalidSource {
            path: url.to_owned(),
            reason: format!("tar entry error: {e}"),
        })?;
        // Clone path display string early to avoid borrow conflicts with unpack_in.
        let entry_path_display = entry
            .path()
            .map_or_else(|_| "<invalid path>".to_owned(), |p| p.display().to_string());
        {
            let entry_path = entry.path().map_err(|e| PluginError::InvalidSource {
                path: url.to_owned(),
                reason: format!("invalid entry path: {e}"),
            })?;
            // Reject absolute paths.
            if entry_path.is_absolute() {
                return Err(PluginError::InvalidSource {
                    path: url.to_owned(),
                    reason: format!("archive contains absolute path: {}", entry_path.display()),
                });
            }
            // Reject path traversal via `..`.
            if entry_path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(PluginError::InvalidSource {
                    path: url.to_owned(),
                    reason: format!(
                        "archive contains path traversal component: {}",
                        entry_path.display()
                    ),
                });
            }
        }
        // Reject symbolic links to prevent symlink-based traversal.
        if entry.header().entry_type().is_symlink() {
            return Err(PluginError::InvalidSource {
                path: url.to_owned(),
                reason: format!(
                    "archive contains a symlink entry: {entry_path_display}; symlinks are not permitted"
                ),
            });
        }
        entry
            .unpack_in(dest)
            .map_err(|e| PluginError::InvalidSource {
                path: url.to_owned(),
                reason: format!("tar extraction failed for {entry_path_display}: {e}"),
            })?;
    }
    Ok(())
}

/// Walk the plugin tree and delete every `.bundled` marker file.
///
/// Read the installed manifest (`.plugin.toml`) from `plugin_dir`.
///
/// # Errors
///
/// Returns [`PluginError`] if the file cannot be read or parsed.
fn load_installed_manifest(plugin_dir: &Path) -> Result<PluginManifest, PluginError> {
    let manifest_path = plugin_dir.join(".plugin.toml");
    let bytes = std::fs::read(&manifest_path).map_err(|e| PluginError::Io {
        path: manifest_path.clone(),
        source: e,
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| PluginError::InvalidManifest(".plugin.toml is not valid UTF-8".to_owned()))?;
    toml::from_str(&text).map_err(|e| PluginError::InvalidManifest(format!("{e}")))
}

/// Plugin skills are third-party and must never be treated as bundled by the scanner.
fn strip_bundled_markers(root: &Path) {
    for entry in WalkDir::new(root).into_iter().flatten() {
        if entry.file_type().is_file() && entry.file_name().to_str() == Some(".bundled") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(dir: &Path, name: &str, manifest_toml: &str, skills: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), manifest_toml).unwrap();
        for (skill_name, body) in skills {
            let skill_dir = dir.join("skills").join(skill_name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {skill_name}\ndescription: test\n---\n{body}"),
            )
            .unwrap();
            // Write a .bundled marker to test stripping.
            std::fs::write(skill_dir.join(".bundled"), "").unwrap();
        }
        let _ = name;
    }

    fn simple_manifest(name: &str, skill: &str) -> String {
        format!(
            r#"[plugin]
name = "{name}"
version = "0.1.0"
description = "test plugin"

[[skills]]
path = "skills/{skill}"
"#
        )
    }

    #[test]
    fn add_and_list_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "test-plugin",
            &simple_manifest("test-plugin", "my-skill"),
            &[("my-skill", "Do stuff")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);

        let result = mgr.add(source.to_str().unwrap()).unwrap();
        assert_eq!(result.name, "test-plugin");
        assert!(result.installed_skills.contains(&"my-skill".to_owned()));

        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "test-plugin");
    }

    #[test]
    fn bundled_markers_stripped_on_install() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "strip-test",
            &simple_manifest("strip-test", "my-skill"),
            &[("my-skill", "Body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        // .bundled markers must not exist in the installed tree.
        let has_bundled = WalkDir::new(&plugins_dir)
            .into_iter()
            .flatten()
            .any(|e| e.file_name().to_str() == Some(".bundled"));
        assert!(!has_bundled, ".bundled markers were not stripped");
    }

    #[test]
    fn mcp_disallowed_command_fails_install() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "mcp-test"
version = "0.1.0"
description = "test"

[[mcp.servers]]
id = "bad-server"
command = "dangerous-binary"
"#;
        write_plugin(&source, "mcp-test", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec!["npx".to_owned()], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, PluginError::DisallowedMcpCommand { .. }));
    }

    #[test]
    fn unsafe_config_overlay_fails_install() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "overlay-test"
version = "0.1.0"
description = "test"

[config.llm]
model = "evil"
"#;
        write_plugin(&source, "overlay-test", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, PluginError::UnsafeOverlay { .. }));
    }

    #[test]
    fn max_active_skills_overlay_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "max-skills-test"
version = "0.1.0"
description = "test"

[config.skills]
max_active_skills = 10
"#;
        write_plugin(&source, "max-skills-test", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, PluginError::UnsafeOverlay { .. }));
    }

    #[test]
    fn safe_config_overlay_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "safe-overlay"
version = "0.1.0"
description = "test"

[config.skills]
disambiguation_threshold = 0.05

[config.tools]
blocked_commands = ["rm -rf"]
"#;
        write_plugin(&source, "safe-overlay", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        let result = mgr.add(source.to_str().unwrap()).unwrap();
        assert_eq!(result.name, "safe-overlay");
    }

    #[test]
    fn remove_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "removable",
            &simple_manifest("removable", "my-skill"),
            &[("my-skill", "Body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        let result = mgr.remove("removable").unwrap();
        assert!(result.removed_skills.contains(&"my-skill".to_owned()));

        let installed = mgr.list_installed().unwrap();
        assert!(installed.is_empty());
    }

    #[test]
    fn remove_nonexistent_plugin_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let mgr = PluginManager::new(plugins_dir, tmp.path().to_path_buf(), vec![], vec![]);
        let err = mgr.remove("no-such-plugin").unwrap_err();
        assert!(matches!(err, PluginError::NotFound { .. }));
    }

    #[test]
    fn invalid_plugin_name_with_slash_rejected() {
        let err = validate_plugin_name("foo/bar").unwrap_err();
        assert!(matches!(err, PluginError::InvalidName { .. }));
    }

    #[test]
    fn plugin_name_with_uppercase_rejected() {
        let err = validate_plugin_name("FooBar").unwrap_err();
        assert!(matches!(err, PluginError::InvalidName { .. }));
    }

    #[test]
    fn valid_plugin_names_accepted() {
        assert!(validate_plugin_name("foo").is_ok());
        assert!(validate_plugin_name("foo-bar").is_ok());
        assert!(validate_plugin_name("foo123").is_ok());
    }

    #[test]
    fn bundled_skill_conflict_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");

        // Find a real bundled skill name to trigger conflict.
        let bundled = bundled_skill_names();
        if bundled.is_empty() {
            // No bundled skills compiled in; skip.
            return;
        }
        let conflict_name = &bundled[0];

        let manifest = format!(
            r#"[plugin]
name = "conflict-test"
version = "0.1.0"
description = "test"

[[skills]]
path = "skills/{conflict_name}"
"#
        );
        write_plugin(
            &source,
            "conflict-test",
            &manifest,
            &[(conflict_name, "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(matches!(
            err,
            PluginError::SkillNameConflictWithBundled { .. }
        ));
    }

    #[test]
    fn path_traversal_in_skill_path_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        // Use canonicalized base to avoid macOS /var → /private/var redirect.
        let real_tmp = tmp.path().canonicalize().unwrap();
        let source = real_tmp.join("source");

        // Create a skill directory that exists but is outside source root via ../escape.
        let outside = real_tmp.join("outside-skill");
        std::fs::create_dir_all(&outside).unwrap();

        // The plugin manifest references ../outside-skill, which canonicalizes to a real path
        // outside the source directory — this is what the traversal guard must catch.
        let manifest = r#"[plugin]
name = "traversal-test"
version = "0.1.0"
description = "test"

[[skills]]
path = "../outside-skill"
"#;
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("plugin.toml"), manifest).unwrap();

        let plugins_dir = real_tmp.join("plugins");
        let managed_dir = real_tmp.join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "expected InvalidSource for path traversal, got {err:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn skill_path_canonicalize_failure_returns_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();

        // Create a broken symlink inside the source directory.
        let skill_dir = source.join("skills").join("broken-skill");
        std::fs::create_dir_all(source.join("skills")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", &skill_dir).unwrap();

        let manifest = r#"[plugin]
name = "broken-link-test"
version = "0.1.0"
description = "test"

[[skills]]
path = "skills/broken-skill"
"#;
        std::fs::write(source.join("plugin.toml"), manifest).unwrap();

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::Io { .. }),
            "expected Io error when canonicalize fails on broken symlink, got {err:?}"
        );
    }

    #[test]
    fn mcp_basename_bypass_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        // allowed_commands = ["npx"] but plugin declares full path "/tmp/evil/npx".
        // Verbatim match must reject this; the old file_name() fallback would have passed it.
        let manifest = r#"[plugin]
name = "basename-bypass"
version = "0.1.0"
description = "test"

[[mcp.servers]]
id = "evil"
command = "/tmp/evil/npx"
"#;
        write_plugin(&source, "basename-bypass", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec!["npx".to_owned()], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::DisallowedMcpCommand { .. }),
            "expected DisallowedMcpCommand for basename bypass, got {err:?}"
        );
    }

    #[test]
    fn managed_skill_conflict_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let managed_dir = tmp.path().join("managed");

        // Create a managed skill named "my-skill".
        let managed_skill = managed_dir.join("my-skill");
        std::fs::create_dir_all(&managed_skill).unwrap();
        std::fs::write(
            managed_skill.join("SKILL.md"),
            "---\nname: my-skill\ndescription: managed\n---\nbody",
        )
        .unwrap();

        // Plugin tries to install a skill with the same name.
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "conflict-managed",
            &simple_manifest("conflict-managed", "my-skill"),
            &[("my-skill", "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::SkillNameConflictWithManaged { .. }),
            "expected SkillNameConflictWithManaged, got {err:?}"
        );
    }

    #[test]
    fn cross_plugin_skill_conflict_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        // Install first plugin with "shared-skill".
        let source_a = tmp.path().join("source_a");
        write_plugin(
            &source_a,
            "plugin-a",
            &simple_manifest("plugin-a", "shared-skill"),
            &[("shared-skill", "body")],
        );
        mgr.add(source_a.to_str().unwrap()).unwrap();

        // Install second plugin with the same skill name — must conflict.
        let source_b = tmp.path().join("source_b");
        write_plugin(
            &source_b,
            "plugin-b",
            &simple_manifest("plugin-b", "shared-skill"),
            &[("shared-skill", "body")],
        );
        let err = mgr.add(source_b.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::SkillNameConflictWithPlugin { .. }),
            "expected SkillNameConflictWithPlugin, got {err:?}"
        );
    }

    #[test]
    fn allowed_commands_overlay_with_empty_base_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "warn-test"
version = "0.1.0"
description = "test"

[config.tools]
allowed_commands = ["curl", "git"]
"#;
        write_plugin(&source, "warn-test", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        // base_allowed_commands is empty — overlay will have no effect
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let result = mgr.add(source.to_str().unwrap()).unwrap();
        assert_eq!(result.warnings.len(), 1);
        let msg = &result.warnings[0];
        assert!(
            msg.contains("warn-test"),
            "warning must contain plugin name"
        );
        assert!(
            msg.contains("allowed_commands"),
            "warning must mention allowed_commands"
        );
        assert!(msg.is_ascii(), "warning message must be ASCII-only");
    }

    #[test]
    fn allowed_commands_overlay_with_non_empty_base_no_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "no-warn-test"
version = "0.1.0"
description = "test"

[config.tools]
allowed_commands = ["curl"]
"#;
        write_plugin(&source, "no-warn-test", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        // base_allowed_commands is non-empty — overlay narrows correctly, no warning
        let mgr = PluginManager::new(
            plugins_dir,
            managed_dir,
            vec![],
            vec!["curl".to_owned(), "git".to_owned()],
        );

        let result = mgr.add(source.to_str().unwrap()).unwrap();
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn empty_allowed_commands_array_no_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "empty-overlay"
version = "0.1.0"
description = "test"

[config.tools]
allowed_commands = []
"#;
        write_plugin(&source, "empty-overlay", manifest, &[]);

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let result = mgr.add(source.to_str().unwrap()).unwrap();
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn list_installed_ignores_non_directory_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().to_path_buf();

        // Stray files that must not be treated as installed plugins.
        std::fs::write(plugins_dir.join(".plugin-integrity.toml"), b"plugins = {}").unwrap();
        std::fs::write(plugins_dir.join("README.txt"), b"docs").unwrap();

        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        assert!(
            mgr.list_installed().unwrap().is_empty(),
            "non-directory entries inside plugins_dir must not be surfaced as installed plugins"
        );
    }

    // --- validate_plugin_name edge cases ---

    #[test]
    fn validate_plugin_name_empty_string_rejected() {
        let err = validate_plugin_name("").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for empty string, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_with_dot_rejected() {
        let err = validate_plugin_name("foo.bar").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for name with dot, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_with_backslash_rejected() {
        let err = validate_plugin_name("foo\\bar").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for name with backslash, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_with_space_rejected() {
        let err = validate_plugin_name("foo bar").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for name with space, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_max_length_boundary() {
        assert!(validate_plugin_name(&"a".repeat(64)).is_ok());
        let err = validate_plugin_name(&"a".repeat(65)).unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for 65-char name, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_leading_dash_rejected() {
        let err = validate_plugin_name("-foo").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for leading dash, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_leading_digit_rejected() {
        let err = validate_plugin_name("123").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for digit-only name, got {err:?}"
        );
        let err = validate_plugin_name("1abc").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for digit-prefixed name, got {err:?}"
        );
    }

    #[test]
    fn validate_plugin_name_valid_names_accepted() {
        assert!(validate_plugin_name("abc").is_ok());
        assert!(validate_plugin_name("my-plugin").is_ok());
        assert!(validate_plugin_name("plugin123").is_ok());
    }

    // --- validate_overlay_keys direct tests ---

    #[test]
    fn validate_overlay_keys_empty_config_accepted() {
        let config = toml::Value::Table(toml::map::Map::new());
        assert!(validate_overlay_keys(&config).is_ok());
    }

    #[test]
    fn validate_overlay_keys_safe_keys_accepted() {
        let toml_str = r#"
[tools]
blocked_commands = ["rm -rf /"]
allowed_commands = ["git"]

[skills]
disambiguation_threshold = 0.8
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        assert!(validate_overlay_keys(&config).is_ok());
    }

    #[test]
    fn validate_overlay_keys_unsafe_key_rejected() {
        let toml_str = r#"
[llm]
model = "evil-model"
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let err = validate_overlay_keys(&config).unwrap_err();
        assert!(
            matches!(err, PluginError::UnsafeOverlay { ref key } if key == "llm.model"),
            "expected UnsafeOverlay with key=\"llm.model\", got {err:?}"
        );
    }

    #[test]
    fn validate_overlay_keys_non_table_section_rejected() {
        // A section value that is not a table (e.g. a string) must be rejected.
        let toml_str = r#"
tools = "not-a-table"
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let err = validate_overlay_keys(&config).unwrap_err();
        assert!(
            matches!(err, PluginError::UnsafeOverlay { .. }),
            "expected UnsafeOverlay for non-table section, got {err:?}"
        );
    }

    // --- list_installed sort order ---

    #[test]
    fn list_installed_returns_plugins_sorted_alphabetically() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        // Install in reverse alphabetical order with unique skill names to avoid cross-plugin
        // name conflicts — the sort test only cares about plugin ordering, not skill uniqueness.
        let plugins = [
            ("zeta-plugin", "skill-zeta"),
            ("beta-plugin", "skill-beta"),
            ("alpha-plugin", "skill-alpha"),
        ];
        for (name, skill) in &plugins {
            let source = tmp.path().join(format!("src-{name}"));
            write_plugin(
                &source,
                name,
                &simple_manifest(name, skill),
                &[(skill, "body")],
            );
            mgr.add(source.to_str().unwrap()).unwrap();
        }

        let installed = mgr.list_installed().unwrap();
        let names: Vec<&str> = installed.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha-plugin", "beta-plugin", "zeta-plugin"],
            "list_installed must return plugins in alphabetical order regardless of install order"
        );
    }

    // --- add() error: SkillEntryMissing when SKILL.md is absent ---

    #[test]
    fn add_skill_entry_without_skill_md_returns_skill_entry_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");

        // Create the plugin manifest that references a skill path, but do NOT write SKILL.md.
        std::fs::create_dir_all(source.join("skills").join("no-skill-md")).unwrap();
        let manifest = r#"[plugin]
name = "missing-skill-md"
version = "0.1.0"
description = "test"

[[skills]]
path = "skills/no-skill-md"
"#;
        std::fs::write(source.join("plugin.toml"), manifest).unwrap();

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::SkillEntryMissing { .. }),
            "expected SkillEntryMissing when SKILL.md is absent, got {err:?}"
        );
    }

    // --- collect_skill_dirs ---

    #[test]
    fn collect_skill_dirs_empty_when_no_plugins_installed() {
        let tmp = tempfile::tempdir().unwrap();
        // Use canonicalized path to work around macOS /var → /private/var symlink.
        let real = tmp.path().canonicalize().unwrap();
        let plugins_dir = real.join("plugins");
        let mgr = PluginManager::new(plugins_dir, real.clone(), vec![], vec![]);
        let dirs = mgr.collect_skill_dirs().unwrap();
        assert!(dirs.is_empty());
    }

    #[test]
    fn collect_skill_dirs_returns_installed_skill_paths() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize so that the path prefix check inside collect_skill_dirs works on macOS.
        let real = tmp.path().canonicalize().unwrap();
        let plugins_dir = real.join("plugins");
        let managed_dir = real.join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let source = real.join("source");
        write_plugin(
            &source,
            "dir-plugin",
            &simple_manifest("dir-plugin", "my-skill"),
            &[("my-skill", "body")],
        );
        mgr.add(source.to_str().unwrap()).unwrap();

        let dirs = mgr.collect_skill_dirs().unwrap();
        assert_eq!(dirs.len(), 1, "expected exactly one skill dir");
        assert!(
            dirs[0].ends_with("skills/my-skill"),
            "skill dir path must end with skills/my-skill, got {:?}",
            dirs[0]
        );
    }

    // --- extract_archive tests ---

    #[test]
    fn extract_archive_rejects_non_gz_bytes() {
        let fake_bytes = b"PK\x03\x04not a tar.gz";
        let tmp = tempfile::tempdir().unwrap();
        let err =
            extract_archive(fake_bytes, tmp.path(), "http://example.com/plugin.zip").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "non-gz archive must return InvalidSource, got {err:?}"
        );
    }

    #[test]
    fn sha256_integrity_mismatch_returns_correct_error() {
        // Validate that the sha256_hex function used in add_remote produces a consistent result
        // and that a mismatch would be detected. (We test the hash function and error variant
        // since we cannot call add_remote without an HTTP server in unit tests.)
        let archive_bytes = b"fake archive content";
        let actual = crate::integrity::sha256_hex(archive_bytes);
        let wrong_expected = "0000000000000000000000000000000000000000000000000000000000000000";
        assert_ne!(
            actual, wrong_expected,
            "sha256 of non-zero bytes must not match all-zero expected"
        );
        // Confirm the error variant is constructable.
        let err = PluginError::IntegrityCheckFailed {
            expected: wrong_expected.to_owned(),
            actual: actual.clone(),
        };
        assert!(
            err.to_string().contains("integrity check failed"),
            "error message must mention integrity check"
        );
        assert!(
            err.to_string().contains(&actual),
            "error message must contain actual hash"
        );
    }

    #[test]
    fn collect_skill_dirs_aggregates_multiple_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize so that the path prefix check inside collect_skill_dirs works on macOS.
        let real = tmp.path().canonicalize().unwrap();
        let plugins_dir = real.join("plugins");
        let managed_dir = real.join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        for (plugin_name, skill_name) in &[("plugin-a", "skill-a"), ("plugin-b", "skill-b")] {
            let source = real.join(plugin_name);
            write_plugin(
                &source,
                plugin_name,
                &simple_manifest(plugin_name, skill_name),
                &[(skill_name, "body")],
            );
            mgr.add(source.to_str().unwrap()).unwrap();
        }

        let dirs = mgr.collect_skill_dirs().unwrap();
        assert_eq!(dirs.len(), 2, "expected two skill dirs from two plugins");
    }

    // --- add_remote tests ---

    /// Build an in-memory `.tar.gz` archive of the directory at `source`.
    #[cfg(test)]
    fn build_tar_gz(source: &std::path::Path) -> Vec<u8> {
        let buf = Vec::new();
        let gz = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all(".", source).unwrap();
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap()
    }

    #[tokio::test]
    async fn add_remote_correct_hash_installs_plugin() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "remote-plugin",
            &simple_manifest("remote-plugin", "my-skill"),
            &[("my-skill", "Do remote stuff")],
        );

        let archive = build_tar_gz(&source);
        let expected_hash = crate::integrity::sha256_hex(&archive);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let url = format!("{}/remote-plugin.tar.gz", mock_server.uri());
        let result = mgr.add_remote(&url, Some(&expected_hash)).await.unwrap();
        assert_eq!(result.name, "remote-plugin");
        assert!(result.installed_skills.contains(&"my-skill".to_owned()));
    }

    #[tokio::test]
    async fn add_remote_connect_timeout_returns_download_failed() {
        use std::time::Duration;

        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "timeout-plugin",
            &simple_manifest("timeout-plugin", "t-skill"),
            &[("t-skill", "body")],
        );

        let archive = build_tar_gz(&source);

        let mock_server = MockServer::start().await;
        // Delay > download_timeout_secs (1s) triggers the tokio::time::timeout guard.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive)
                    .set_delay(Duration::from_secs(3)),
            )
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![])
            .with_download_timeout_secs(1);

        let url = format!("{}/timeout-plugin.tar.gz", mock_server.uri());
        let err = mgr.add_remote(&url, None).await.unwrap_err();
        assert!(
            matches!(err, PluginError::DownloadFailed { ref reason, .. } if reason.contains("timed out")),
            "slow response must produce DownloadFailed with timeout message, got {err:?}"
        );
    }

    #[tokio::test]
    async fn add_remote_wrong_hash_returns_integrity_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "bad-plugin",
            &simple_manifest("bad-plugin", "bad-skill"),
            &[("bad-skill", "Body")],
        );

        let archive = build_tar_gz(&source);
        let wrong_hash = "0".repeat(64);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let url = format!("{}/bad-plugin.tar.gz", mock_server.uri());
        let err = mgr.add_remote(&url, Some(&wrong_hash)).await.unwrap_err();
        assert!(
            matches!(err, PluginError::IntegrityCheckFailed { .. }),
            "wrong hash must produce IntegrityCheckFailed, got {err:?}"
        );
    }

    // --- auto_update and PluginSource tests ---

    #[tokio::test]
    async fn add_remote_persists_plugin_source_sidecar() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "src-plugin",
            &simple_manifest("src-plugin", "src-skill"),
            &[("src-skill", "body")],
        );
        let archive = build_tar_gz(&source);
        let expected_hash = crate::integrity::sha256_hex(&archive);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        let url = format!("{}/src-plugin.tar.gz", mock_server.uri());
        mgr.add_remote(&url, Some(&expected_hash)).await.unwrap();

        let sidecar = plugins_dir.join("src-plugin").join(".plugin-source.toml");
        assert!(
            sidecar.exists(),
            ".plugin-source.toml must be written after add_remote"
        );

        let parsed: PluginSource =
            toml::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(parsed.url.as_deref(), Some(url.as_str()));
        assert_eq!(parsed.sha256.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn list_installed_exposes_auto_update_field() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "auto-update-plugin"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &source,
            "auto-update-plugin",
            manifest,
            &[("my-skill", "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert!(
            installed[0].auto_update,
            "InstalledPlugin.auto_update must reflect manifest auto_update = true"
        );
    }

    #[test]
    fn list_installed_auto_update_defaults_to_false() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "no-update-plugin",
            &simple_manifest("no-update-plugin", "skill-a"),
            &[("skill-a", "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        let installed = mgr.list_installed().unwrap();
        assert!(
            !installed[0].auto_update,
            "auto_update must default to false"
        );
    }

    #[tokio::test]
    async fn check_auto_updates_skips_local_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "local-autoupdate"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &source,
            "local-autoupdate",
            manifest,
            &[("my-skill", "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        // No .plugin-source.toml is written by `add()` — only by `add_remote()`.
        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].status, AutoUpdateStatus::NoSource),
            "local-installed plugin must return NoSource, got {:?}",
            results[0].status
        );
    }

    #[tokio::test]
    async fn check_auto_updates_up_to_date_when_sha256_unchanged() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "up-to-date-plugin"
version = "0.2.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &source,
            "up-to-date-plugin",
            manifest,
            &[("my-skill", "body")],
        );
        let archive = build_tar_gz(&source);
        let hash = crate::integrity::sha256_hex(&archive);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive.clone())
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .expect(2) // once for install, once for check
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        let url = format!("{}/plugin.tar.gz", mock_server.uri());
        mgr.add_remote(&url, Some(&hash)).await.unwrap();

        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].status, AutoUpdateStatus::UpToDate),
            "identical archive must yield UpToDate, got {:?}",
            results[0].status
        );
    }

    #[tokio::test]
    async fn check_auto_updates_applies_update_when_archive_changed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");

        // Build v0.1.0 archive.
        let src_v1 = tmp.path().join("src-v1");
        let manifest_v1 = r#"[plugin]
name = "update-test"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &src_v1,
            "update-test",
            manifest_v1,
            &[("my-skill", "v1 body")],
        );
        let archive_v1 = build_tar_gz(&src_v1);
        let hash_v1 = crate::integrity::sha256_hex(&archive_v1);

        // Build v0.2.0 archive.
        let src_v2 = tmp.path().join("src-v2");
        let manifest_v2 = r#"[plugin]
name = "update-test"
version = "0.2.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &src_v2,
            "update-test",
            manifest_v2,
            &[("my-skill", "v2 body")],
        );
        let archive_v2 = build_tar_gz(&src_v2);

        let mock_server = MockServer::start().await;
        // First call: install (serves v1).
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive_v1)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        // Second call: auto-update check (serves v2).
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive_v2)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .mount(&mock_server)
            .await;

        let url = format!("{}/plugin.tar.gz", mock_server.uri());
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        mgr.add_remote(&url, Some(&hash_v1)).await.unwrap();

        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(
                &results[0].status,
                AutoUpdateStatus::Updated { old_version, new_version }
                if old_version == "0.1.0" && new_version == "0.2.0"
            ),
            "changed archive must yield Updated(0.1.0 → 0.2.0), got {:?}",
            results[0].status
        );

        // Installed version must reflect v0.2.0.
        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed[0].version, "0.2.0");
    }

    #[tokio::test]
    async fn check_auto_updates_returns_failed_on_http_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "fail-update"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(&source, "fail-update", manifest, &[("my-skill", "body")]);
        let archive = build_tar_gz(&source);
        let hash = crate::integrity::sha256_hex(&archive);

        let mock_server = MockServer::start().await;
        // Install succeeds.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        // Auto-update check returns 404.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        let url = format!("{}/fail-update.tar.gz", mock_server.uri());
        mgr.add_remote(&url, Some(&hash)).await.unwrap();

        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].status, AutoUpdateStatus::Failed(_)),
            "HTTP 404 must yield Failed, got {:?}",
            results[0].status
        );

        // Plugin must still be installed at the old version.
        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed[0].version, "0.1.0");
    }

    #[tokio::test]
    async fn check_auto_updates_skips_plugins_with_auto_update_false() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        write_plugin(
            &source,
            "no-autoupdate",
            &simple_manifest("no-autoupdate", "skill-b"),
            &[("skill-b", "body")],
        );

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        // auto_update = false (default) — check_auto_updates must return an empty list.
        let results = mgr.check_auto_updates().await;
        assert!(
            results.is_empty(),
            "auto_update=false plugin must be excluded from results"
        );
    }

    // --- Security tests ---

    #[test]
    fn validate_url_scheme_rejects_file_url() {
        let err = validate_url_scheme("file:///etc/passwd").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { ref reason, .. } if reason.contains("file")),
            "file:// URL must be rejected, got {err:?}"
        );
    }

    #[test]
    fn validate_url_scheme_rejects_data_url() {
        let err = validate_url_scheme("data:text/plain,hello").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "data: URL must be rejected, got {err:?}"
        );
    }

    #[test]
    fn validate_url_scheme_accepts_https() {
        assert!(validate_url_scheme("https://example.com/plugin.tar.gz").is_ok());
    }

    #[test]
    fn validate_url_scheme_accepts_http() {
        assert!(validate_url_scheme("http://example.com/plugin.tar.gz").is_ok());
    }

    #[test]
    fn validate_url_scheme_rejects_invalid_url() {
        let err = validate_url_scheme("not a url at all").unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "invalid URL must return InvalidSource, got {err:?}"
        );
    }

    #[tokio::test]
    async fn add_remote_rejects_file_scheme_url() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);
        let err = mgr
            .add_remote("file:///etc/passwd", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "add_remote must reject file:// URL, got {err:?}"
        );
    }

    #[tokio::test]
    async fn check_auto_updates_rejects_file_scheme_in_source() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "ssrf-test"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(&source, "ssrf-test", manifest, &[("my-skill", "body")]);
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        mgr.add(source.to_str().unwrap()).unwrap();

        // Manually write a malicious .plugin-source.toml with file:// URL.
        let sidecar = plugins_dir.join("ssrf-test").join(".plugin-source.toml");
        std::fs::write(
            &sidecar,
            r#"url = "file:///etc/passwd"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
        )
        .unwrap();

        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].status, AutoUpdateStatus::Failed(_)),
            "file:// URL in source sidecar must yield Failed, got {:?}",
            results[0].status
        );
    }

    #[tokio::test]
    async fn check_auto_updates_rejects_name_change_in_update() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");

        // Install v0.1.0 as "original-plugin".
        let src_v1 = tmp.path().join("src-v1");
        let manifest_v1 = r#"[plugin]
name = "original-plugin"
version = "0.1.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &src_v1,
            "original-plugin",
            manifest_v1,
            &[("my-skill", "v1")],
        );
        let archive_v1 = build_tar_gz(&src_v1);
        let hash_v1 = crate::integrity::sha256_hex(&archive_v1);

        // Build an "update" archive that renames the plugin to "evil-plugin".
        let src_evil = tmp.path().join("src-evil");
        let manifest_evil = r#"[plugin]
name = "evil-plugin"
version = "0.2.0"
description = "test"
auto_update = true

[[skills]]
path = "skills/my-skill"
"#;
        write_plugin(
            &src_evil,
            "evil-plugin",
            manifest_evil,
            &[("my-skill", "evil")],
        );
        let archive_evil = build_tar_gz(&src_evil);

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive_v1)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(archive_evil)
                    .append_header("Content-Type", "application/octet-stream"),
            )
            .mount(&mock_server)
            .await;

        let url = format!("{}/plugin.tar.gz", mock_server.uri());
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        mgr.add_remote(&url, Some(&hash_v1)).await.unwrap();

        let results = mgr.check_auto_updates().await;
        assert_eq!(results.len(), 1);
        assert!(
            matches!(results[0].status, AutoUpdateStatus::Failed(_)),
            "name change in update archive must yield Failed, got {:?}",
            results[0].status
        );

        // Original plugin must still be installed at v0.1.0.
        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed[0].version, "0.1.0");
    }

    #[test]
    fn extract_archive_safe_path_traversal_detection() {
        // Verify the path-component check logic used inside extract_archive_safe.
        // The tar builder itself rejects `..` entries, so we test the detection logic
        // directly by constructing a path and running the same check.
        let traversal = std::path::Path::new("subdir/../../../etc/evil");
        let has_traversal = traversal
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        assert!(
            has_traversal,
            "path with .. components must be detected as a traversal attempt"
        );

        let safe = std::path::Path::new("plugin/skills/my-skill/SKILL.md");
        let safe_ok = safe
            .components()
            .all(|c| c != std::path::Component::ParentDir);
        assert!(safe_ok, "safe relative path must pass traversal check");
    }

    // --- dependency enforcement tests ---

    fn install_plugin_with_deps(plugins_dir: &Path, managed_dir: &Path, name: &str, deps: &[&str]) {
        // Use a canonicalized tmp dir so the path-prefix check in collect_skill_dirs works on
        // macOS where /tmp is a symlink to /private/tmp.
        let plugin_src_raw = tempfile::tempdir().unwrap();
        let plugin_src = plugin_src_raw.path().canonicalize().unwrap();
        let deps_toml = deps
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let skill_name = format!("skill-{name}");
        let manifest = format!(
            "[plugin]\nname = \"{name}\"\nversion = \"0.1.0\"\ndependencies = [{deps_toml}]\n\n[[skills]]\npath = \"skills/{skill_name}\"\n"
        );
        write_plugin(&plugin_src, name, &manifest, &[(&skill_name, "test skill")]);
        let mgr = PluginManager::new(
            plugins_dir.to_path_buf(),
            managed_dir.to_path_buf(),
            vec![],
            vec![],
        );
        mgr.add(plugin_src.to_str().unwrap()).unwrap();
    }

    #[test]
    fn dependencies_field_defaults_to_empty() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let installed = mgr.list_installed().unwrap();
        assert_eq!(installed.len(), 1);
        // Manifest with no dependencies field must deserialize with empty Vec.
        let manifest_path = plugins_dir.path().join("base").join(".plugin.toml");
        let text = std::fs::read_to_string(manifest_path).unwrap();
        let manifest: crate::manifest::PluginManifest = toml::from_str(&text).unwrap();
        assert!(manifest.plugin.dependencies.is_empty());
    }

    #[test]
    fn remove_refused_when_dependent_enabled() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.remove("base").unwrap_err();
        assert!(
            matches!(err, PluginError::DependencyRequired { ref name, .. } if name == "base"),
            "expected DependencyRequired, got {err:?}"
        );
    }

    #[test]
    fn remove_succeeds_after_dependent_removed() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        mgr.remove("ext").unwrap();
        mgr.remove("base").unwrap();
        assert!(mgr.list_installed().unwrap().is_empty());
    }

    #[test]
    fn disable_refused_when_dependent_enabled() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.disable("base", false).unwrap_err();
        assert!(
            matches!(err, PluginError::DependencyRequired { ref name, .. } if name == "base"),
            "expected DependencyRequired, got {err:?}"
        );
    }

    #[test]
    fn disable_and_enable_roundtrip() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        mgr.disable("base", false).unwrap();
        assert!(plugins_dir.path().join("base").join(".disabled").exists());
        mgr.enable("base").unwrap();
        assert!(!plugins_dir.path().join("base").join(".disabled").exists());
    }

    #[test]
    fn disable_idempotent() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        mgr.disable("base", false).unwrap();
        // Second disable must be a no-op, not an error.
        mgr.disable("base", false).unwrap();
    }

    #[test]
    fn enable_idempotent() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        // Plugin is already enabled — second enable is a no-op.
        mgr.enable("base").unwrap();
        mgr.enable("base").unwrap();
    }

    #[test]
    fn enable_transitively_enables_dependencies() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        // Disable both.
        std::fs::write(plugins_dir.path().join("base").join(".disabled"), b"").unwrap();
        std::fs::write(plugins_dir.path().join("ext").join(".disabled"), b"").unwrap();
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        // Enabling ext must also enable base.
        mgr.enable("ext").unwrap();
        assert!(
            !plugins_dir.path().join("base").join(".disabled").exists(),
            "base must be enabled"
        );
        assert!(
            !plugins_dir.path().join("ext").join(".disabled").exists(),
            "ext must be enabled"
        );
    }

    #[test]
    fn enable_detects_dependency_cycle() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        // Install alpha → beta, beta → alpha (cycle).
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "alpha", &["beta"]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "beta", &["alpha"]);
        // Disable both to force the enable path.
        std::fs::write(plugins_dir.path().join("alpha").join(".disabled"), b"").unwrap();
        std::fs::write(plugins_dir.path().join("beta").join(".disabled"), b"").unwrap();
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.enable("alpha").unwrap_err();
        assert!(
            matches!(err, PluginError::DependencyCycle { .. }),
            "expected DependencyCycle, got {err:?}"
        );
    }

    #[test]
    fn disable_ignored_by_dependents_of() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        // Disable ext — it should no longer block removing base.
        std::fs::write(plugins_dir.path().join("ext").join(".disabled"), b"").unwrap();
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        // base has no enabled dependents now.
        mgr.remove("base").unwrap();
    }

    #[test]
    fn enable_returns_missing_dependency_when_dep_not_installed() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(
            plugins_dir.path(),
            managed_dir.path(),
            "needs-ghost",
            &["nonexistent"],
        );
        // Disable the plugin so enable() actually tries to traverse deps.
        std::fs::write(
            plugins_dir.path().join("needs-ghost").join(".disabled"),
            b"",
        )
        .unwrap();
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.enable("needs-ghost").unwrap_err();
        assert!(
            matches!(
                err,
                PluginError::MissingDependency {
                    ref dependency,
                    ..
                } if dependency == "nonexistent"
            ),
            "expected MissingDependency, got {err:?}"
        );
    }

    #[test]
    fn add_rejects_too_many_dependencies() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        let deps: Vec<String> = (0..=64).map(|i| format!("dep-{i:02}")).collect();
        let deps_toml = deps
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            "[plugin]\nname = \"bloated\"\nversion = \"0.1.0\"\ndependencies = [{deps_toml}]\n"
        );
        let plugin_src = tempfile::tempdir().unwrap();
        write_plugin(
            plugin_src.path(),
            "bloated",
            &manifest,
            &[("skill-a", "test")],
        );
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.add(plugin_src.path().to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidManifest(_)),
            "expected InvalidManifest for too many dependencies, got {err:?}"
        );
    }

    #[test]
    fn add_rejects_invalid_dependency_name() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        let manifest =
            "[plugin]\nname = \"myplugin\"\nversion = \"0.1.0\"\ndependencies = [\"../evil\"]\n";
        let plugin_src = tempfile::tempdir().unwrap();
        write_plugin(
            plugin_src.path(),
            "myplugin",
            manifest,
            &[("skill-a", "test")],
        );
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let err = mgr.add(plugin_src.path().to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidName { .. }),
            "expected InvalidName for malformed dep name, got {err:?}"
        );
    }

    #[test]
    fn disable_force_succeeds_despite_dependent() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        // Without force this would fail with DependencyRequired.
        let result = mgr.disable("base", true).unwrap();
        assert!(
            result.forced_over_dependents.contains(&"ext".to_owned()),
            "forced_over_dependents must list 'ext', got {:?}",
            result.forced_over_dependents
        );
        assert!(
            plugins_dir.path().join("base").join(".disabled").exists(),
            "base must be disabled after force"
        );
    }

    #[test]
    fn disable_force_no_dependents_returns_empty_list() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "standalone", &[]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        let result = mgr.disable("standalone", true).unwrap();
        assert!(
            result.forced_over_dependents.is_empty(),
            "no dependents means forced_over_dependents must be empty"
        );
    }

    #[test]
    fn disable_force_false_same_as_no_force() {
        let plugins_dir = tempfile::tempdir().unwrap();
        let managed_dir = tempfile::tempdir().unwrap();
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "base", &[]);
        install_plugin_with_deps(plugins_dir.path(), managed_dir.path(), "ext", &["base"]);
        let mgr = PluginManager::new(
            plugins_dir.path().to_path_buf(),
            managed_dir.path().to_path_buf(),
            vec![],
            vec![],
        );
        // force=false with dependents must still refuse.
        let err = mgr.disable("base", false).unwrap_err();
        assert!(
            matches!(err, PluginError::DependencyRequired { .. }),
            "expected DependencyRequired with force=false, got {err:?}"
        );
    }

    #[test]
    fn collect_skill_dirs_excludes_disabled_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize so the path-prefix check inside collect_skill_dirs works on macOS.
        let real = tmp.path().canonicalize().unwrap();
        let plugins_dir = real.join("plugins");
        let managed_dir = real.join("managed");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::create_dir_all(&managed_dir).unwrap();
        install_plugin_with_deps(&plugins_dir, &managed_dir, "active", &[]);
        install_plugin_with_deps(&plugins_dir, &managed_dir, "sleeping", &[]);
        // Disable sleeping.
        std::fs::write(plugins_dir.join("sleeping").join(".disabled"), b"").unwrap();
        let mgr = PluginManager::new(plugins_dir.clone(), managed_dir, vec![], vec![]);
        let dirs = mgr.collect_skill_dirs().unwrap();
        // Only the active plugin's skill dirs should appear.
        for dir in &dirs {
            assert!(
                !dir.to_string_lossy().contains("sleeping"),
                "disabled plugin skill dir must not appear: {dir:?}"
            );
        }
        assert!(!dirs.is_empty(), "active plugin skills must be present");
    }
}
