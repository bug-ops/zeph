// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Unit tests for memory configuration types.

#[cfg(test)]
mod general_config {
    use crate::memory::*;

    // Verify that serde deserialization routes through FromStr so that removed variants
    // (task_aware_mig) fall back to Reactive instead of hard-erroring when found in TOML.
    #[test]
    fn pruning_strategy_toml_task_aware_mig_falls_back_to_reactive() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            pruning_strategy: PruningStrategy,
        }
        let toml = r#"pruning_strategy = "task_aware_mig""#;
        let w: Wrapper = toml::from_str(toml).expect("should deserialize without error");
        assert_eq!(
            w.pruning_strategy,
            PruningStrategy::Reactive,
            "task_aware_mig must fall back to Reactive"
        );
    }

    #[test]
    fn pruning_strategy_toml_round_trip() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            pruning_strategy: PruningStrategy,
        }
        for (input, expected) in [
            ("reactive", PruningStrategy::Reactive),
            ("task_aware", PruningStrategy::TaskAware),
            ("mig", PruningStrategy::Mig),
        ] {
            let toml = format!(r#"pruning_strategy = "{input}""#);
            let w: Wrapper = toml::from_str(&toml)
                .unwrap_or_else(|e| panic!("failed to deserialize `{input}`: {e}"));
            assert_eq!(w.pruning_strategy, expected, "mismatch for `{input}`");
        }
    }

    #[test]
    fn pruning_strategy_toml_unknown_value_errors() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Wrapper {
            pruning_strategy: PruningStrategy,
        }
        let toml = r#"pruning_strategy = "nonexistent_strategy""#;
        assert!(
            toml::from_str::<Wrapper>(toml).is_err(),
            "unknown strategy must produce an error"
        );
    }

    #[test]
    fn tier_config_defaults_are_correct() {
        let cfg = TierConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.promotion_min_sessions, 3);
        assert!((cfg.similarity_threshold - 0.92).abs() < f32::EPSILON);
        assert_eq!(cfg.sweep_interval_secs, 3600);
        assert_eq!(cfg.sweep_batch_size, 100);
    }

    #[test]
    fn tier_config_rejects_min_sessions_below_2() {
        let toml = "promotion_min_sessions = 1";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    #[test]
    fn tier_config_rejects_similarity_threshold_below_0_5() {
        let toml = "similarity_threshold = 0.4";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    #[test]
    fn tier_config_rejects_zero_sweep_batch_size() {
        let toml = "sweep_batch_size = 0";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    fn deserialize_importance_weight(toml_val: &str) -> Result<SemanticConfig, toml::de::Error> {
        let input = format!("importance_weight = {toml_val}");
        toml::from_str::<SemanticConfig>(&input)
    }

    #[test]
    fn importance_weight_default_is_0_15() {
        let cfg = SemanticConfig::default();
        assert!((cfg.importance_weight - 0.15).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_valid_zero() {
        let cfg = deserialize_importance_weight("0.0").unwrap();
        assert!((cfg.importance_weight - 0.0_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_valid_one() {
        let cfg = deserialize_importance_weight("1.0").unwrap();
        assert!((cfg.importance_weight - 1.0_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_rejects_near_zero_negative() {
        // TOML does not have a NaN literal, but we can test via a f64 that
        // the validator rejects out-of-range values. Test with negative here
        // and rely on validate_importance_weight rejecting non-finite via
        // a constructed deserializer call.
        let result = deserialize_importance_weight("-0.01");
        assert!(
            result.is_err(),
            "negative importance_weight must be rejected"
        );
    }

    #[test]
    fn importance_weight_rejects_negative() {
        let result = deserialize_importance_weight("-1.0");
        assert!(result.is_err(), "negative value must be rejected");
    }

    #[test]
    fn importance_weight_rejects_greater_than_one() {
        let result = deserialize_importance_weight("1.01");
        assert!(result.is_err(), "value > 1.0 must be rejected");
    }

    // ── AdmissionWeights::normalized() tests (#2317) ────────────────────────

    // Test: weights that don't sum to 1.0 are normalized to sum to 1.0.
    #[test]
    fn admission_weights_normalized_sums_to_one() {
        let w = AdmissionWeights {
            future_utility: 2.0,
            factual_confidence: 1.0,
            semantic_novelty: 3.0,
            temporal_recency: 1.0,
            content_type_prior: 3.0,
            goal_utility: 0.0,
        };
        let n = w.normalized();
        let sum = n.future_utility
            + n.factual_confidence
            + n.semantic_novelty
            + n.temporal_recency
            + n.content_type_prior;
        assert!(
            (sum - 1.0).abs() < 0.001,
            "normalized weights must sum to 1.0, got {sum}"
        );
    }

    // Test: already-normalized weights are preserved.
    #[test]
    fn admission_weights_normalized_preserves_already_unit_sum() {
        let w = AdmissionWeights::default();
        let n = w.normalized();
        let sum = n.future_utility
            + n.factual_confidence
            + n.semantic_novelty
            + n.temporal_recency
            + n.content_type_prior;
        assert!(
            (sum - 1.0).abs() < 0.001,
            "default weights sum to ~1.0 after normalization"
        );
    }

    // Test: zero weights fall back to default (no divide-by-zero panic).
    #[test]
    fn admission_weights_normalized_zero_sum_falls_back_to_default() {
        let w = AdmissionWeights {
            future_utility: 0.0,
            factual_confidence: 0.0,
            semantic_novelty: 0.0,
            temporal_recency: 0.0,
            content_type_prior: 0.0,
            goal_utility: 0.0,
        };
        let n = w.normalized();
        let default = AdmissionWeights::default();
        assert!(
            (n.future_utility - default.future_utility).abs() < 0.001,
            "zero-sum weights must fall back to defaults"
        );
    }

    // Test: AdmissionConfig default values match documented defaults.
    #[test]
    fn admission_config_defaults() {
        let cfg = AdmissionConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - 0.40).abs() < 0.001);
        assert!((cfg.fast_path_margin - 0.15).abs() < 0.001);
        assert!(cfg.admission_provider.is_empty());
    }

    // ── SpreadingActivationConfig tests (#2514) ──────────────────────────────

    #[test]
    fn spreading_activation_default_recall_timeout_ms_is_1000() {
        let cfg = SpreadingActivationConfig::default();
        assert_eq!(
            cfg.recall_timeout_ms, 1000,
            "default recall_timeout_ms must be 1000ms"
        );
    }

    #[test]
    fn spreading_activation_toml_recall_timeout_ms_round_trip() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            recall_timeout_ms: u64,
        }
        let toml = "recall_timeout_ms = 500";
        let w: Wrapper = toml::from_str(toml).unwrap();
        assert_eq!(w.recall_timeout_ms, 500);
    }

    #[test]
    fn spreading_activation_validate_cross_field_constraints() {
        let mut cfg = SpreadingActivationConfig::default();
        // Default activation_threshold (0.1) < inhibition_threshold (0.8) → must be Ok.
        assert!(cfg.validate().is_ok());

        // Equal thresholds must be rejected.
        cfg.activation_threshold = 0.5;
        cfg.inhibition_threshold = 0.5;
        assert!(cfg.validate().is_err());
    }

    // ─── CompressionConfig: new Focus fields deserialization (#2510, #2481) ──

    #[test]
    fn compression_config_focus_strategy_deserializes() {
        let toml = r#"strategy = "focus""#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.strategy, CompressionStrategy::Focus);
    }

    #[test]
    fn compression_config_density_budget_defaults_on_deserialize() {
        // `#[serde(default = "...")]` applies during deserialization, not via Default::default().
        // Verify that omitting both fields yields the serde defaults (0.7 / 0.3).
        let toml = r#"strategy = "reactive""#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert!((cfg.high_density_budget - 0.7).abs() < 1e-6);
        assert!((cfg.low_density_budget - 0.3).abs() < 1e-6);
    }

    #[test]
    fn compression_config_density_budget_round_trip() {
        let toml = "strategy = \"reactive\"\nhigh_density_budget = 0.6\nlow_density_budget = 0.4";
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert!((cfg.high_density_budget - 0.6).abs() < f32::EPSILON);
        assert!((cfg.low_density_budget - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn compression_config_focus_scorer_provider_default_empty() {
        let cfg = CompressionConfig::default();
        assert!(cfg.focus_scorer_provider.is_empty());
    }

    #[test]
    fn compression_config_focus_scorer_provider_round_trip() {
        let toml = "strategy = \"focus\"\nfocus_scorer_provider = \"fast\"";
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.focus_scorer_provider.as_str(), "fast");
    }
}

#[cfg(test)]
mod memcot_config_tests {
    use crate::memory::*;

    #[test]
    fn memcot_config_default_disabled() {
        let cfg = MemCotConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.distill_provider.is_empty());
        assert_eq!(cfg.distill_timeout_secs, 5);
        assert_eq!(cfg.min_assistant_chars, 200);
        assert_eq!(cfg.min_distill_interval_secs, 30);
        assert_eq!(cfg.max_distills_per_session, 50);
        assert_eq!(cfg.max_state_chars, 800);
        assert_eq!(cfg.recall_view, RecallViewConfig::Head);
        assert_eq!(cfg.zoom_out_neighbor_cap, 3);
    }

    #[test]
    fn memcot_config_round_trip() {
        let toml = r#"
            enabled = true
            distill_provider = "fast"
            distill_timeout_secs = 10
            min_assistant_chars = 100
            min_distill_interval_secs = 60
            max_distills_per_session = 20
            max_state_chars = 400
            recall_view = "zoom_in"
            zoom_out_neighbor_cap = 5
        "#;
        let cfg: MemCotConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.distill_provider.as_str(), "fast");
        assert_eq!(cfg.distill_timeout_secs, 10);
        assert_eq!(cfg.min_distill_interval_secs, 60);
        assert_eq!(cfg.max_distills_per_session, 20);
        assert_eq!(cfg.recall_view, RecallViewConfig::ZoomIn);
        assert_eq!(cfg.zoom_out_neighbor_cap, 5);
    }
}

#[cfg(test)]
mod apex_mem_quality_gate_config_tests {
    use crate::memory::*;

    #[test]
    fn apex_mem_config_default_disabled() {
        let cfg = ApexMemConfig::default();
        assert!(!cfg.enabled, "APEX-MEM must be disabled by default");
    }

    #[test]
    fn apex_mem_config_serde_round_trip() {
        let toml = "enabled = true";
        let cfg: ApexMemConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
    }

    #[test]
    fn apex_mem_config_empty_toml_uses_defaults() {
        let cfg: ApexMemConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled, "empty TOML must produce default (disabled)");
    }

    #[test]
    fn write_quality_gate_config_default_disabled() {
        let cfg = WriteQualityGateConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - 0.55).abs() < f32::EPSILON);
        assert_eq!(cfg.recent_window, 32);
        assert_eq!(cfg.contradiction_grace_seconds, 300);
        assert!((cfg.information_value_weight - 0.4).abs() < f32::EPSILON);
        assert!((cfg.reference_completeness_weight - 0.3).abs() < f32::EPSILON);
        assert!((cfg.contradiction_weight - 0.3).abs() < f32::EPSILON);
        assert!((cfg.rejection_rate_alarm_ratio - 0.35).abs() < f32::EPSILON);
        assert!(cfg.quality_gate_provider.is_empty());
        assert_eq!(cfg.llm_timeout_ms, 500);
        assert!((cfg.llm_weight - 0.5).abs() < f32::EPSILON);
        assert!(cfg.reference_check_lang_en);
    }

    #[test]
    fn write_quality_gate_config_serde_round_trip() {
        let toml = r#"
            enabled = true
            threshold = 0.70
            recent_window = 16
            contradiction_grace_seconds = 600
            information_value_weight = 0.5
            reference_completeness_weight = 0.25
            contradiction_weight = 0.25
            rejection_rate_alarm_ratio = 0.50
            quality_gate_provider = "fast"
            llm_timeout_ms = 1000
            llm_weight = 0.3
            reference_check_lang_en = false
        "#;
        let cfg: WriteQualityGateConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert!((cfg.threshold - 0.70).abs() < f32::EPSILON);
        assert_eq!(cfg.recent_window, 16);
        assert_eq!(cfg.contradiction_grace_seconds, 600);
        assert_eq!(cfg.quality_gate_provider.as_str(), "fast");
        assert_eq!(cfg.llm_timeout_ms, 1000);
        assert!(!cfg.reference_check_lang_en);
    }

    #[test]
    fn write_quality_gate_config_empty_toml_uses_defaults() {
        let cfg: WriteQualityGateConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled, "empty TOML must produce default (disabled)");
        assert_eq!(cfg.recent_window, 32);
    }

    #[test]
    fn memory_config_shutdown_summary_provider_toml_roundtrip() {
        let toml = r#"
            history_limit = 50
            shutdown_summary_provider = "fast"
        "#;
        let cfg: MemoryConfig = toml::from_str(toml).expect("must deserialize");
        assert_eq!(
            cfg.shutdown_summary_provider.as_str(),
            "fast",
            "shutdown_summary_provider must deserialize from TOML"
        );
    }

    #[test]
    fn five_signal_config_default_is_disabled() {
        let cfg: MemoryConfig = toml::from_str("history_limit = 50").expect("must deserialize");
        assert!(!cfg.five_signal.enabled);
        assert!((cfg.five_signal.w_recency - 0.35).abs() < 1e-9);
        assert!((cfg.five_signal.w_relevance - 0.35).abs() < 1e-9);
        assert!((cfg.five_signal.w_frequency).abs() < 1e-9);
        assert!((cfg.five_signal.w_causal).abs() < 1e-9);
        assert!((cfg.five_signal.w_novelty).abs() < 1e-9);
    }

    #[test]
    fn five_signal_config_toml_roundtrip() {
        let toml = r"
            history_limit = 50
            [five_signal]
            enabled = true
            w_recency = 0.35
            w_relevance = 0.35
            w_frequency = 0.15
            w_causal = 0.10
            w_novelty = 0.05
        ";
        let cfg: MemoryConfig = toml::from_str(toml).expect("must deserialize");
        assert!(cfg.five_signal.enabled);
        assert!((cfg.five_signal.w_frequency - 0.15).abs() < 1e-9);
    }

    #[test]
    fn memory_config_shutdown_summary_provider_default_is_empty() {
        let cfg: MemoryConfig = toml::from_str("history_limit = 50").expect("must deserialize");
        assert_eq!(
            cfg.shutdown_summary_provider.as_str(),
            "",
            "shutdown_summary_provider must default to empty string"
        );
    }

    #[test]
    fn memory_config_compaction_provider_toml_roundtrip() {
        let toml = r#"
            history_limit = 50
            compaction_provider = "mid"
        "#;
        let cfg: MemoryConfig = toml::from_str(toml).expect("must deserialize");
        assert_eq!(
            cfg.compaction_provider.as_str(),
            "mid",
            "compaction_provider must deserialize from TOML"
        );
    }

    #[test]
    fn memory_config_compaction_provider_default_is_empty() {
        let cfg: MemoryConfig = toml::from_str("history_limit = 50").expect("must deserialize");
        assert_eq!(
            cfg.compaction_provider.as_str(),
            "",
            "compaction_provider must default to empty string"
        );
    }
}
