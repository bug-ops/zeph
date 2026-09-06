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

/// Insert `max_spawns_per_session = 100` under `[agents]` when `enabled = true` and the key is
/// absent (issue #6545).
///
/// Mirrors [`migrate_agents_delegation_mode`] exactly: `#[serde(default = "...")]` already
/// makes the field's absence safe on load (see `SubAgentConfig::max_spawns_per_session`'s doc
/// comment), so this insertion is behaviorally a no-op — its purpose is discoverability, so an
/// operator who already opted into `enabled = true` sees the active session-wide spawn cap in
/// their config file rather than having it resolved silently.
///
/// No-op when `[agents]` is absent, not active (only commented-out), `enabled` is not `true`,
/// or `max_spawns_per_session` is already present.
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_agents_max_spawns_per_session(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if toml_src.contains("max_spawns_per_session") || !section_header_present(toml_src, "agents") {
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

    agents_table.insert("max_spawns_per_session", toml_edit::value(100_i64));

    Ok(MigrationResult {
        output: doc.to_string(),
        changed_count: 1,
        sections_changed: vec!["agents.max_spawns_per_session".to_owned()],
    })
}

/// Add a `[agents.peer_messaging]` advisory block to an existing active `[agents]` table
/// (spec `046-subagent-peer-messaging-parity`, issue #5871).
///
/// `#[serde(default)]` on every field of `PeerMessagingConfig` already makes the section's
/// absence safe on load, so this insertion is purely for discoverability — an operator who
/// already has `[agents]` active sees the new inter-sub-agent messaging knobs in their config
/// file rather than having them resolved silently.
///
/// No-op when `[agents]` is absent or not active (only commented-out), or
/// `[agents.peer_messaging]` is already present (active or as a prior advisory comment).
///
/// # Errors
///
/// Returns `MigrateError::Parse` if the TOML cannot be parsed.
pub fn migrate_agents_peer_messaging_config(
    toml_src: &str,
) -> Result<MigrationResult, MigrateError> {
    if section_header_present(toml_src, "agents.peer_messaging")
        || toml_src.contains("# [agents.peer_messaging]")
        || !section_header_present(toml_src, "agents")
    {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let doc = toml_src.parse::<toml_edit::DocumentMut>()?;
    if !doc.contains_key("agents") {
        return Ok(MigrationResult {
            output: toml_src.to_owned(),
            changed_count: 0,
            sections_changed: Vec::new(),
        });
    }

    let comment = "\n# Live inter-sub-agent messaging (spec 046-subagent-peer-messaging-parity, #5871).\n\
         # Lets a spawner, coordinator, or sibling sub-agent send an addressed message to a\n\
         # currently-running sub-agent's own mailbox, without it terminating or being respawned.\n\
         # Enabled by default; each addressable agent's mailbox is bounded and denies delivery\n\
         # across spawn-tree roots.\n\
         # [agents.peer_messaging]\n\
         # enabled = true\n\
         # mailbox_capacity = 32   # bounded queue per agent\n\
         # max_body_bytes = 8192  # per-message cap\n\
         # max_wait_ms = 30000    # ceiling for check_messages(wait_ms)\n";
    let raw = doc.to_string();
    let output = format!("{raw}{comment}");

    Ok(MigrationResult {
        output,
        changed_count: 1,
        sections_changed: vec!["agents.peer_messaging".to_owned()],
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

    #[test]
    fn max_spawns_inserts_100_when_enabled_and_key_absent() {
        let src = "[agents]\nenabled = true\nmax_concurrent = 3\n";
        let result = migrate_agents_max_spawns_per_session(src).expect("migrate");
        assert_eq!(result.changed_count, 1);
        assert!(result.output.contains("max_spawns_per_session = 100"));
    }

    #[test]
    fn max_spawns_noop_when_disabled() {
        let src = "[agents]\nenabled = false\n";
        let result = migrate_agents_max_spawns_per_session(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert!(!result.output.contains("max_spawns_per_session"));
    }

    #[test]
    fn max_spawns_noop_when_key_already_present() {
        let src = "[agents]\nenabled = true\nmax_spawns_per_session = 50\n";
        let result = migrate_agents_max_spawns_per_session(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn max_spawns_noop_when_section_absent() {
        let src = "[llm]\nprovider = \"claude\"\n";
        let result = migrate_agents_max_spawns_per_session(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn max_spawns_noop_when_section_only_commented_out() {
        let src = "# [agents]\n# enabled = true\n";
        let result = migrate_agents_max_spawns_per_session(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn max_spawns_idempotent() {
        let src = "[agents]\nenabled = true\n";
        let once = migrate_agents_max_spawns_per_session(src).expect("migrate");
        let twice = migrate_agents_max_spawns_per_session(&once.output).expect("migrate");
        assert_eq!(twice.changed_count, 0);
        assert_eq!(twice.output, once.output);
    }

    #[test]
    fn peer_messaging_inserts_advisory_block_when_agents_active() {
        let src = "[agents]\nenabled = true\n";
        let result = migrate_agents_peer_messaging_config(src).expect("migrate");
        assert_eq!(result.changed_count, 1);
        assert!(result.output.contains("# [agents.peer_messaging]"));
        assert!(result.output.contains("# max_wait_ms = 30000"));
    }

    #[test]
    fn peer_messaging_noop_when_section_absent() {
        let src = "[llm]\nprovider = \"claude\"\n";
        let result = migrate_agents_peer_messaging_config(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn peer_messaging_noop_when_section_only_commented_out() {
        let src = "# [agents]\n# enabled = true\n";
        let result = migrate_agents_peer_messaging_config(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn peer_messaging_noop_when_already_present() {
        let src = "[agents]\nenabled = true\n\n[agents.peer_messaging]\nenabled = false\n";
        let result = migrate_agents_peer_messaging_config(src).expect("migrate");
        assert_eq!(result.changed_count, 0);
        assert_eq!(result.output, src);
    }

    #[test]
    fn peer_messaging_idempotent() {
        let src = "[agents]\nenabled = true\n";
        let once = migrate_agents_peer_messaging_config(src).expect("migrate");
        let twice = migrate_agents_peer_messaging_config(&once.output).expect("migrate");
        assert_eq!(twice.changed_count, 0);
        assert_eq!(twice.output, once.output);
    }
}
