// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin tighten-only config overlay merge.
//!
//! Scans every `<plugin>/.plugin.toml` under the plugins directory, resolves the
//! union / intersection / max of the safelisted overlay keys, and mutates a [`Config`]
//! in place.
//!
//! # Invariants
//!
//! - `tools.shell.blocked_commands` grows monotonically (union across all plugins).
//! - `tools.shell.allowed_commands` never grows beyond the base — an empty base stays
//!   empty (plugins cannot re-enable `DEFAULT_BLOCKED` commands). A non-empty base is
//!   narrowed to the intersection with every plugin's list.
//! - `skills.disambiguation_threshold` only rises (max across all plugins).

use std::collections::BTreeSet;
use std::path::Path;

use zeph_config::Config;

use crate::PluginError;
use crate::manager::{validate_overlay_keys, validate_plugin_name};
use crate::manifest::PluginManifest;

/// Summary of the overlay applied to a [`Config`] by [`apply_plugin_config_overlays`].
///
/// Returned so callers (bootstrap, TUI, `zeph plugin list`) can surface which plugins
/// contributed and which were skipped without re-parsing the manifest files.
#[derive(Debug, Clone, Default)]
pub struct ResolvedOverlay {
    /// Union of all plugin `tools.blocked_commands` lists, sorted and de-duplicated.
    pub blocked_commands_add: Vec<String>,

    /// Accumulated intersection of `allowed_commands` across plugins that supplied it.
    /// `None` = no plugin mentioned this key → merge step is a no-op for this field.
    /// Used internally by `apply_resolved`; also available for diagnostics.
    pub allowed_commands_intersect_accum: Option<BTreeSet<String>>,

    /// `max` across all plugins that supplied `skills.disambiguation_threshold`.
    /// `None` means no plugin supplied this key.
    pub disambiguation_threshold_max: Option<f32>,

    /// Names of plugins whose overlay contributed at least one safelisted value.
    /// Sorted ascending (deterministic — follows `sort_by_key(file_name)` iteration).
    pub source_plugins: Vec<String>,

    /// Plugins that were skipped. Each entry: `"<name>: <reason>"`.
    pub skipped_plugins: Vec<String>,
}

/// Apply tighten-only config overlays from every installed plugin to `config`.
///
/// Reads `<plugins_dir>/<plugin>/.plugin.toml` for each subdirectory, validates the
/// safelisted keys, and merges: `blocked_commands` (union), `allowed_commands` (intersection,
/// base-gated), `disambiguation_threshold` (max).
///
/// Returns [`ResolvedOverlay`] describing what was applied and what was skipped.
/// A missing `plugins_dir` is silently treated as an empty directory.
///
/// # Errors
///
/// Returns [`PluginError::Io`] only when `plugins_dir` exists but cannot be enumerated.
/// Per-plugin failures are recorded in [`ResolvedOverlay::skipped_plugins`] and do not
/// abort the merge.
pub fn apply_plugin_config_overlays(
    config: &mut Config,
    plugins_dir: &Path,
) -> Result<ResolvedOverlay, PluginError> {
    let resolved = resolve_overlays(plugins_dir)?;
    apply_resolved(config, &resolved);
    Ok(resolved)
}

fn resolve_overlays(plugins_dir: &Path) -> Result<ResolvedOverlay, PluginError> {
    let mut out = ResolvedOverlay::default();

    if !plugins_dir.exists() {
        return Ok(out);
    }

    // M1: sort entries deterministically so log ordering and `source_plugins` are
    // platform-independent (ext4 inode order, APFS insertion order, etc. vary).
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(plugins_dir)
        .map_err(|e| PluginError::Io {
            path: plugins_dir.to_path_buf(),
            source: e,
        })?
        .flatten()
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut blocked_set: BTreeSet<String> = BTreeSet::new();
    let mut allowed_accum: Option<BTreeSet<String>> = None;
    let mut threshold: Option<f32> = None;

    for entry in entries {
        process_plugin_entry(
            &entry.path(),
            &mut out,
            &mut blocked_set,
            &mut allowed_accum,
            &mut threshold,
        );
    }

    out.blocked_commands_add = blocked_set.into_iter().collect();
    out.allowed_commands_intersect_accum = allowed_accum;
    out.disambiguation_threshold_max = threshold;
    Ok(out)
}

