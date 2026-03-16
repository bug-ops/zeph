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
    assert_eq!(cfg.edge_history_limit, 100);
    assert!((cfg.temporal_decay_rate - 0.0).abs() < f64::EPSILON);
}

#[test]
fn graph_config_temporal_decay_rate_valid_zero() {
    let toml = r#"temporal_decay_rate = 0.0"#;
    let cfg: GraphConfig = toml::from_str(toml).unwrap();
    assert!((cfg.temporal_decay_rate - 0.0).abs() < f64::EPSILON);
}

#[test]
fn graph_config_temporal_decay_rate_valid_mid() {
    let toml = r#"temporal_decay_rate = 5.0"#;
    let cfg: GraphConfig = toml::from_str(toml).unwrap();
    assert!((cfg.temporal_decay_rate - 5.0).abs() < f64::EPSILON);
}

#[test]
fn graph_config_temporal_decay_rate_valid_max() {
    let toml = r#"temporal_decay_rate = 10.0"#;
    let cfg: GraphConfig = toml::from_str(toml).unwrap();
    assert!((cfg.temporal_decay_rate - 10.0).abs() < f64::EPSILON);
}

#[test]
fn graph_config_temporal_decay_rate_negative_rejected() {
    let toml = r#"temporal_decay_rate = -0.1"#;
    assert!(
        toml::from_str::<GraphConfig>(toml).is_err(),
        "negative temporal_decay_rate must be rejected"
    );
}

#[test]
fn graph_config_temporal_decay_rate_above_max_rejected() {
    let toml = r#"temporal_decay_rate = 10.1"#;
    assert!(
        toml::from_str::<GraphConfig>(toml).is_err(),
        "temporal_decay_rate > 10.0 must be rejected"
    );
}

// NaN cannot be expressed in TOML or JSON; the is_nan() guard in validate_temporal_decay_rate
// is a defense-in-depth check against programmatic misuse (e.g. direct struct construction),
// not reachable through normal deserialization paths.

#[test]
fn graph_config_temporal_decay_rate_inf_rejected() {
    // TOML does not support Inf literals; test via a sufficiently large JSON float that
    // overflows to f64::INFINITY when parsed by serde_json.
    let json = r#"{"temporal_decay_rate": 1e309}"#;
    assert!(
        serde_json::from_str::<GraphConfig>(json).is_err(),
        "+Inf temporal_decay_rate must be rejected"
    );
}

#[test]
fn graph_config_temporal_decay_rate_neg_inf_rejected() {
    let json = r#"{"temporal_decay_rate": -1e309}"#;
    assert!(
        serde_json::from_str::<GraphConfig>(json).is_err(),
        "-Inf temporal_decay_rate must be rejected"
    );
}

#[test]
fn graph_config_toml_round_trip() {
    let original = GraphConfig::default();
    let toml_str = toml::to_string_pretty(&original).expect("serialize");
    let back: GraphConfig = toml::from_str(&toml_str).expect("deserialize");
    assert_eq!(back.enabled, original.enabled);
    assert_eq!(back.max_hops, original.max_hops);
    assert_eq!(back.recall_limit, original.recall_limit);
    assert_eq!(back.temporal_decay_rate, original.temporal_decay_rate);
}

// ── NoteLinkingConfig serde validation tests ──────────────────────────────

#[test]
fn note_linking_config_defaults() {
    let cfg = NoteLinkingConfig::default();
    assert!(!cfg.enabled);
    assert!((cfg.similarity_threshold - 0.85_f32).abs() < f32::EPSILON);
    assert_eq!(cfg.top_k, 10);
    assert_eq!(cfg.timeout_secs, 5);
}

#[test]
fn note_linking_config_valid_threshold_round_trips() {
    let toml = r#"
        enabled = true
        similarity_threshold = 0.9
        top_k = 5
        timeout_secs = 10
    "#;
    let cfg: NoteLinkingConfig = toml::from_str(toml).unwrap();
    assert!(cfg.enabled);
    assert!((cfg.similarity_threshold - 0.9_f32).abs() < 1e-6_f32);
    assert_eq!(cfg.top_k, 5);
    assert_eq!(cfg.timeout_secs, 10);
}

#[test]
fn note_linking_config_threshold_boundary_zero_valid() {
    let toml = "similarity_threshold = 0.0";
    assert!(
        toml::from_str::<NoteLinkingConfig>(toml).is_ok(),
        "threshold 0.0 must be valid"
    );
}

#[test]
fn note_linking_config_threshold_boundary_one_valid() {
    let toml = "similarity_threshold = 1.0";
    assert!(
        toml::from_str::<NoteLinkingConfig>(toml).is_ok(),
        "threshold 1.0 must be valid"
    );
}

