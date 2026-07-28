// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin security: URL/name/overlay validation, MCP allowlisting, archive safety, and skill scanning.

use std::path::{Path, PathBuf};

use zeph_skills::bundled::bundled_skill_names;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::scanner::scan_skill_body;

use crate::PluginError;
use crate::manifest::{PluginManifest, PluginMcpServer};

use super::{
    MatchedSource, PluginManager, ReputationWarning, SkillScanInput, parse_frontmatter_meta,
};

/// The tighten-only config overlay safelist. Any key outside this list causes
/// [`PluginError::UnsafeOverlay`] at install time.
const CONFIG_SAFELIST: &[&str] = &[
    "tools.blocked_commands",
    "tools.allowed_commands",
    "skills.disambiguation_threshold",
];

/// Maximum number of entries allowed in `plugin.dependencies`.
///
/// Prevents a malicious manifest from triggering a fan-out `DoS` via recursive `enable()` calls
/// across an unbounded dependency graph.
const MAX_DEPENDENCIES: usize = 64;

/// Validate a manifest's dependency list and `[[skills]] path` entries before it is installed.
///
/// # Security invariant
///
/// This is the single shared checkpoint for two checks that must run on **every** manifest
/// before its declared skills are ever interpreted by
/// [`super::collect_skill_names`]/`SkillRegistry::load`:
///
/// 1. **Dependency bound** — rejects manifests with more than [`MAX_DEPENDENCIES`] entries or a
///    malformed dependency name, bounding the fan-out of recursive `enable()` calls.
/// 2. **Skill path traversal** — each `[[skills]] path` entry is canonicalized and must resolve
///    inside `source_path`. [`extract_archive_safe`]'s tar-slip protection only guards *tar
///    entry paths* during extraction; it does not protect against a manifest `path` value
///    (e.g. `"../../etc"`) that is interpreted later, after extraction, by
///    `collect_skill_names`/`SkillRegistry::load`.
///
/// [`PluginManager::add`] (fresh installs) and `apply_staged_update` (the auto-update path) both
/// write a manifest's declared skills to disk and must call this function so the two call sites
/// cannot silently diverge again — see issue #5401, where `apply_staged_update` re-ran the
/// overlay/MCP/skill-conflict checks but skipped both of these, letting a compromised
/// auto-update URL bypass validation that `add()` would have enforced.
///
/// # Errors
///
/// - [`PluginError::InvalidManifest`] — dependency count exceeds [`MAX_DEPENDENCIES`].
/// - [`PluginError::InvalidName`] — a dependency name fails [`validate_plugin_name`].
/// - [`PluginError::Io`] — `source_path` or a skill path cannot be canonicalized.
/// - [`PluginError::InvalidSource`] — a `[[skills]] path` entry resolves outside `source_path`.
/// - [`PluginError::SkillEntryMissing`] — a `[[skills]] path` entry has no `SKILL.md`.
pub(crate) fn validate_manifest_for_install(
    source_path: &Path,
    manifest: &PluginManifest,
) -> Result<(), PluginError> {
    if manifest.plugin.dependencies.len() > MAX_DEPENDENCIES {
        return Err(PluginError::InvalidManifest(format!(
            "plugin declares {} dependencies; maximum allowed is {MAX_DEPENDENCIES}",
            manifest.plugin.dependencies.len()
        )));
    }
    for dep in &manifest.plugin.dependencies {
        validate_plugin_name(dep)?;
    }

    for entry in &manifest.skills {
        let skill_path = source_path.join(&entry.path);
        // Reject path traversal: resolved path must be inside source_path.
        let canonical_source = source_path.canonicalize().map_err(|e| PluginError::Io {
            path: source_path.to_path_buf(),
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
    Ok(())
}

impl PluginManager {
    /// Collect [`SkillScanInput`] entries for each skill in the plugin at `source`.
    ///
    /// Validates the manifest and skill paths without copying any files. The returned
    /// inputs are passed to `SkillSemanticScanner::scan` by the caller (core/commands
    /// layer) before the blocking `add()` call proceeds. This keeps `zeph-plugins`
    /// free of any LLM dependency.
    ///
    /// Missing `SKILL.md` files return [`PluginError::SkillEntryMissing`] — a manifest
    /// entry with no body is suspicious and treated as a blocking error, not a warning.
    ///
    /// # Errors
    ///
    /// - [`PluginError::InvalidSource`] — `source` does not exist or `plugin.toml` is missing/invalid.
    /// - [`PluginError::SkillEntryMissing`] — a `[[skills]]` entry has no `SKILL.md`.
    /// - [`PluginError::Io`] — filesystem error while reading `SKILL.md`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use zeph_plugins::PluginManager;
    ///
    /// fn collect(mgr: &PluginManager, source: &str) -> Result<(), zeph_plugins::PluginError> {
    ///     let inputs = mgr.scan_targets(source)?;
    ///     for input in &inputs {
    ///         println!("skill: {} — {}", input.skill_name, input.declared_purpose);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    #[tracing::instrument(name = "plugins.security.scan_targets", skip_all)]
    pub fn scan_targets(&self, source: &str) -> Result<Vec<SkillScanInput>, PluginError> {
        // Cap per-file reads to prevent memory DoS when scanning untrusted plugin archives.
        const MAX_SKILL_MD_READ_BYTES: u64 = 512 * 1024; // 512 KiB
        let source_path = std::path::PathBuf::from(source);
        if !source_path.exists() {
            return Err(PluginError::InvalidSource {
                path: source.to_owned(),
                reason: "path does not exist".to_owned(),
            });
        }

        let manifest_path = source_path.join("plugin.toml");
        let manifest_str =
            std::fs::read_to_string(&manifest_path).map_err(|e| PluginError::Io {
                path: manifest_path.clone(),
                source: e,
            })?;
        let manifest: crate::manifest::PluginManifest = toml::from_str(&manifest_str)
            .map_err(|e| PluginError::InvalidManifest(e.to_string()))?;

        let canonical_source = source_path.canonicalize().map_err(|e| PluginError::Io {
            path: source_path.clone(),
            source: e,
        })?;

        let mut inputs = Vec::with_capacity(manifest.skills.len());
        for entry in &manifest.skills {
            let skill_dir = source_path.join(&entry.path);
            let skill_md_path = skill_dir.join("SKILL.md");

            if !skill_md_path.is_file() {
                return Err(PluginError::SkillEntryMissing { path: skill_dir });
            }

            // Reject path traversal: resolved SKILL.md path must stay within source root.
            let canonical_skill = skill_md_path.canonicalize().map_err(|e| PluginError::Io {
                path: skill_md_path.clone(),
                source: e,
            })?;
            if !canonical_skill.starts_with(&canonical_source) {
                return Err(PluginError::InvalidSource {
                    path: entry.path.clone(),
                    reason: "skill path escapes plugin source root".to_owned(),
                });
            }

            // Reject oversized SKILL.md before reading to prevent memory DoS.
            let file_len = skill_md_path.metadata().map_or(0, |m| m.len());
            if file_len > MAX_SKILL_MD_READ_BYTES {
                return Err(PluginError::InvalidSource {
                    path: skill_md_path.display().to_string(),
                    reason: format!(
                        "SKILL.md is too large ({file_len} bytes, max {MAX_SKILL_MD_READ_BYTES})"
                    ),
                });
            }

            let content = std::fs::read_to_string(&skill_md_path).map_err(|e| PluginError::Io {
                path: skill_md_path.clone(),
                source: e,
            })?;

            // Extract name and description from frontmatter; fall back to manifest path on error.
            let (skill_name, declared_purpose) = parse_frontmatter_meta(&content, &entry.path);

            inputs.push(SkillScanInput {
                skill_name,
                declared_purpose,
                skill_md: content,
            });
        }
        Ok(inputs)
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

    pub(crate) fn check_skill_conflicts(
        &self,
        skill_names: &[String],
        this_plugin: &str,
    ) -> Result<(), PluginError> {
        let corpus = self.known_names(this_plugin);
        for name in skill_names {
            if let Some((_, source)) = corpus.iter().find(|(known, _)| known == name) {
                return Err(match source {
                    MatchedSource::Bundled => {
                        PluginError::SkillNameConflictWithBundled { name: name.clone() }
                    }
                    MatchedSource::Managed => {
                        PluginError::SkillNameConflictWithManaged { name: name.clone() }
                    }
                    MatchedSource::Plugin(plugin) => PluginError::SkillNameConflictWithPlugin {
                        name: name.clone(),
                        plugin: plugin.clone(),
                    },
                });
            }
        }
        Ok(())
    }

    /// Assemble the corpus of known skill/plugin names, tagged by [`MatchedSource`], used by
    /// both the exact-match conflict check ([`Self::check_skill_conflicts`]) and the advisory
    /// reputation/typosquat check ([`Self::check_reputation`]) — DRY per spec-043 NFR-005.
    ///
    /// Skips `this_plugin`'s own skills so the auto-update path does not self-flag against the
    /// plugin's own currently-installed skills, which are about to be replaced.
    ///
    /// Order is bundled, then managed, then other-plugin skills — [`Self::check_skill_conflicts`]
    /// relies on this order to report the same conflict category (bundled > managed > plugin)
    /// it did before this helper was extracted.
    pub(crate) fn known_names(&self, this_plugin: &str) -> Vec<(String, MatchedSource)> {
        let mut names: Vec<(String, MatchedSource)> = bundled_skill_names()
            .into_iter()
            .map(|n| (n, MatchedSource::Bundled))
            .collect();

        let managed_registry = {
            let dirs: Vec<PathBuf> = if self.managed_skills_dir.exists() {
                vec![self.managed_skills_dir.clone()]
            } else {
                vec![]
            };
            SkillRegistry::load(&dirs)
        };
        names.extend(
            managed_registry
                .all_meta()
                .iter()
                .map(|m| (m.name.clone(), MatchedSource::Managed)),
        );

        let installed = self.list_installed().unwrap_or_default();
        for plugin in &installed {
            if plugin.name.as_str() == this_plugin {
                continue;
            }
            names.extend(plugin.skill_names.iter().map(|n| {
                (
                    n.clone(),
                    MatchedSource::Plugin(plugin.name.as_str().to_owned()),
                )
            }));
        }
        names
    }

    /// Advisory typosquat/reputation check (spec-043, #5864).
    ///
    /// Returns an empty `Vec` when no [`super::ReputationSource`] is attached
    /// (`self.reputation.is_none()`) — the corpus is not assembled in that case, satisfying
    /// NFR-002 (zero cost when disabled). Checks `plugin_name` and every entry in
    /// `skill_names` against the shared [`Self::known_names`] corpus, extended with every other
    /// installed plugin's own *name* (not just its skill names) — FR-001 reads "bundled skill
    /// names … and currently-installed **plugin**/skill names", so a plugin-name-vs-plugin-name
    /// squat (e.g. installing `acme-tool` to impersonate an installed `acme-tools` whose skills
    /// happen to be named differently) must be caught too. Plugin names are deliberately NOT
    /// added to [`Self::known_names`] itself — that corpus also feeds
    /// [`Self::check_skill_conflicts`]'s exact-match check, and an incoming *skill* exactly
    /// matching another plugin's *name* is not a skill-name conflict.
    #[tracing::instrument(name = "plugins.security.reputation_check", skip_all, fields(plugin = %plugin_name))]
    pub(crate) fn check_reputation(
        &self,
        plugin_name: &str,
        skill_names: &[String],
        this_plugin: &str,
    ) -> Vec<ReputationWarning> {
        let Some(source) = &self.reputation else {
            return Vec::new();
        };
        let mut corpus = self.known_names(this_plugin);
        corpus.extend(
            self.list_installed()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|plugin| {
                    (plugin.name.as_str() != this_plugin).then(|| {
                        (
                            plugin.name.to_string(),
                            MatchedSource::Plugin(plugin.name.to_string()),
                        )
                    })
                }),
        );
        let mut warnings = source.check(plugin_name, &corpus);
        for skill in skill_names {
            warnings.extend(source.check(skill, &corpus));
        }
        warnings
    }
}

/// Validate that `url` uses the `https` scheme for ephemeral plugin loading.
///
/// Only `https://` is accepted. `http://` and any other scheme are rejected to prevent
/// MITM attacks against session-scoped plugin archives (security invariant INV-EPH-1).
///
/// # Errors
///
/// Returns [`PluginError::InsecureUrl`] when the URL is `http://` or any other non-HTTPS scheme.
/// Returns [`PluginError::InvalidSource`] when the URL is unparseable.
#[must_use = "validation result must be checked"]
pub fn validate_url_scheme_ephemeral(url: &str) -> Result<(), PluginError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| PluginError::InvalidSource {
        path: url.to_owned(),
        reason: "URL is not valid".to_owned(),
    })?;
    if parsed.scheme() != "https" {
        return Err(PluginError::InsecureUrl(url.to_owned()));
    }
    Ok(())
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
pub(crate) fn check_allowed_commands_overlay_effect(
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
pub(crate) fn validate_mcp_commands(
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
pub(crate) fn scan_skill_entries(
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

/// Extract a `.tar.gz` archive with tar-slip protection.
///
/// Only gzip-compressed tar archives are supported (gzip magic `0x1f 0x8b`). Beyond plain
/// extraction, this function rejects:
/// - Absolute paths inside the archive.
/// - Entries with `..` path components.
/// - Symbolic link entries (prevent symlink-based traversal).
///
/// # Errors
///
/// Returns [`PluginError::InvalidSource`] when the archive format is unrecognised, extraction
/// fails, or a dangerous entry is found.
pub(crate) fn extract_archive_safe(
    bytes: &[u8],
    dest: &Path,
    url: &str,
) -> Result<(), PluginError> {
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
