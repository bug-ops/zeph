// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin install lifecycle: add, remove, enable, disable, and dependency guards.

use std::path::PathBuf;

use crate::PluginError;
use crate::manifest::PluginManifest;

use super::{
    AddResult, DisableResult, PluginManager, RemoveResult, check_allowed_commands_overlay_effect,
    collect_skill_names, copy_dir_all, load_installed_manifest, scan_skill_entries,
    strip_bundled_markers, validate_manifest_for_install, validate_mcp_commands,
    validate_overlay_keys, validate_plugin_name,
};

impl PluginManager {
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

        // Name validity is enforced by PluginName deserialization; no separate call needed.

        // Validate dependency list and [[skills]] path entries (shared with apply_staged_update).
        validate_manifest_for_install(&source_path, &manifest)?;

        // Validate config overlay keys.
        validate_overlay_keys(&manifest.config)?;

        // Stage-1: advisory regex scan over each SKILL.md before copying files.
        // Results are warnings only — they never block installation.
        scan_skill_entries(
            source_path.as_path(),
            &manifest.skills,
            manifest.plugin.name.as_str(),
        );

        let mut warnings: Vec<String> = Vec::new();
        if let Some(msg) = check_allowed_commands_overlay_effect(
            &manifest.config,
            &self.base_allowed_commands,
            manifest.plugin.name.as_str(),
        ) {
            tracing::warn!(plugin = %manifest.plugin.name, "{msg}");
            warnings.push(msg);
        }

        // Validate MCP command allowlist.
        validate_mcp_commands(&manifest.mcp.servers, &self.mcp_allowed_commands)?;

        // Collect skill names from the plugin source.
        let skill_names = collect_skill_names(&source_path, &manifest);

        // Check for name conflicts.
        self.check_skill_conflicts(&skill_names, manifest.plugin.name.as_str())?;

        let dest = self.plugins_dir.join(manifest.plugin.name.as_str());

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
            .record(manifest.plugin.name.as_str(), &installed_manifest_path)
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
    #[tracing::instrument(name = "plugins.install.remove", skip_all)]
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
    #[tracing::instrument(name = "plugins.install.enable", skip_all)]
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
    #[tracing::instrument(name = "plugins.install.disable", skip_all)]
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
                dependents.push(manifest.plugin.name.to_string());
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
}
