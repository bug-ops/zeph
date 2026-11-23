// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Acceptance tests for #7157: `[agents] instructions` key, its enum and the migration step.

use zeph_config::migrate::migrate_agents_instructions;
use zeph_config::{SubAgentConfig, SubagentInstructions};

#[test]
fn test_subagent_instructions_default_is_inherit() {
    assert_eq!(
        SubagentInstructions::default(),
        SubagentInstructions::Inherit
    );
    assert_eq!(
        SubAgentConfig::default().instructions,
        SubagentInstructions::Inherit
    );
}

#[test]
fn test_subagent_config_instructions_parses_snake_case_values() {
    let none: SubAgentConfig = toml::from_str("instructions = \"none\"").expect("none");
    assert_eq!(none.instructions, SubagentInstructions::None);
    let inherit: SubAgentConfig = toml::from_str("instructions = \"inherit\"").expect("inherit");
    assert_eq!(inherit.instructions, SubagentInstructions::Inherit);
}

#[test]
fn test_subagent_config_instructions_rejects_unknown_value_and_bool() {
    assert!(toml::from_str::<SubAgentConfig>("instructions = \"all\"").is_err());
    assert!(toml::from_str::<SubAgentConfig>("instructions = true").is_err());
}

#[test]
fn test_migrate_agents_instructions_adds_commented_key_when_agents_active() {
    let src = "[agents]\nenabled = true\n";
    let result = migrate_agents_instructions(src).expect("migrate");
    assert_eq!(result.changed_count, 1);
    assert!(result.output.contains("# instructions = \"inherit\""));
    assert!(
        result
            .sections_changed
            .contains(&"agents.instructions".to_owned())
    );
}

#[test]
fn test_migrate_agents_instructions_is_idempotent() {
    let once = migrate_agents_instructions("[agents]\nenabled = true\n").expect("migrate");
    let twice = migrate_agents_instructions(&once.output).expect("again");
    assert_eq!(twice.changed_count, 0);
    assert_eq!(twice.output, once.output);
}

#[test]
fn test_migrate_agents_instructions_noop_without_agents_or_with_key_present() {
    let no_agents = "[llm]\nx = 1\n";
    assert_eq!(
        migrate_agents_instructions(no_agents)
            .expect("m")
            .changed_count,
        0
    );
    let present = "[agents]\nenabled = true\ninstructions = \"none\"\n";
    let r = migrate_agents_instructions(present).expect("m");
    assert_eq!(r.changed_count, 0);
    assert_eq!(r.output, present);
}
