// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Filesystem state and path helpers: listing installed plugins and reading manifests.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;
use zeph_skills::registry::SkillRegistry;

use crate::PluginError;
use crate::manifest::PluginManifest;

use super::{InstalledPlugin, PluginManager};

impl PluginManager {
    /// List all installed plugins.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the plugins directory cannot be read.
    #[tracing::instrument(name = "plugins.store.list_installed", skip_all)]
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
}

/// Collect skill names from a plugin source tree according to the manifest's `[[skills]]` entries.
///
/// Each `[[skills]] path` entry points to a single skill directory that directly contains
/// `SKILL.md`. `SkillRegistry::load` expects *parent* directories, so we pass each entry's
/// parent and collect only the skills whose directory matches the declared path.
pub(crate) fn collect_skill_names(root: &Path, manifest: &PluginManifest) -> Vec<String> {
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
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), PluginError> {
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
/// Read the installed manifest (`.plugin.toml`) from `plugin_dir`.
///
/// # Errors
///
/// Returns [`PluginError`] if the file cannot be read or parsed.
pub(crate) fn load_installed_manifest(plugin_dir: &Path) -> Result<PluginManifest, PluginError> {
    let manifest_path = plugin_dir.join(".plugin.toml");
    let bytes = std::fs::read(&manifest_path).map_err(|e| PluginError::Io {
        path: manifest_path.clone(),
        source: e,
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| PluginError::InvalidManifest(".plugin.toml is not valid UTF-8".to_owned()))?;
    toml::from_str(&text).map_err(|e| PluginError::InvalidManifest(format!("{e}")))
}

/// Extract `name` and `description` from a SKILL.md YAML frontmatter block.
///
/// Returns the parsed values on success or `(fallback_path.to_owned(), String::new())` on
/// any parse failure. The caller surfaces these as `skill_name` and `declared_purpose` in
/// [`SkillScanInput`].
pub(crate) fn parse_frontmatter_meta(content: &str, fallback_path: &str) -> (String, String) {
    // SKILL.md frontmatter is delimited by `---` lines.
    let after_open = content.strip_prefix("---").and_then(|s| {
        // Allow `---\n` or `--- \n`.
        s.strip_prefix('\n')
            .or_else(|| s.strip_prefix(" \n"))
            .or_else(|| s.strip_prefix('\r'))
    });
    let Some(rest) = after_open else {
        return (fallback_path.to_owned(), String::new());
    };
    let Some(end) = rest.find("\n---") else {
        return (fallback_path.to_owned(), String::new());
    };
    let frontmatter = &rest[..end];

    let mut name = fallback_path.to_owned();
    let mut description = String::new();
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            v.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .clone_into(&mut name);
        } else if let Some(v) = line.strip_prefix("description:") {
            v.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .clone_into(&mut description);
        }
    }
    (name, description)
}

/// Plugin skills are third-party and must never be treated as bundled by the scanner.
pub(crate) fn strip_bundled_markers(root: &Path) {
    for entry in WalkDir::new(root).into_iter().flatten() {
        if entry.file_type().is_file() && entry.file_name().to_str() == Some(".bundled") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