fn process_plugin_entry(
    path: &std::path::Path,
    out: &mut ResolvedOverlay,
    blocked_set: &mut BTreeSet<String>,
    allowed_accum: &mut Option<BTreeSet<String>>,
    threshold: &mut Option<f32>,
) {
    // E8: reject symlinked subdirectories; only real dirs installed by
    // PluginManager::add may contribute overlays.
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "stat failed; skipping");
            return;
        }
    };
    if !md.is_dir() || md.file_type().is_symlink() {
        return;
    }

    let manifest_path = path.join(".plugin.toml");
    let bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(path = %manifest_path.display(), kind = ?e.kind(), "cannot read .plugin.toml; skipping");
            return;
        }
    };

    let Ok(text) = String::from_utf8(bytes) else {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        out.skipped_plugins
            .push(format!("{name}: .plugin.toml is not valid UTF-8"));
        return;
    };
    let manifest: PluginManifest = {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        match toml::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                out.skipped_plugins
                    .push(format!("{name}: malformed .plugin.toml ({e})"));
                return;
            }
        }
    };

    // Security: validate plugin name before using it in logs/data structures to prevent
    // log injection via a post-install-tampered manifest.
    if let Err(e) = validate_plugin_name(&manifest.plugin.name) {
        tracing::warn!(
            path = %path.display(),
            "plugin overlay skipped: invalid plugin name ({e})"
        );
        return;
    }

    // M2: re-run install-time safelist check as defence-in-depth against post-install tampering.
    if let Err(e) = validate_overlay_keys(&manifest.config) {
        out.skipped_plugins.push(format!(
            "{}: overlay rejected by safelist ({e})",
            manifest.plugin.name
        ));
        return;
    }

    let contributed = merge_manifest_overlay(&manifest, out, blocked_set, allowed_accum, threshold);
    if contributed {
        out.source_plugins.push(manifest.plugin.name);
    }
}

fn merge_manifest_overlay(
    manifest: &PluginManifest,
    out: &mut ResolvedOverlay,
    blocked_set: &mut BTreeSet<String>,
    allowed_accum: &mut Option<BTreeSet<String>>,
    threshold: &mut Option<f32>,
) -> bool {
    let Some(cfg_table) = manifest.config.as_table() else {
        return false;
    };
    let mut contributed = false;

    if let Some(tools) = cfg_table.get("tools").and_then(toml::Value::as_table) {
        if let Some(arr) = tools
            .get("blocked_commands")
            .and_then(toml::Value::as_array)
        {
            for v in arr {
                if let Some(s) = v.as_str() {
                    blocked_set.insert(s.to_owned());
                    contributed = true;
                }
            }
        }
        if let Some(arr) = tools
            .get("allowed_commands")
            .and_then(toml::Value::as_array)
        {
            let plugin_allowed: BTreeSet<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            *allowed_accum = Some(match allowed_accum.take() {
                None => plugin_allowed,
                Some(prev) => prev.intersection(&plugin_allowed).cloned().collect(),
            });
            contributed = true;
        }
    }

    if let Some(skills) = cfg_table.get("skills").and_then(toml::Value::as_table)
        && let Some(v) = skills.get("disambiguation_threshold")
    {
        // M3: accept both float literals (`0.5`) and integer literals (`0`, `1`).
        #[allow(clippy::cast_precision_loss)]
        let raw = v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
        match raw {
            Some(f) if (0.0_f64..=1.0_f64).contains(&f) => {
                #[allow(clippy::cast_possible_truncation)]
                let f32_val = f as f32;
                *threshold = Some(threshold.map_or(f32_val, |cur: f32| cur.max(f32_val)));
                contributed = true;
            }
            Some(f) => {
                out.skipped_plugins.push(format!(
                    "{}: disambiguation_threshold={f} out of [0,1]; ignored",
                    manifest.plugin.name
                ));
            }
            None => {
                out.skipped_plugins.push(format!(
                    "{}: disambiguation_threshold has non-numeric value; ignored",
                    manifest.plugin.name
                ));
            }
        }
    }

    contributed
}

