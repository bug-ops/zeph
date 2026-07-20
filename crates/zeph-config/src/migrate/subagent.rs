// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent delegation-mode config migration step.
//!
//! Extracted as its own module (rather than folded into `tools.rs`, which owns the
//! `[agent]` — singular, main-agent — migrations) because `[agents]` (plural,
//! `SubAgentConfig`) is a distinct TOML section.

use super::{MigrateError, MigrationResult, section_header_present};

/// Insert `delegation_mode = "proactive"` under `[agents]` when `enabled = true` and the key
/// is absent (spec `042-subagent-delegation-mode-parity` FR-010, issue #5857).
///
/// Unlike most migration steps in this module, which surface a *commented-out* example because
/// `#[serde(default)]` already makes the field's absence harmless, this step inserts a real,
/// active value. `DelegationMode::default()` already resolves to `Proactive` on load, so the
/// insertion is behaviorally a no-op — its purpose is discoverability: an operator who already
/// opted into `enabled = true` should see the explicit autonomy setting in their config file
/// rather than have it resolved silently, since narrowing or widening this trust boundary later
/// is a security-relevant change (NFR-001).
///
/// No-op when `[agents]` is absent, not active (only commented-out), `enabled` is not `true`,
/// or `delegation_mode` is already present.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_agents_delegation_mode(toml_src: &str) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("delegation_mode") || !section_header_present(toml_src, "agents") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let mut doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    let Some(agents_table) = doc
        .get_mut("agents")
        .and_then(toml_edit::Item::as_table_mut)
    else {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    };

    let enabled = agents_table
        .get("enabled")
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_bool)
        .unwrap_or(false);

    if !enabled {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    agents_table.insert("delegation_mode", toml_edit::value("proactive"));

    Ok(MigrationResult {
        output: doc.to_string(),
        changed_count: 1,
        sections_changed: vec!["agents.delegation_mode".to_owned()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_proactive_when_enabled_and_key_absent() {
        let src = "[agents]\nenabled = true\nmax_concurrent = 3\n";
        let result = migrate_agents_delegation_mode(src).expect("migrate");
        assert_eq!(result.changed_count, 1);
        assert!(result.output.contains("delegation_mode = \"proactive\""));
    }

    #[test]
    fn noop_when_disabled() {
        let src = "[agents]\nenabled = false\n";
        let result = migrate_agents_delegation_mode(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert!(!result.output.contains("delegation_mode"));
    }

    #[test]
    fn noop_when_key_already_present() {
        let src = "[agents]\nenabled = true\ndelegation_mode = \"explicit_request_only\"\n";
        let result = migrate_agents_delegation_mode(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn noop_when_section_absent() {
        let src = "[llm]\nprovider = \"claude\"\n";
        let result = migrate_agents_delegation_mode(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn noop_when_section_only_commented_out() {
        let src = "# [agents]\n# enabled = true\n";
        let result = migrate_agents_delegation_mode(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn idempotent() {
        let src = "[agents]\nenabled = true\n";
        let once = migrate_agents_delegation_mode(src).expect("migrate");
        let twice = migrate_agents_delegation_mode(&once.output).expect("migrate");
        assert_eq!(twice.changed_count, 0);
        assert_eq!(twice.output, once.output);
    }
}
