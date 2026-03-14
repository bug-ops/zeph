// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::super::*;

#[test]
fn memory_config_sqlite_pool_size_default_is_5() {
    let config = Config::default();
    assert_eq!(config.memory.sqlite_pool_size, 5);
}

#[test]
fn memory_config_sqlite_pool_size_deserializes_from_toml() {
    let toml = r#"
        sqlite_path = "test.db"
        history_limit = 50
        sqlite_pool_size = 10
    "#;
    let cfg: MemoryConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.sqlite_pool_size, 10);
}

#[test]
fn memory_config_sqlite_pool_size_uses_default_when_absent() {
    let toml = r#"
        sqlite_path = "test.db"
        history_limit = 50
    "#;
    let cfg: MemoryConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.sqlite_pool_size, 5);
}

#[test]
fn subagent_config_defaults_when_section_absent() {
    let cfg = SubAgentConfig::default();
    assert!(!cfg.enabled, "enabled defaults to false");
    assert_eq!(cfg.max_concurrent, 5, "max_concurrent defaults to 5");
    assert!(cfg.extra_dirs.is_empty(), "extra_dirs defaults to empty");

    let default_cfg = Config::default();
    assert!(!default_cfg.agents.enabled);
    assert_eq!(default_cfg.agents.max_concurrent, 5);
    assert!(default_cfg.agents.extra_dirs.is_empty());
}

#[test]
fn subagent_config_full_section_deserializes() {
    let toml = r#"
        enabled = true
        max_concurrent = 8
        extra_dirs = ["/custom/agents", "/other/agents"]
    "#;
    let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
    assert!(cfg.enabled);
    assert_eq!(cfg.max_concurrent, 8);
    assert_eq!(cfg.extra_dirs.len(), 2);
    assert_eq!(
        cfg.extra_dirs[0],
        std::path::PathBuf::from("/custom/agents")
    );
}

#[test]
fn subagent_config_partial_section_uses_field_defaults() {
    // Only max_concurrent provided — other fields use Default.
    let toml = r"max_concurrent = 3";
    let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.max_concurrent, 3);
    assert!(!cfg.enabled);
    assert!(cfg.extra_dirs.is_empty());
}

#[test]
fn subagent_config_default_permission_mode_is_none() {
    let cfg = SubAgentConfig::default();
    assert!(cfg.default_permission_mode.is_none());
    assert!(cfg.default_disallowed_tools.is_empty());
}

#[test]
fn subagent_config_default_permission_mode_deserializes() {
    use crate::subagent::def::PermissionMode;
    let toml = r#"
        enabled = true
        max_concurrent = 2
        default_permission_mode = "plan"
        default_disallowed_tools = ["dangerous_tool", "other"]
    "#;
    let cfg: SubAgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.default_permission_mode, Some(PermissionMode::Plan));
    assert_eq!(cfg.default_disallowed_tools, ["dangerous_tool", "other"]);
}

#[test]
fn graph_config_defaults() {
    let cfg = GraphConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.extract_model.is_empty());
    assert_eq!(cfg.max_entities_per_message, 10);
    assert_eq!(cfg.max_edges_per_message, 15);
    assert_eq!(cfg.community_refresh_interval, 100);
    assert!((cfg.entity_similarity_threshold - 0.85).abs() < f32::EPSILON);
    assert_eq!(cfg.extraction_timeout_secs, 15);
    assert!(!cfg.use_embedding_resolution);
    assert!((cfg.entity_ambiguous_threshold - 0.70).abs() < f32::EPSILON);
    assert_eq!(cfg.max_hops, 2);
    assert_eq!(cfg.recall_limit, 10);
    assert_eq!(cfg.expired_edge_retention_days, 90);
    assert_eq!(cfg.max_entities, 0);
    assert_eq!(cfg.community_summary_max_prompt_bytes, 8192);
    assert_eq!(cfg.community_summary_concurrency, 4);
    assert_eq!(cfg.lpa_edge_chunk_size, 10_000);
}

#[test]
fn graph_config_toml_round_trip() {
    let original = GraphConfig::default();
    let toml_str = toml::to_string_pretty(&original).expect("serialize");
    let back: GraphConfig = toml::from_str(&toml_str).expect("deserialize");
    assert_eq!(back.enabled, original.enabled);
    assert_eq!(back.max_hops, original.max_hops);
    assert_eq!(back.recall_limit, original.recall_limit);
}