fn apply_resolved(config: &mut Config, r: &ResolvedOverlay) {
    // blocked_commands: base ∪ overlay (tighten — more commands blocked).
    let mut seen: BTreeSet<String> = config
        .tools
        .shell
        .blocked_commands
        .iter()
        .cloned()
        .collect();
    for cmd in &r.blocked_commands_add {
        if seen.insert(cmd.clone()) {
            config.tools.shell.blocked_commands.push(cmd.clone());
        }
    }

    // allowed_commands: base ∩ overlay, BUT empty base stays empty.
    //
    // B2: `allowed_commands` *subtracts* from DEFAULT_BLOCKED in ShellExecutor::new.
    // Adopting a non-empty plugin list when the base is empty would loosen
    // restrictions by re-enabling DEFAULT_BLOCKED commands. Tighten-only requires
    // that only a non-empty base may be further narrowed.
    if let Some(ref plugin_allowed) = r.allowed_commands_intersect_accum {
        if config.tools.shell.allowed_commands.is_empty() {
            tracing::debug!(
                "plugin overlay supplied allowed_commands but base is empty; \
                 ignoring (tighten-only — plugins cannot widen the allowlist)"
            );
        } else {
            let base: BTreeSet<String> = config
                .tools
                .shell
                .allowed_commands
                .iter()
                .cloned()
                .collect();
            let narrowed: Vec<String> = base.intersection(plugin_allowed).cloned().collect();
            let narrowed_count = narrowed.len();
            let prev_count = config.tools.shell.allowed_commands.len();
            config.tools.shell.allowed_commands = narrowed;
            if narrowed_count < prev_count {
                tracing::info!(
                    from = prev_count,
                    to = narrowed_count,
                    "plugin overlay narrowed tools.shell.allowed_commands"
                );
            }
        }
    }

    // disambiguation_threshold: max(base, overlay).
    if let Some(t) = r.disambiguation_threshold_max
        && t > config.skills.disambiguation_threshold
    {
        tracing::info!(
            from = config.skills.disambiguation_threshold,
            to = t,
            "plugin overlay raised skills.disambiguation_threshold"
        );
        config.skills.disambiguation_threshold = t;
    }

    if !r.source_plugins.is_empty() {
        tracing::info!(
            plugins = ?r.source_plugins,
            blocked_added = r.blocked_commands_add.len(),
            threshold = ?r.disambiguation_threshold_max,
            "applied plugin config overlays"
        );
    }
    for s in &r.skipped_plugins {
        tracing::warn!("plugin overlay skipped: {s}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use zeph_config::Config;

    fn write_plugin_overlay(plugins_dir: &Path, name: &str, overlay_toml: &str) {
        let plugin_dir = plugins_dir.join(name);
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join(".plugin.toml"),
            format!("[plugin]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n{overlay_toml}"),
        )
        .unwrap();
    }

    fn base_config() -> Config {
        Config::default()
    }

    // 1. empty_plugins_dir_is_noop
    #[test]
    fn empty_plugins_dir_is_noop() {
        let dir = TempDir::new().unwrap();
        let absent = dir.path().join("no-such-dir");
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, &absent).unwrap();
        assert!(overlay.source_plugins.is_empty());
        assert!(overlay.skipped_plugins.is_empty());
        assert!(cfg.tools.shell.blocked_commands.is_empty());
    }

    // 2. plugins_dir_without_manifests_is_noop
    #[test]
    fn plugins_dir_without_manifests_is_noop() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("myplugin")).unwrap();
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(overlay.source_plugins.is_empty());
        assert!(cfg.tools.shell.blocked_commands.is_empty());
    }

    // 3. single_plugin_blocked_commands_union
    #[test]
    fn single_plugin_blocked_commands_union() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "hardening",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        let mut cfg = base_config();
        cfg.tools.shell.blocked_commands = vec!["rm -rf".to_owned()];
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(
            cfg.tools
                .shell
                .blocked_commands
                .contains(&"rm -rf".to_owned())
        );
        assert!(
            cfg.tools
                .shell
                .blocked_commands
                .contains(&"sudo".to_owned())
        );
    }

    // 4. multi_plugin_blocked_commands_dedup
    #[test]
    fn multi_plugin_blocked_commands_dedup() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "p1",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        write_plugin_overlay(
            dir.path(),
            "p2",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        let mut cfg = base_config();
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        let count = cfg
            .tools
            .shell
            .blocked_commands
            .iter()
            .filter(|c| c.as_str() == "sudo")
            .count();
        assert_eq!(count, 1);
    }

    // 5. non_empty_base_allowed_commands_narrowed
    #[test]
    fn non_empty_base_allowed_commands_narrowed() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "narrow",
            "[config.tools]\nallowed_commands = [\"a\", \"b\"]",
        );
        let mut cfg = base_config();
        cfg.tools.shell.allowed_commands = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        let mut result = cfg.tools.shell.allowed_commands.clone();
        result.sort();
        assert_eq!(result, vec!["a".to_owned(), "b".to_owned()]);
    }

    // 6. multi_plugin_allowed_commands_intersection
    #[test]
    fn multi_plugin_allowed_commands_intersection() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "p1",
            "[config.tools]\nallowed_commands = [\"a\", \"b\"]",
        );
        write_plugin_overlay(
            dir.path(),
            "p2",
            "[config.tools]\nallowed_commands = [\"b\", \"c\"]",
        );
        let mut cfg = base_config();
        cfg.tools.shell.allowed_commands = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert_eq!(cfg.tools.shell.allowed_commands, vec!["b".to_owned()]);
    }

    // 7. empty_base_allowed_commands_overlay_ignored (B2 regression)
    #[test]
    fn empty_base_allowed_commands_overlay_ignored() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "widener",
            "[config.tools]\nallowed_commands = [\"curl\"]",
        );
        let mut cfg = base_config();
        assert!(cfg.tools.shell.allowed_commands.is_empty());
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        // Base was empty — plugin must NOT have widened it.
        assert!(cfg.tools.shell.allowed_commands.is_empty());
    }

    // 8. disambiguation_threshold_max_wins
    #[test]
    fn disambiguation_threshold_max_wins() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "strict",
            "[config.skills]\ndisambiguation_threshold = 0.25",
        );
        let mut cfg = base_config();
        cfg.skills.disambiguation_threshold = 0.20;
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!((cfg.skills.disambiguation_threshold - 0.25_f32).abs() < 1e-5);
    }

    // 9. disambiguation_threshold_lower_ignored
    #[test]
    fn disambiguation_threshold_lower_ignored() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "loose",
            "[config.skills]\ndisambiguation_threshold = 0.20",
        );
        let mut cfg = base_config();
        cfg.skills.disambiguation_threshold = 0.30;
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!((cfg.skills.disambiguation_threshold - 0.30_f32).abs() < 1e-5);
    }

    // 10. threshold_out_of_range_skipped_with_warning
    #[test]
    fn threshold_out_of_range_skipped_with_warning() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "bad",
            "[config.skills]\ndisambiguation_threshold = 1.5",
        );
        let mut cfg = base_config();
        let orig = cfg.skills.disambiguation_threshold;
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!((cfg.skills.disambiguation_threshold - orig).abs() < 1e-5);
        assert!(
            overlay
                .skipped_plugins
                .iter()
                .any(|s| s.contains("bad") && s.contains("1.5"))
        );
    }

    // 11. threshold_boundary_one_accepted
    #[test]
    fn threshold_boundary_one_accepted() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "max-strict",
            "[config.skills]\ndisambiguation_threshold = 1.0",
        );
        let mut cfg = base_config();
        cfg.skills.disambiguation_threshold = 0.5;
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!((cfg.skills.disambiguation_threshold - 1.0_f32).abs() < 1e-5);
    }

    // 12. threshold_integer_literal_accepted (M3)
    #[test]
    fn threshold_integer_literal_accepted() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "int-thresh",
            "[config.skills]\ndisambiguation_threshold = 0",
        );
        let mut cfg = base_config();
        cfg.skills.disambiguation_threshold = 0.5;
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        // 0 < 0.5 so max keeps 0.5; but the key must parse without error (no skipped_plugins).
        assert!(
            overlay.skipped_plugins.is_empty(),
            "unexpected skips: {:?}",
            overlay.skipped_plugins
        );
    }

    // 13. malformed_manifest_skipped
    #[test]
    fn malformed_manifest_skipped() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("broken");
        fs::create_dir(&plugin_dir).unwrap();
        fs::write(plugin_dir.join(".plugin.toml"), b"not valid toml ][[[").unwrap();
        write_plugin_overlay(
            dir.path(),
            "good",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(overlay.skipped_plugins.iter().any(|s| s.contains("broken")));
        assert!(
            cfg.tools
                .shell
                .blocked_commands
                .contains(&"sudo".to_owned())
        );
    }

    // 14. unsafelisted_overlay_key_skipped
    #[test]
    fn unsafelisted_overlay_key_skipped() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(dir.path(), "tampered", "[config.llm]\nmodel = \"evil\"");
        write_plugin_overlay(
            dir.path(),
            "good",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(
            overlay
                .skipped_plugins
                .iter()
                .any(|s| s.contains("tampered"))
        );
        assert!(
            cfg.tools
                .shell
                .blocked_commands
                .contains(&"sudo".to_owned())
        );
    }

    // 15. symlinked_plugin_dir_ignored (E8)
    #[cfg(unix)]
    #[test]
    fn symlinked_plugin_dir_ignored() {
        let dir = TempDir::new().unwrap();
        let real_dir = TempDir::new().unwrap();
        let plugin_in_real = real_dir.path().join("evil");
        fs::create_dir(&plugin_in_real).unwrap();
        fs::write(
            plugin_in_real.join(".plugin.toml"),
            "[plugin]\nname = \"evil\"\nversion = \"0.1.0\"\n[config.tools]\nblocked_commands = [\"curl\"]",
        )
        .unwrap();
        // Symlink: plugins_dir/evil -> real_dir/evil
        std::os::unix::fs::symlink(&plugin_in_real, dir.path().join("evil")).unwrap();
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(overlay.source_plugins.is_empty());
        assert!(cfg.tools.shell.blocked_commands.is_empty());
    }

    // 16. idempotent_merge
    #[test]
    fn idempotent_merge() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "idem",
            "[config.tools]\nblocked_commands = [\"sudo\"]",
        );
        let mut cfg = base_config();
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        let snap1 = cfg.tools.shell.blocked_commands.clone();
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        let snap2 = cfg.tools.shell.blocked_commands.clone();
        assert_eq!(snap1, snap2);
    }

    // 17. iteration_order_deterministic (M1)
    #[test]
    fn iteration_order_deterministic() {
        let dir = TempDir::new().unwrap();
        // Create in reverse alphabetical order; iteration must still be sorted.
        write_plugin_overlay(
            dir.path(),
            "z-plugin",
            "[config.tools]\nblocked_commands = [\"z\"]",
        );
        write_plugin_overlay(
            dir.path(),
            "a-plugin",
            "[config.tools]\nblocked_commands = [\"a\"]",
        );
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert_eq!(overlay.source_plugins, vec!["a-plugin", "z-plugin"]);
    }

    // 18. plugin_blocked_wins_over_base_allowed (E4)
    #[test]
    fn plugin_blocked_wins_over_base_allowed() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(
            dir.path(),
            "hardening",
            "[config.tools]\nblocked_commands = [\"curl\"]",
        );
        let mut cfg = base_config();
        cfg.tools.shell.allowed_commands = vec!["curl".to_owned()];
        apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(
            cfg.tools
                .shell
                .blocked_commands
                .contains(&"curl".to_owned())
        );
    }

    // 19. tampered_overlay_skipped_but_source_plugins_still_has_good
    #[test]
    fn tampered_overlay_skipped_but_good_plugin_still_loaded() {
        let dir = TempDir::new().unwrap();
        write_plugin_overlay(dir.path(), "evil", "[config.llm]\nmodel = \"x\"");
        write_plugin_overlay(
            dir.path(),
            "good",
            "[config.skills]\ndisambiguation_threshold = 0.5",
        );
        let mut cfg = base_config();
        cfg.skills.disambiguation_threshold = 0.1;
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        assert!(overlay.source_plugins.contains(&"good".to_owned()));
        assert!(!overlay.source_plugins.contains(&"evil".to_owned()));
        assert!((cfg.skills.disambiguation_threshold - 0.5_f32).abs() < 1e-5);
    }

    // T22. reload_warns_on_shell_overlay_divergence (divergence detection logic)
    //
    // Simulates the startup-vs-reload comparison: a startup snapshot with no plugin-blocked
    // commands, then a reload that merges a plugin adding "curl" to blocked_commands. The
    // resulting full blocked set differs from the startup snapshot → divergence detected.
    #[test]
    fn reload_warns_on_shell_overlay_divergence() {
        let dir = TempDir::new().unwrap();

        // --- Startup: no plugins yet ---
        let mut startup_cfg = base_config();
        apply_plugin_config_overlays(&mut startup_cfg, dir.path()).unwrap();
        let mut startup_blocked = startup_cfg.tools.shell.blocked_commands.clone();
        startup_blocked.sort();
        let mut startup_allowed = startup_cfg.tools.shell.allowed_commands.clone();
        startup_allowed.sort();

        // --- A plugin is installed after startup ---
        write_plugin_overlay(
            dir.path(),
            "hardening",
            "[config.tools]\nblocked_commands = [\"curl\"]",
        );

        // --- Reload: re-apply overlays to a fresh config ---
        let mut reload_cfg = base_config();
        apply_plugin_config_overlays(&mut reload_cfg, dir.path()).unwrap();
        let mut reload_blocked = reload_cfg.tools.shell.blocked_commands.clone();
        reload_blocked.sort();
        let mut reload_allowed = reload_cfg.tools.shell.allowed_commands.clone();
        reload_allowed.sort();

        // The blocked set changed — divergence must be detectable by comparison.
        assert_ne!(
            startup_blocked, reload_blocked,
            "reload should produce a different blocked_commands set after plugin install"
        );
        assert!(
            reload_blocked.contains(&"curl".to_owned()),
            "reload config must contain plugin-added blocked command"
        );
        // allowed_commands unchanged (base was empty, plugin list ignored).
        assert_eq!(startup_allowed, reload_allowed);
    }

    // 20. invalid_plugin_name_in_manifest_skipped (Security M-2)
    #[test]
    fn invalid_plugin_name_in_manifest_skipped() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("bad-name-dir");
        fs::create_dir(&plugin_dir).unwrap();
        // Plugin name contains uppercase — invalid per [a-z0-9][a-z0-9-]*.
        fs::write(
            plugin_dir.join(".plugin.toml"),
            "[plugin]\nname = \"INVALID\"\nversion = \"0.1.0\"\n[config.tools]\nblocked_commands = [\"evil\"]",
        )
        .unwrap();
        let mut cfg = base_config();
        let overlay = apply_plugin_config_overlays(&mut cfg, dir.path()).unwrap();
        // Plugin skipped due to invalid name — no commands added.
        assert!(cfg.tools.shell.blocked_commands.is_empty());
        // source_plugins must NOT contain the untrusted name.
        assert!(overlay.source_plugins.is_empty());
    }
}
