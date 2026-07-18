// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Migration for the `[plugins.reputation]` section (spec-043, #5864).

use super::{MigrateError, MigrationResult, section_header_present};

/// Add a commented-out `[plugins.reputation]` advisory block if absent (spec-043, #5864).
///
/// All `ReputationConfig` fields have `#[serde(default)]` so existing configs without this
/// section parse fine (the typosquat check defaults to enabled/advisory). This step is
/// discoverability-only, mirroring the sibling `[deep_link]`/`[knowledge]` advisory blocks.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_plugins_reputation_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "plugins.reputation")
        || toml_src.contains("# [plugins.reputation]")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [plugins.reputation] — install-time name-similarity/typosquat advisory check (spec-043, #5864).\n\
         # [plugins.reputation]\n\
         # enabled = true               # local, zero-network Levenshtein-similarity check at plugin install\n\
         # similarity_threshold = 0.65  # [0,1]; higher = require closer match to warn\n\
         # min_name_len = 3             # skip names shorter than this\n\
         # enforcement = \"warn\"         # \"warn\" (advisory, default) | \"block\" (opt-in hard gate)\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["plugins.reputation".to_owned()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_plugins_reputation_config_appends_advisory_block() {
        let base = "[agent]\nname = \"zeph\"\n";
        let result = migrate_plugins_reputation_config(base).unwrap();
        assert_eq!(result.changed_count, 1);
        assert!(result.output.contains("# [plugins.reputation]"));
        assert!(result.output.contains("# enabled = true"));
        assert!(result.output.contains("# similarity_threshold = 0.65"));
        assert!(result.output.contains("# enforcement = \"warn\""));
    }

    #[test]
    fn migrate_plugins_reputation_config_idempotent_on_commented_output() {
        let base = "[agent]\nname = \"zeph\"\n";
        let first = migrate_plugins_reputation_config(base).unwrap();
        let second = migrate_plugins_reputation_config(&first.output).unwrap();
        assert_eq!(second.changed_count, 0, "second run must not double-append");
        assert_eq!(second.output, first.output);
    }

    #[test]
    fn migrate_plugins_reputation_config_noop_when_active_section_present() {
        let base = "[plugins.reputation]\nenabled = false\n";
        let result = migrate_plugins_reputation_config(base).unwrap();
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, base);
    }
}
