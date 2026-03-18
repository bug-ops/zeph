// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::super::*;

#[test]
fn orchestration_config_defaults() {
    let cfg = OrchestrationConfig::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.max_tasks, 20);
    assert_eq!(cfg.max_parallel, 4);
    assert_eq!(cfg.default_failure_strategy, "abort");
    assert_eq!(cfg.default_max_retries, 3);
}

#[test]
fn orchestration_config_serde_roundtrip() {
    let toml_str = "enabled = true\nmax_tasks = 10\ndefault_failure_strategy = \"skip\"\n";
    let cfg: OrchestrationConfig = toml::from_str(toml_str).expect("parse");
    assert!(cfg.enabled);
    assert_eq!(cfg.max_tasks, 10);
    assert_eq!(cfg.default_failure_strategy, "skip");
}

#[test]
fn orchestration_config_failure_strategy_valid() {
    let cfg = OrchestrationConfig::default(); // "abort"
    let fs: crate::orchestration::FailureStrategy =
        cfg.default_failure_strategy.parse().expect("should parse");
    assert_eq!(fs, crate::orchestration::FailureStrategy::Abort);
}

#[test]
fn orchestration_config_failure_strategy_invalid() {
    let cfg = OrchestrationConfig {
        default_failure_strategy: "abort_all".to_string(),
        ..Default::default()
    };
    assert!(
        cfg.default_failure_strategy
            .parse::<crate::orchestration::FailureStrategy>()
            .is_err()
    );
}

#[test]
fn generation_params_defaults() {
    let p = GenerationParams::default();
    assert!((p.temperature - 0.7).abs() < f64::EPSILON);
    assert_eq!(p.max_tokens, 2048);
    assert_eq!(p.seed, 42);
}

