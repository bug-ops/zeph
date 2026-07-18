// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Migration for the `[integrity]` section (issue #6449, vault-anchor downgrade-resistance).

use super::{MigrateError, MigrationResult, section_header_present};

/// Add a commented-out `[integrity]` advisory block if absent (issue #6449).
///
/// All `IntegrityConfig` fields have `#[serde(default)]` so existing configs without this
/// section parse fine (anchor defaults to `"vault"`, closing the whole-file-strip downgrade gap
/// automatically whenever the age vault + a history-integrity key are available). This step is
/// discoverability-only, mirroring the sibling `[deep_link]`/`[knowledge]`/`[plugins.reputation]`
/// advisory blocks.
///
/// # Errors
///
/// Infallible in practice; `Result` matches the migration convention.
pub fn migrate_integrity_config(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "integrity") || toml_src.contains("# [integrity]") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# [integrity] — vault-anchor downgrade-resistance for transcript/session \
         history (issue #6449).\n\
         # [integrity]\n\
         # anchor = \"vault\"             # \"vault\" (default) | \"none\" (chain-only, #6453-level protection)\n\
         # max_session_anchors = 512    # LRU cap on retained session anchors before the oldest degrade to chain-only\n";
    let output = format!("{toml_src}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["integrity".to_owned()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_integrity_config_appends_advisory_block() {
        let base = "[agent]\nname = \"zeph\"\n";
        let result = migrate_integrity_config(base).unwrap();
        assert_eq!(result.changed_count, 1);
        assert!(result.output.contains("# [integrity]"));
        assert!(result.output.contains("# anchor = \"vault\""));
        assert!(result.output.contains("# max_session_anchors = 512"));
    }

    #[test]
    fn migrate_integrity_config_idempotent_on_commented_output() {
        let base = "[agent]\nname = \"zeph\"\n";
        let first = migrate_integrity_config(base).unwrap();
        let second = migrate_integrity_config(&first.output).unwrap();
        assert_eq!(second.changed_count, 0, "second run must not double-append");
        assert_eq!(second.output, first.output);
    }

    #[test]
    fn migrate_integrity_config_noop_when_active_section_present() {
        let base = "[integrity]\nanchor = \"none\"\n";
        let result = migrate_integrity_config(base).unwrap();
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, base);
    }
}
