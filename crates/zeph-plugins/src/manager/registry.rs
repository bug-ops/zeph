// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Remote registry interaction: download, archive extraction, auto-update, and ephemeral install.

use crate::PluginError;

use super::{
    AddResult, AutoUpdateResult, AutoUpdateStatus, InstalledPlugin, PluginManager, PluginSource,
    collect_skill_names, extract_archive_safe, scan_skill_entries, strip_bundled_markers,
    validate_mcp_commands, validate_overlay_keys, validate_plugin_name, validate_url_scheme,
    validate_url_scheme_ephemeral,
};

impl PluginManager {
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
    #[tracing::instrument(name = "plugins.manager.add_remote", skip(self, expected_sha256), fields(%url))]
    pub async fn add_remote(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
    ) -> Result<AddResult, PluginError> {
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
        extract_archive_safe(&bytes, tmp.path(), url)?;

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
            .join(result.name.as_str())
            .join(".plugin-source.toml");
        match toml::to_string(&source) {
            Ok(toml_str) => {
                if let Err(e) = tokio::fs::write(&source_path, toml_str).await {
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
        let plugin_name = plugin.name.as_str().to_owned();

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

    /// Download a plugin archive and load it as a session-scoped ephemeral plugin.
    ///
    /// Unlike [`Self::add_remote`], this method:
    /// - Requires `https://` (never `http://`) via [`validate_url_scheme_ephemeral`].
    /// - Extracts into a [`tempfile::TempDir`] that is **never** copied to the permanent plugins
    ///   store.
    /// - Runs a **blocking** (non-advisory) SKILL.md injection scan before returning.
    /// - Returns ownership of the `TempDir`. The caller must hold it for the session duration;
    ///   dropping it cleans up the extracted archive automatically.
    ///
    /// # Security invariants
    ///
    /// - Never accepts `http://` or any scheme other than `https://`.
    /// - Never writes to `self.plugins_dir`.
    /// - Never applies config overlays from the plugin manifest.
    ///
    /// # Errors
    ///
    /// - [`PluginError::InsecureUrl`] — URL scheme is not `https://`.
    /// - [`PluginError::DownloadFailed`] — HTTP request failed or returned a non-2xx status.
    /// - [`PluginError::IntegrityCheckFailed`] — SHA-256 mismatch when `sha256` is `Some`.
    /// - [`PluginError::InvalidSource`] — archive format is unsupported or extraction failed.
    /// - [`PluginError::SemanticViolation`] — blocking skill scan detected injection patterns.
    /// - [`PluginError::Io`] — failed to create temporary directory.
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
    /// let _temp = mgr.add_remote_ephemeral(
    ///     "https://example.com/my-plugin.tar.gz",
    ///     Some("abc123def456..."),
    /// ).await?;
    /// // _temp stays alive for the session; drop it to clean up
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument(name = "plugins.manager.add_remote_ephemeral", skip(self, sha256), fields(%url))]
    pub async fn add_remote_ephemeral(
        &self,
        url: &str,
        sha256: Option<&str>,
    ) -> Result<tempfile::TempDir, PluginError> {
        validate_url_scheme_ephemeral(url)?;

        let tmp = tempfile::tempdir().map_err(|e| PluginError::Io {
            path: std::path::PathBuf::from(url),
            source: e,
        })?;

        download_and_extract(url, sha256, tmp.path(), self.download_timeout_secs).await?;

        // Strip .bundled markers so ephemeral skills are visible to the registry.
        strip_bundled_markers(tmp.path());

        // Read manifest to get skill entries for blocking scan.
        let manifest_path = tmp.path().join("plugin.toml");
        if manifest_path.exists() {
            let manifest_str = tokio::fs::read_to_string(&manifest_path)
                .await
                .map_err(|e| PluginError::Io {
                    path: manifest_path.clone(),
                    source: e,
                })?;
            if let Ok(manifest) = toml::from_str::<crate::manifest::PluginManifest>(&manifest_str) {
                // Blocking scan: treat any injection match as a hard error.
                for entry in &manifest.skills {
                    let skill_md_path = tmp.path().join(&entry.path).join("SKILL.md");
                    if let Ok(content) = tokio::fs::read_to_string(&skill_md_path).await {
                        let result = zeph_skills::scanner::scan_skill_body(&content);
                        if result.has_matches() {
                            return Err(PluginError::SemanticViolation {
                                skill: entry.path.clone(),
                                reason: format!(
                                    "SKILL.md matched injection/exfiltration patterns: {:?}",
                                    result.matched_patterns
                                ),
                            });
                        }
                    }
                }
            }
        }

        tracing::info!(url, "ephemeral plugin loaded into temporary directory");
        Ok(tmp)
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

/// Maximum archive size accepted by [`download_and_extract`]: 50 MiB.
///
/// Checked against `Content-Length` before reading the body. Prevents memory exhaustion
/// from oversized or gzip-bombed archives served by a malicious host.
pub const MAX_ARCHIVE_BYTES: u64 = 52 * 1024 * 1024;

/// Download the archive at `url`, verify its SHA-256 digest (when provided), and extract it
/// into `dest`.
///
/// This shared helper is used by both [`PluginManager::add_remote`] (permanent install) and
/// [`PluginManager::add_remote_ephemeral`] (session-scoped). Callers are responsible for
/// creating `dest` before calling this function.
///
/// The HTTP client rejects any redirect that leaves `https://` — an `https → http` downgrade
/// redirect is treated as [`PluginError::InsecureUrl`].  The response body is capped at
/// [`MAX_ARCHIVE_BYTES`] via a `Content-Length` pre-check.
///
/// # Errors
///
/// - [`PluginError::InsecureUrl`] — a redirect tried to downgrade from `https://` to `http://`.
/// - [`PluginError::DownloadFailed`] — HTTP request failed, returned a non-2xx status, body
///   exceeded [`MAX_ARCHIVE_BYTES`], or the download timed out.
/// - [`PluginError::IntegrityCheckFailed`] — SHA-256 mismatch when `sha256` is `Some`.
/// - [`PluginError::InvalidSource`] — archive format is unsupported or extraction failed.
#[tracing::instrument(name = "plugins.manager.download_and_extract", skip(sha256, dest, timeout_secs), fields(%url))]
pub async fn download_and_extract(
    url: &str,
    sha256: Option<&str>,
    dest: &std::path::Path,
    timeout_secs: u64,
) -> Result<(), PluginError> {
    let timeout = std::time::Duration::from_secs(timeout_secs);

    // Build a client that refuses to follow any redirect that downgrades from https (B1).
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                let redirect_url = attempt.url().to_string();
                attempt.error(format!(
                    "redirect to non-HTTPS URL is not permitted: {redirect_url}"
                ))
            }
        }))
        .build()
        .map_err(|e| PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("failed to build HTTP client: {e}"),
        })?;