#[test]
fn note_linking_config_threshold_negative_rejected() {
    let toml = "similarity_threshold = -0.1";
    assert!(
        toml::from_str::<NoteLinkingConfig>(toml).is_err(),
        "negative threshold must be rejected"
    );
}

#[test]
fn note_linking_config_threshold_above_one_rejected() {
    let toml = "similarity_threshold = 1.1";
    assert!(
        toml::from_str::<NoteLinkingConfig>(toml).is_err(),
        "threshold > 1.0 must be rejected"
    );
}

#[test]
fn note_linking_config_threshold_inf_rejected() {
    let json = r#"{"similarity_threshold": 1e39}"#;
    assert!(
        serde_json::from_str::<NoteLinkingConfig>(json).is_err(),
        "+Inf similarity_threshold must be rejected"
    );
}

#[test]
fn graph_config_includes_note_linking_defaults() {
    let cfg = GraphConfig::default();
    assert!(!cfg.note_linking.enabled);
    assert!((cfg.note_linking.similarity_threshold - 0.85_f32).abs() < f32::EPSILON);
    assert_eq!(cfg.note_linking.top_k, 10);
    assert_eq!(cfg.note_linking.timeout_secs, 5);
}

#[test]
fn graph_config_note_linking_toml_round_trip() {
    let original = GraphConfig::default();
    let toml_str = toml::to_string_pretty(&original).expect("serialize");
    let back: GraphConfig = toml::from_str(&toml_str).expect("deserialize");
    assert_eq!(back.note_linking.enabled, original.note_linking.enabled);
    assert_eq!(back.note_linking.top_k, original.note_linking.top_k);
    assert_eq!(
        back.note_linking.timeout_secs,
        original.note_linking.timeout_secs
    );
}

// T-MED-01: Config validation for SidequestConfig and FocusConfig new fields.

#[test]
fn sidequest_config_defaults_are_sane() {
    let cfg = SidequestConfig::default();
    assert!(!cfg.enabled, "sidequest defaults to disabled");
    assert!(
        cfg.interval_turns > 0,
        "interval_turns must be > 0 by default"
    );
    assert!(
        cfg.max_eviction_ratio > 0.0 && cfg.max_eviction_ratio <= 1.0,
        "max_eviction_ratio must be in (0.0, 1.0]"
    );
    assert!(cfg.max_cursors > 0, "max_cursors must be > 0 by default");
}

#[test]
fn sidequest_config_validates_zero_interval_turns() {
    let mut config = Config::default();
    config.memory.sidequest.interval_turns = 0;
    assert!(
        config.validate().is_err(),
        "interval_turns=0 must fail validation"
    );
}

#[test]
fn sidequest_config_validates_max_eviction_ratio_zero() {
    let mut config = Config::default();
    config.memory.sidequest.max_eviction_ratio = 0.0;
    assert!(
        config.validate().is_err(),
        "max_eviction_ratio=0.0 must fail validation"
    );
}

#[test]
fn sidequest_config_validates_max_eviction_ratio_above_one() {
    let mut config = Config::default();
    config.memory.sidequest.max_eviction_ratio = 1.1;
    assert!(
        config.validate().is_err(),
        "max_eviction_ratio=1.1 must fail validation"
    );
}

#[test]
fn sidequest_config_validates_max_eviction_ratio_one_is_valid() {
    let mut config = Config::default();
    config.memory.sidequest.max_eviction_ratio = 1.0;
    // Other fields must be valid for this test to isolate the ratio
    assert!(
        config.validate().is_ok(),
        "max_eviction_ratio=1.0 must pass validation"
    );
}

#[test]
fn focus_config_defaults_are_sane() {
    let cfg = FocusConfig::default();
    assert!(!cfg.enabled, "focus defaults to disabled");
    assert!(
        cfg.compression_interval > 0,
        "compression_interval must be > 0"
    );
    assert!(
        cfg.min_messages_per_focus > 0,
        "min_messages_per_focus must be > 0"
    );
    assert!(
        cfg.max_knowledge_tokens > 0,
        "max_knowledge_tokens must be > 0"
    );
}

#[test]
fn focus_compression_interval_default_matches_doc() {
    let cfg = FocusConfig::default();
    assert_eq!(
        cfg.compression_interval, 12,
        "default compression_interval must be 12 (matches default.toml comment)"
    );
}

#[test]
fn focus_max_knowledge_tokens_default_is_4096() {
    let cfg = FocusConfig::default();
    assert_eq!(cfg.max_knowledge_tokens, 4096);
}