#[test]
fn scheduled_task_kind_serde_memory_cleanup() {
    let kind = ScheduledTaskKind::MemoryCleanup;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#""memory_cleanup""#);
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_kind_serde_skill_refresh() {
    let kind = ScheduledTaskKind::SkillRefresh;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#""skill_refresh""#);
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_kind_serde_health_check() {
    let kind = ScheduledTaskKind::HealthCheck;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#""health_check""#);
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_kind_serde_update_check() {
    let kind = ScheduledTaskKind::UpdateCheck;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#""update_check""#);
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_kind_serde_experiment() {
    let kind = ScheduledTaskKind::Experiment;
    let json = serde_json::to_string(&kind).unwrap();
    assert_eq!(json, r#""experiment""#);
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_kind_serde_custom_roundtrip() {
    let kind = ScheduledTaskKind::Custom("my_task".to_owned());
    let json = serde_json::to_string(&kind).unwrap();
    let back: ScheduledTaskKind = serde_json::from_str(&json).unwrap();
    assert_eq!(back, kind);
}

#[test]
fn scheduled_task_config_toml_known_kind() {
    let toml = r#"
        name = "cleanup"
        cron = "0 3 * * *"
        kind = "memory_cleanup"
    "#;
    let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.kind, ScheduledTaskKind::MemoryCleanup);
    assert_eq!(cfg.name, "cleanup");
}

#[test]
fn scheduled_task_config_toml_custom_kind() {
    let toml = r#"
        name = "my-job"
        cron = "*/5 * * * *"
        kind = { custom = "report_gen" }
    "#;
    let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.kind, ScheduledTaskKind::Custom("report_gen".to_owned()));
}

#[test]
fn scheduled_task_config_toml_invalid_kind_errors() {
    let toml = r#"
        name = "bad"
        cron = "* * * * *"
        kind = "does_not_exist"
    "#;
    let result: Result<ScheduledTaskConfig, _> = toml::from_str(toml);
    assert!(result.is_err());
}

#[test]
fn scheduled_task_config_oneshot_with_run_at() {
    let toml = r#"
        name = "reminder"
        run_at = "2026-04-01T09:00:00Z"
        kind = { custom = "my_job" }
    "#;
    let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
    assert!(cfg.cron.is_none());
    assert_eq!(cfg.run_at.as_deref(), Some("2026-04-01T09:00:00Z"));
    assert_eq!(cfg.kind, ScheduledTaskKind::Custom("my_job".to_owned()));
}

#[test]
fn config_rejects_both_cron_and_run_at() {
    // Both set: application should validate, struct itself accepts both for flexibility.
    // The validation is done at bootstrap, not at deserialization.
    let toml = r#"
        name = "bad"
        cron = "0 * * * * *"
        run_at = "2026-04-01T09:00:00Z"
        kind = "health_check"
    "#;
    let cfg: ScheduledTaskConfig = toml::from_str(toml).unwrap();
    assert!(cfg.cron.is_some() && cfg.run_at.is_some());
}

#[test]
fn scheduler_config_defaults() {
    let cfg = SchedulerConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.tick_interval_secs, 60);
    assert_eq!(cfg.max_tasks, 100);
    assert!(cfg.tasks.is_empty());
}

#[test]
fn experiment_config_defaults() {
    let cfg = ExperimentConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.eval_model.is_none());
    assert!(cfg.benchmark_file.is_none());
    assert_eq!(cfg.max_experiments, 20);
    assert_eq!(cfg.max_wall_time_secs, 3600);
    assert!((cfg.min_improvement - 0.5).abs() < f64::EPSILON);
    assert_eq!(cfg.eval_budget_tokens, 100_000);
    assert!(!cfg.auto_apply);
}

#[test]
fn experiment_schedule_defaults() {
    let cfg = ExperimentSchedule::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.cron, "0 3 * * *");
    assert_eq!(cfg.max_experiments_per_run, 20);
    assert_eq!(cfg.max_wall_time_secs, 1800);
}

#[test]
fn experiment_config_serde_roundtrip() {
    let toml_str = r"
enabled = true
max_experiments = 10
min_improvement = 1.0
eval_budget_tokens = 50000
";
    let cfg: ExperimentConfig = toml::from_str(toml_str).expect("parse");
    assert!(cfg.enabled);
    assert_eq!(cfg.max_experiments, 10);
    assert!((cfg.min_improvement - 1.0).abs() < f64::EPSILON);
    assert_eq!(cfg.eval_budget_tokens, 50_000);
    // check defaults preserved for fields not in toml
    assert_eq!(cfg.max_wall_time_secs, 3600);
    let serialized = toml::to_string_pretty(&cfg).expect("serialize");
    let cfg2: ExperimentConfig = toml::from_str(&serialized).expect("reparse");
    assert!(cfg2.enabled);
    assert_eq!(cfg2.max_experiments, 10);
}

#[test]
fn config_has_experiments_field() {
    let config = Config::default();
    assert!(!config.experiments.enabled);
}

#[test]
fn experiment_config_validate_defaults_pass() {
    let cfg = ExperimentConfig::default();
    assert!(cfg.validate().is_ok());
}

#[test]
fn experiment_config_validate_rejects_max_experiments_zero() {
    let cfg = ExperimentConfig {
        max_experiments: 0,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_max_experiments_over_limit() {
    let cfg = ExperimentConfig {
        max_experiments: 1_001,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_wall_time_too_short() {
    let cfg = ExperimentConfig {
        max_wall_time_secs: 10,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_negative_min_improvement() {
    let cfg = ExperimentConfig {
        min_improvement: -1.0,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_excessive_budget() {
    let cfg = ExperimentConfig {
        eval_budget_tokens: 100,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_schedule_over_limit() {
    let cfg = ExperimentConfig {
        schedule: ExperimentSchedule {
            max_experiments_per_run: 200,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn experiment_config_validate_rejects_schedule_wall_time_too_short() {
    let cfg = ExperimentConfig {
        schedule: ExperimentSchedule {
            max_wall_time_secs: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

#[test]
fn detector_mode_default_is_regex() {
    assert_eq!(DetectorMode::default(), DetectorMode::Regex);
}

#[test]
fn learning_config_default_detector_mode_is_regex() {
    let cfg = LearningConfig::default();
    assert_eq!(cfg.detector_mode, DetectorMode::Regex);
    assert!(cfg.judge_model.is_empty());
    assert!((cfg.judge_adaptive_low - 0.5).abs() < f32::EPSILON);
    assert!((cfg.judge_adaptive_high - 0.8).abs() < f32::EPSILON);
}

#[test]
fn learning_config_deserialize_judge_mode() {
    let toml = r#"
        enabled = true
        detector_mode = "judge"
        judge_model = "claude-sonnet-4-6"
        judge_adaptive_low = 0.4
        judge_adaptive_high = 0.9
    "#;
    let cfg: LearningConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.detector_mode, DetectorMode::Judge);
    assert_eq!(cfg.judge_model, "claude-sonnet-4-6");
    assert!((cfg.judge_adaptive_low - 0.4).abs() < f32::EPSILON);
    assert!((cfg.judge_adaptive_high - 0.9).abs() < f32::EPSILON);
}

#[test]
fn learning_config_detector_mode_defaults_to_regex_when_absent() {
    let toml = "enabled = true";
    let cfg: LearningConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.detector_mode, DetectorMode::Regex);
}

#[test]
fn routing_strategy_default_is_heuristic() {
    assert_eq!(RoutingStrategy::default(), RoutingStrategy::Heuristic);
    let cfg = RoutingConfig::default();
    assert_eq!(cfg.strategy, RoutingStrategy::Heuristic);
}

#[test]
fn routing_config_toml_heuristic() {
    let toml = r#"strategy = "heuristic""#;
    let cfg: RoutingConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.strategy, RoutingStrategy::Heuristic);
}

#[test]
fn routing_config_toml_invalid_strategy_rejected() {
    let toml = r#"strategy = "unknown_strategy""#;
    let result: Result<RoutingConfig, _> = toml::from_str(toml);
    assert!(
        result.is_err(),
        "unknown strategy must fail deserialization"
    );
}

#[test]
fn compression_strategy_default_is_reactive() {
    assert_eq!(
        CompressionStrategy::default(),
        CompressionStrategy::Reactive
    );
    let cfg = CompressionConfig::default();
    assert_eq!(cfg.strategy, CompressionStrategy::Reactive);
}

#[test]
fn compression_config_toml_reactive() {
    let toml = r#"strategy = "reactive""#;
    let cfg: CompressionConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.strategy, CompressionStrategy::Reactive);
}

#[test]
fn compression_config_toml_proactive() {
    let toml = r#"
        strategy = "proactive"
        threshold_tokens = 80000
        max_summary_tokens = 4000
    "#;
    let cfg: CompressionConfig = toml::from_str(toml).unwrap();
    assert_eq!(
        cfg.strategy,
        CompressionStrategy::Proactive {
            threshold_tokens: 80_000,
            max_summary_tokens: 4_000,
        }
    );
}

#[test]
fn compression_config_toml_model_roundtrip() {
    let toml = r#"
        strategy = "reactive"
        model = "claude-haiku-4-5-20251001"
    "#;
    let cfg: CompressionConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.model, "claude-haiku-4-5-20251001");
    let serialized = toml::to_string_pretty(&cfg).unwrap();
    let back: CompressionConfig = toml::from_str(&serialized).unwrap();
    assert_eq!(back.model, cfg.model);
}

#[test]
fn routing_config_toml_question_with_double_colon_routes_test() {
    // Verifies that "what does foo::bar do" routes Semantic, not Keyword.
    // This tests the question-word override for structural code patterns (router.rs:69-70).
    // The test lives here to keep router.rs unit-focused and types.rs integration-focused.
    use zeph_memory::{HeuristicRouter, MemoryRoute, MemoryRouter};
    let router = HeuristicRouter;
    assert_eq!(
        router.route("what does foo::bar do"),
        MemoryRoute::Semantic,
        "question word must override :: structural pattern"
    );
}

#[test]
fn router_strategy_config_serde_ema() {
    let s: RouterStrategyConfig = toml::from_str("strategy = \"ema\"")
        .map(|t: toml::Value| {
            t["strategy"]
                .as_str()
                .and_then(|v| serde_json::from_value(serde_json::Value::String(v.to_owned())).ok())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    assert_eq!(s, RouterStrategyConfig::Ema);
    let json = serde_json::to_string(&RouterStrategyConfig::Ema).unwrap();
    assert_eq!(json, r#""ema""#);
}

#[test]
fn router_strategy_config_serde_thompson() {
    let json = serde_json::to_string(&RouterStrategyConfig::Thompson).unwrap();
    assert_eq!(json, r#""thompson""#);
    let back: RouterStrategyConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, RouterStrategyConfig::Thompson);
}

#[test]
fn router_strategy_config_serde_cascade() {
    let json = serde_json::to_string(&RouterStrategyConfig::Cascade).unwrap();
    assert_eq!(json, r#""cascade""#);
    let back: RouterStrategyConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back, RouterStrategyConfig::Cascade);
}

#[test]
fn router_strategy_config_default_is_ema() {
    assert_eq!(RouterStrategyConfig::default(), RouterStrategyConfig::Ema);
}

#[test]
fn router_strategy_config_invalid_deserialize_fails() {
    let result: Result<RouterStrategyConfig, _> = serde_json::from_str(r#""unknown""#);
    assert!(result.is_err(), "unknown variant must fail to deserialize");
}
