// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin lifecycle management: add, remove, list.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zeph_skills::bundled::bundled_skill_names;
use zeph_skills::registry::SkillRegistry;

use crate::PluginError;
use crate::manifest::{PluginManifest, PluginMcpServer};

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
        Self {
            plugins_dir,
            managed_skills_dir,
            mcp_allowed_commands,
            base_allowed_commands,
        }
    }

    /// Install a plugin from a local directory path.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the manifest is invalid, the source cannot be read,
    /// there are skill name conflicts, MCP commands are not allowlisted, or config
    /// overlay keys are not in the tighten-only safelist.
    pub fn add(&self, source: &str) -> Result<AddResult, PluginError> {
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
        let manifest: PluginManifest = toml::from_str(&String::from_utf8_lossy(&manifest_bytes))
            .map_err(|e| PluginError::InvalidManifest(format!("{e}")))?;

        // Validate plugin name.
        validate_plugin_name(&manifest.plugin.name)?;

        // Validate each [[skills]] entry: path must stay within source root and SKILL.md must exist.
        for entry in &manifest.skills {
            let skill_path = source_path.join(&entry.path);
            // Reject path traversal: resolved path must be inside source_path.
            let canonical_source = source_path.canonicalize().map_err(|e| PluginError::Io {
                path: source_path.clone(),
                source: e,
            })?;
            let canonical_skill = skill_path
                .canonicalize()
                .unwrap_or_else(|_| skill_path.clone());
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
        std::fs::write(&installed_manifest_path, manifest_str).map_err(|e| PluginError::Io {
            path: installed_manifest_path,
            source: e,
        })?;

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
    /// # Errors
    ///
    /// Returns [`PluginError::NotFound`] if the plugin is not installed.
    pub fn remove(&self, name: &str) -> Result<RemoveResult, PluginError> {
        validate_plugin_name(name)?;
        let plugin_dir = self.plugins_dir.join(name);
        if !plugin_dir.exists() {
            return Err(PluginError::NotFound {
                name: name.to_owned(),
            });
        }

        let manifest_path = plugin_dir.join(".plugin.toml");
        let (removed_skills, removed_mcp_ids) = if manifest_path.exists() {
            let bytes = std::fs::read(&manifest_path).map_err(|e| PluginError::Io {
                path: manifest_path,
                source: e,
            })?;
            let manifest: PluginManifest = toml::from_str(&String::from_utf8_lossy(&bytes))
                .map_err(|e| PluginError::InvalidManifest(format!("{e}")))?;
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
            let Ok(manifest): Result<PluginManifest, _> =
                toml::from_str(&String::from_utf8_lossy(&bytes))
            else {
                continue;
            };
            plugins.push(InstalledPlugin {
                name: manifest.plugin.name,
                version: manifest.plugin.version,
                description: manifest.plugin.description,
                path,
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
    pub fn collect_skill_dirs(&self) -> Result<Vec<PathBuf>, PluginError> {
        if !self.plugins_dir.exists() {
            return Ok(Vec::new());
        }

        let mut dirs = Vec::new();
        let plugins = self.list_installed()?;
        for plugin in &plugins {
            let manifest_path = plugin.path.join(".plugin.toml");
            if let Ok(bytes) = std::fs::read(&manifest_path)
                && let Ok(manifest) =
                    toml::from_str::<PluginManifest>(&String::from_utf8_lossy(&bytes))
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

        // Other installed plugins' skill names.
        let installed = self.list_installed().unwrap_or_default();
        let mut other_plugin_skills: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for plugin in &installed {
            if plugin.name == this_plugin {
                continue;
            }
            let manifest_path = plugin.path.join(".plugin.toml");
            if let Ok(bytes) = std::fs::read(&manifest_path)
                && let Ok(manifest) =
                    toml::from_str::<PluginManifest>(&String::from_utf8_lossy(&bytes))
            {
                let names = collect_skill_names(&plugin.path, &manifest);
                for name in names {
                    other_plugin_skills.insert(name, plugin.name.clone());
                }
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

/// Validate that a plugin name is a safe identifier: `[a-z0-9][a-z0-9-]*`.
pub(crate) fn validate_plugin_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty() {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must not be empty".to_owned(),
        });
    }
    if name.contains('/') || name.contains('\\') || name.contains('.') {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must not contain path separators or dots".to_owned(),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(PluginError::InvalidName {
            name: name.to_owned(),
            reason: "name must match [a-z0-9][a-z0-9-]*".to_owned(),
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

/// Walk the plugin tree and delete every `.bundled` marker file.
///
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
        let source = tmp.path().join("source");
        let manifest = r#"[plugin]
name = "traversal-test"
version = "0.1.0"
description = "test"

[[skills]]
path = "../../../etc/passwd"
"#;
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("plugin.toml"), manifest).unwrap();

        let plugins_dir = tmp.path().join("plugins");
        let managed_dir = tmp.path().join("managed");
        let mgr = PluginManager::new(plugins_dir, managed_dir, vec![], vec![]);

        let err = mgr.add(source.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, PluginError::InvalidSource { .. }),
            "expected InvalidSource for path traversal, got {err:?}"
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
}