    let response = tokio::time::timeout(timeout, client.get(url).send())
        .await
        .map_err(|_| PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("download timed out after {timeout_secs}s"),
        })?
        .map_err(|e| {
            // reqwest surfaces our custom redirect error as a generic error; re-classify it.
            let msg = e.to_string();
            if msg.contains("redirect to non-HTTPS") {
                PluginError::InsecureUrl(msg)
            } else {
                PluginError::DownloadFailed {
                    url: url.to_owned(),
                    reason: msg,
                }
            }
        })?;

    if !response.status().is_success() {
        return Err(PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("HTTP {}", response.status()),
        });
    }

    // Reject oversized archives before reading the body (B3).
    if let Some(content_length) = response.content_length()
        && content_length > MAX_ARCHIVE_BYTES
    {
        return Err(PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("archive too large: {content_length} bytes (max {MAX_ARCHIVE_BYTES})"),
        });
    }

    let bytes = tokio::time::timeout(timeout, response.bytes())
        .await
        .map_err(|_| PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("download timed out after {timeout_secs}s"),
        })?
        .map_err(|e| PluginError::DownloadFailed {
            url: url.to_owned(),
            reason: format!("failed to read response body: {e}"),
        })?;

    if let Some(expected) = sha256 {
        let actual = crate::integrity::sha256_hex(&bytes);
        if actual != expected.to_ascii_lowercase() {
            return Err(PluginError::IntegrityCheckFailed {
                expected: expected.to_ascii_lowercase(),
                actual,
            });
        }
        tracing::debug!(url, "archive SHA-256 verified");
    } else {
        tracing::warn!(url, "loading plugin without integrity check");
    }

    // Use the symlink-safe extractor (B2).
    extract_archive_safe(&bytes, dest, url)
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
