// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::agent_tests::{
    MockChannel, MockToolExecutor, create_test_registry, mock_provider, mock_provider_failing,
};
#[allow(clippy::wildcard_imports)]
use super::super::*;
use super::background::{chrono_parse_sqlite, write_skill_file};
use super::preferences::infer_preferences;
use crate::config::LearningConfig;
use tokio::sync::watch;
use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::registry::SkillRegistry;

async fn test_memory() -> SemanticMemory {
    let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
    SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model")
        .await
        .unwrap()
}

/// Creates a registry with a "test-skill" and returns both the registry and the `TempDir`.
/// The `TempDir` must be kept alive for the duration of the test because `get_skill` reads
/// the skill body lazily from the filesystem.
fn create_registry_with_tempdir() -> (SkillRegistry, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill\n---\nTest skill body",
    )
    .unwrap();
    let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
    (registry, temp_dir)
}

#[allow(clippy::default_trait_access)]
fn learning_config_enabled() -> LearningConfig {
    LearningConfig {
        enabled: true,
        auto_activate: false,
        min_failures: 2,
        improve_threshold: 0.7,
        rollback_threshold: 0.3,
        min_evaluations: 3,
        max_versions: 5,
        cooldown_minutes: 0,
        correction_detection: true,
        correction_confidence_threshold: 0.6,
        detector_mode: crate::config::DetectorMode::default(),
        judge_model: String::new(),
        judge_adaptive_low: 0.5,
        judge_adaptive_high: 0.8,
        correction_recall_limit: 3,
        correction_min_similarity: 0.75,
        auto_promote_min_uses: 50,
        auto_promote_threshold: 0.95,
        auto_demote_min_uses: 30,
        auto_demote_threshold: 0.40,
        feedback_provider: zeph_config::ProviderName::default(),
        cross_session_rollout: false,
        min_sessions_before_promote: 2,
        min_sessions_before_demote: 1,
        max_auto_sections: 3,
        domain_success_gate: false,
        ..LearningConfig::default()
    }
}

#[test]
fn chrono_parse_valid_datetime() {
    let secs = chrono_parse_sqlite("2024-01-15 10:30:00").unwrap();
    assert!(secs > 0);
}

#[test]
fn chrono_parse_ordering_preserved() {
    let earlier = chrono_parse_sqlite("2024-01-15 10:00:00").unwrap();
    let later = chrono_parse_sqlite("2024-01-15 11:00:00").unwrap();
    assert!(later > earlier);
}

#[test]
fn chrono_parse_different_days() {
    let day1 = chrono_parse_sqlite("2024-06-01 00:00:00").unwrap();
    let day2 = chrono_parse_sqlite("2024-06-02 00:00:00").unwrap();
    assert_eq!(day2 - day1, 86400);
}

#[test]
fn chrono_parse_invalid_format() {
    assert!(chrono_parse_sqlite("not-a-date").is_err());
    assert!(chrono_parse_sqlite("").is_err());
    assert!(chrono_parse_sqlite("2024-01").is_err());
}

#[tokio::test]
async fn write_skill_file_missing_dir() {
    let dir = tempfile::tempdir().unwrap();
    let result = write_skill_file(
        &[dir.path().to_path_buf()],
        "nonexistent-skill",
        "desc",
        "body",
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn write_skill_file_updates_existing() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "old content").unwrap();

    write_skill_file(
        &[dir.path().to_path_buf()],
        "test-skill",
        "new desc",
        "new body",
    )
    .await
    .unwrap();

    let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    assert!(content.contains("new body"));
    assert!(content.contains("new desc"));
}

#[tokio::test]
async fn write_skill_file_rejects_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        write_skill_file(&[dir.path().to_path_buf()], "../evil", "d", "b")
            .await
            .is_err()
    );
    assert!(
        write_skill_file(&[dir.path().to_path_buf()], "a/b", "d", "b")
            .await
            .is_err()
    );
    assert!(
        write_skill_file(&[dir.path().to_path_buf()], "a\\b", "d", "b")
            .await
            .is_err()
    );
}

// Priority 2: is_learning_enabled

#[test]
fn is_learning_enabled_no_config_returns_false() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = Agent::new(provider, channel, registry, None, 5, executor);
    // No learning config set → false
    assert!(!agent.is_learning_enabled());
}

#[test]
fn is_learning_enabled_with_disabled_config_returns_false() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut config = learning_config_enabled();
    config.enabled = false;
    let agent = Agent::new(provider, channel, registry, None, 5, executor).with_learning(config);
    assert!(!agent.is_learning_enabled());
}

#[test]
fn is_learning_enabled_with_enabled_config_returns_true() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(learning_config_enabled());
    assert!(agent.is_learning_enabled());
}

// Priority 1: check_improvement_allowed

#[tokio::test]
async fn check_improvement_allowed_below_min_failures_returns_false() {
    let provider = mock_provider(vec!["improved skill body".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // Record 1 failure (below min_failures = 2)
    memory
        .sqlite()
        .record_skill_outcomes_batch(
            &["test-skill".to_string()],
            Some(cid),
            "tool_failure",
            None,
            None,
        )
        .await
        .unwrap();

    let config = learning_config_enabled(); // min_failures = 2
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config.clone())
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    let allowed = agent
        .check_improvement_allowed(mem, &config, "test-skill", None)
        .await
        .unwrap();
    assert!(
        !allowed,
        "should be false when below min_failures threshold"
    );
}

#[tokio::test]
async fn check_improvement_allowed_high_success_rate_returns_false() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // Record 5 successes and 2 failures (success rate = 5/7 ≈ 0.71 >= improve_threshold 0.7)
    for _ in 0..5 {
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "success",
                None,
                None,
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "tool_failure",
                None,
                None,
            )
            .await
            .unwrap();
    }

    let config = learning_config_enabled(); // improve_threshold = 0.7
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config.clone())
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    let allowed = agent
        .check_improvement_allowed(mem, &config, "test-skill", None)
        .await
        .unwrap();
    assert!(
        !allowed,
        "should be false when success rate >= improve_threshold"
    );
}

#[tokio::test]
async fn check_improvement_allowed_all_conditions_met_returns_true() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // 1 success, 3 failures (success rate = 0.25 < 0.7, failures = 3 >= min_failures = 2)
    memory
        .sqlite()
        .record_skill_outcomes_batch(
            &["test-skill".to_string()],
            Some(cid),
            "success",
            None,
            None,
        )
        .await
        .unwrap();
    for _ in 0..3 {
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "tool_failure",
                None,
                None,
            )
            .await
            .unwrap();
    }

    let config = LearningConfig {
        cooldown_minutes: 0,
        min_failures: 2,
        improve_threshold: 0.7,
        ..learning_config_enabled()
    };
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config.clone())
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    let allowed = agent
        .check_improvement_allowed(mem, &config, "test-skill", None)
        .await
        .unwrap();
    assert!(allowed, "should be true when all conditions are met");
}

#[tokio::test]
async fn check_improvement_allowed_with_user_feedback_skips_metrics() {
    // When user_feedback is Some, metrics check is skipped entirely → returns true
    // (assuming no cooldown active)
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    // No skill outcomes recorded → metrics would block; but user_feedback bypasses it

    let config = learning_config_enabled();
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config.clone())
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    let allowed = agent
        .check_improvement_allowed(mem, &config, "test-skill", Some("please improve this"))
        .await
        .unwrap();
    assert!(allowed, "user_feedback bypasses metrics check");
}

// Priority 1: generate_improved_skill evaluation gate

#[tokio::test]
async fn generate_improved_skill_returns_early_when_learning_disabled() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // No learning config → is_learning_enabled() = false → returns Ok(()) immediately
    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent
        .generate_improved_skill("test-skill", "error", "response", None)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn generate_improved_skill_returns_early_when_no_memory() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // Learning enabled but no memory → returns Ok(()) early
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(learning_config_enabled());

    let result = agent
        .generate_improved_skill("test-skill", "error", "response", None)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn generate_improved_skill_should_improve_false_skips_improvement() {
    // Provider returns SkillEvaluation JSON with should_improve: false → returns Ok(()) early
    let eval_json = r#"{"should_improve": false, "issues": [], "severity": "low"}"#;
    let provider = mock_provider(vec![eval_json.into()]);
    let channel = MockChannel::new(vec![]);
    // Keep tempdir alive so get_skill can load body from filesystem
    let (registry, _tempdir) = create_registry_with_tempdir();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // Add enough failures to pass check_improvement_allowed
    for _ in 0..3 {
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "tool_failure",
                None,
                None,
            )
            .await
            .unwrap();
    }

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(LearningConfig {
            cooldown_minutes: 0,
            min_failures: 2,
            improve_threshold: 0.7,
            ..learning_config_enabled()
        })
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let result = agent
        .generate_improved_skill("test-skill", "exit code 1", "response", None)
        .await;
    // Should return Ok(()) without calling improvement LLM
    assert!(result.is_ok());
}

#[tokio::test]
async fn generate_improved_skill_eval_error_proceeds_with_improvement() {
    // Provider fails for eval → logs warning, proceeds to call improvement LLM
    // Second call (improvement) also fails (failing provider) → error propagates
    let provider = mock_provider_failing();
    let channel = MockChannel::new(vec![]);
    // Keep tempdir alive so get_skill can load body from filesystem
    let (registry, _tempdir) = create_registry_with_tempdir();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // Add enough failures
    for _ in 0..3 {
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "tool_failure",
                None,
                None,
            )
            .await
            .unwrap();
    }

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(LearningConfig {
            cooldown_minutes: 0,
            min_failures: 2,
            improve_threshold: 0.7,
            ..learning_config_enabled()
        })
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let result = agent
        .generate_improved_skill("test-skill", "exit code 1", "response", None)
        .await;
    // eval fails (warn) → proceeds to call_improvement_llm → provider fails → Err
    assert!(result.is_err());
}

// Priority 2: attempt_self_reflection

#[tokio::test]
async fn attempt_self_reflection_learning_disabled_returns_false() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    // No learning config → is_learning_enabled() = false
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let result = agent.attempt_self_reflection("error", "output").await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[tokio::test]
async fn attempt_self_reflection_reflection_used_returns_false() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(learning_config_enabled());

    // Mark reflection as already used
    agent.learning_engine.mark_reflection_used();

    let result = agent.attempt_self_reflection("error", "output").await;
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

// Priority 2: write_skill_file with multiple paths

#[tokio::test]
async fn write_skill_file_uses_first_matching_path() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    // Create skill only in dir2
    let skill_dir = dir2.path().join("my-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "old").unwrap();

    // dir1 has no matching skill dir
    write_skill_file(
        &[dir1.path().to_path_buf(), dir2.path().to_path_buf()],
        "my-skill",
        "desc",
        "updated body",
    )
    .await
    .unwrap();

    let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    assert!(content.contains("updated body"));
}

#[tokio::test]
async fn write_skill_file_empty_paths_returns_error() {
    let result = write_skill_file(&[], "any-skill", "desc", "body").await;
    assert!(result.is_err());
}

// Priority 3: handle_skill_command dispatch (no memory → early exit messages)

#[tokio::test]
async fn handle_skill_command_unknown_subcommand() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("unknown-cmd").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Unknown /skill subcommand")));
}

#[tokio::test]
async fn handle_skill_command_stats_no_memory() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("stats").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Memory not available")));
}

#[tokio::test]
async fn handle_skill_command_versions_no_name() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("versions").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Usage: /skill versions")));
}

#[tokio::test]
async fn handle_skill_command_activate_no_args() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("activate").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Usage: /skill activate")));
}

#[tokio::test]
async fn handle_skill_command_approve_no_name() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("approve").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Usage: /skill approve")));
}

#[tokio::test]
async fn handle_skill_command_reset_no_name() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("reset").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Usage: /skill reset")));
}

#[tokio::test]
async fn handle_skill_command_versions_no_memory() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .handle_skill_command("versions test-skill")
        .await
        .unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Memory not available")));
}

#[tokio::test]
async fn handle_skill_command_activate_invalid_version() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .handle_skill_command("activate test-skill not-a-number")
        .await
        .unwrap();
    let sent = agent.channel.sent_messages();
    assert!(sent.iter().any(|s| s.contains("Invalid version number")));
}

#[tokio::test]
async fn record_skill_outcomes_no_active_skills_is_noop() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    // No active skills and no memory → should return immediately without panic
    agent.record_skill_outcomes("success", None, None).await;
    agent
        .record_skill_outcomes("tool_failure", Some("error"), None)
        .await;
}

// Priority 3: handle_skill_install / handle_skill_remove via handle_skill_command

#[tokio::test]
async fn handle_skill_command_install_no_source() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("install").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Usage: /skill install")),
        "expected usage hint, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_command_remove_no_name() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("remove").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Usage: /skill remove")),
        "expected usage hint, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_command_install_no_managed_dir() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // No managed_dir configured
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .handle_skill_command("install https://example.com/skill")
        .await
        .unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("not configured")),
        "expected not-configured message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_command_remove_no_managed_dir() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    // No managed_dir configured
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("remove my-skill").await.unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("not configured")),
        "expected not-configured message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_command_install_from_path_not_found() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let managed = tempfile::tempdir().unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_managed_skills_dir(managed.path().to_path_buf());

    agent
        .handle_skill_command("install /nonexistent/path/to/skill")
        .await
        .unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Install failed")),
        "expected install failure message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_command_remove_nonexistent_skill() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let managed = tempfile::tempdir().unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_managed_skills_dir(managed.path().to_path_buf());

    agent
        .handle_skill_command("remove nonexistent-skill")
        .await
        .unwrap();
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Remove failed")),
        "expected remove failure message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_reject_records_outcome_and_replies() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let (registry, _tempdir) = create_registry_with_tempdir();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );

    agent
        .handle_skill_command("reject test-skill the output was wrong")
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Rejection recorded")),
        "expected rejection confirmation, got: {sent:?}"
    );

    let mem = agent.memory_state.memory.as_ref().unwrap();
    let row: Option<(String,)> = zeph_db::query_as(
        "SELECT outcome FROM skill_outcomes WHERE skill_name = 'test-skill' LIMIT 1",
    )
    .fetch_optional(mem.sqlite().pool())
    .await
    .unwrap();
    assert!(row.is_some(), "outcome should be recorded in DB");
    assert_eq!(row.unwrap().0, "user_rejection");
}

#[tokio::test]
async fn handle_skill_reject_unknown_skill_returns_error_message() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .handle_skill_command("reject nonexistent-skill bad output")
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Unknown skill")),
        "expected unknown skill message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_reject_missing_name_shows_usage() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent.handle_skill_command("reject").await.unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Usage")),
        "expected usage message, got: {sent:?}"
    );
}

#[tokio::test]
async fn handle_skill_reject_missing_reason_shows_usage() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let (registry, _tempdir) = create_registry_with_tempdir();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    agent
        .handle_skill_command("reject test-skill")
        .await
        .unwrap();

    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|s| s.contains("Usage")),
        "expected usage message, got: {sent:?}"
    );
}

// check_trust_transition: auto-promote and auto-demote

async fn setup_skill_with_outcomes(
    memory: &SemanticMemory,
    skill_name: &str,
    successes: u32,
    failures: u32,
    initial_trust: &str,
) {
    use zeph_memory::store::SourceKind;
    memory
        .sqlite()
        .upsert_skill_trust(
            skill_name,
            initial_trust,
            SourceKind::Local,
            None,
            None,
            "hash",
        )
        .await
        .unwrap();
    for _ in 0..successes {
        memory
            .sqlite()
            .record_skill_outcome(skill_name, None, None, "success", None, None)
            .await
            .unwrap();
    }
    for _ in 0..failures {
        memory
            .sqlite()
            .record_skill_outcome(skill_name, None, None, "tool_failure", None, None)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn check_trust_transition_auto_promotes_to_trusted() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // 50 successes, 0 failures → posterior > 0.95 threshold
    setup_skill_with_outcomes(&memory, "test-skill", 50, 0, "local").await;

    let mut config = learning_config_enabled();
    config.auto_promote_min_uses = 50;
    config.auto_promote_threshold = 0.95;

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config)
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    agent.check_trust_transition("test-skill").await;

    let row = mem
        .sqlite()
        .load_skill_trust("test-skill")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.trust_level, "trusted",
        "should auto-promote to trusted, got: {}",
        row.trust_level
    );
}

#[tokio::test]
async fn check_trust_transition_auto_demotes_to_quarantined() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // 5 successes, 30 failures → posterior < 0.40 threshold, starting as "trusted"
    setup_skill_with_outcomes(&memory, "test-skill", 5, 30, "trusted").await;

    let mut config = learning_config_enabled();
    config.auto_demote_min_uses = 30;
    config.auto_demote_threshold = 0.40;

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config)
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    agent.check_trust_transition("test-skill").await;

    let row = mem
        .sqlite()
        .load_skill_trust("test-skill")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.trust_level, "quarantined",
        "should auto-demote to quarantined, got: {}",
        row.trust_level
    );
}

#[tokio::test]
async fn check_trust_transition_does_not_promote_blocked() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // High success rate but "blocked" — should NOT be promoted
    setup_skill_with_outcomes(&memory, "test-skill", 100, 0, "blocked").await;

    let mut config = learning_config_enabled();
    config.auto_promote_min_uses = 50;
    config.auto_promote_threshold = 0.95;

    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_learning(config)
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    let mem = agent.memory_state.memory.as_ref().unwrap();
    agent.check_trust_transition("test-skill").await;

    let row = mem
        .sqlite()
        .load_skill_trust("test-skill")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.trust_level, "blocked",
        "blocked skill should never be auto-promoted, got: {}",
        row.trust_level
    );
}

// Priority 3: proptest

use proptest::prelude::*;

proptest! {
    #[test]
    fn chrono_parse_never_panics(s in ".*") {
        let _ = chrono_parse_sqlite(&s);
    }
}

#[tokio::test]
async fn skill_confidence_populated_before_first_outcome() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory = test_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();

    // Record one success so load_skill_outcome_stats returns data.
    memory
        .sqlite()
        .record_skill_outcomes_batch(
            &["test-skill".to_string()],
            Some(cid),
            "success",
            None,
            None,
        )
        .await
        .unwrap();

    let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());
    let agent = Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(tx)
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

    // update_skill_confidence_metrics is called inside rebuild_system_prompt after
    // active_skills is set. Invoke directly to test the fix in isolation.
    agent.update_skill_confidence_metrics().await;

    let snapshot = rx.borrow().clone();
    assert!(
        !snapshot.skill_confidence.is_empty(),
        "skill_confidence must be populated after update_skill_confidence_metrics"
    );
    let entry = snapshot
        .skill_confidence
        .iter()
        .find(|c| c.name == "test-skill")
        .expect("test-skill confidence entry must exist");
    assert!(
        entry.total_uses > 0,
        "total_uses must reflect recorded outcome"
    );
}

// ── infer_preferences unit tests ──────────────────────────────────────────

fn make_correction(
    id: i64,
    text: &str,
    kind: &str,
) -> zeph_memory::store::corrections::UserCorrectionRow {
    zeph_memory::store::corrections::UserCorrectionRow {
        id,
        session_id: None,
        original_output: String::new(),
        correction_text: text.to_string(),
        skill_name: None,
        correction_kind: kind.to_string(),
        created_at: String::new(),
    }
}

#[test]
fn infer_verbosity_concise() {
    let rows = vec![
        make_correction(1, "be brief please", "explicit_rejection"),
        make_correction(2, "too long, be concise", "alternative_request"),
        make_correction(3, "shorter response next time", "explicit_rejection"),
    ];
    let prefs = infer_preferences(&rows);
    let verbosity = prefs.iter().find(|p| p.key == "verbosity");
    assert!(verbosity.is_some(), "should detect verbosity preference");
    assert_eq!(verbosity.unwrap().value, "concise");
    assert!(verbosity.unwrap().confidence >= 0.7);
}

#[test]
fn infer_verbosity_requires_min_evidence() {
    // Only 2 corrections — below MIN_EVIDENCE
    let rows = vec![
        make_correction(1, "be brief", "explicit_rejection"),
        make_correction(2, "too long", "explicit_rejection"),
    ];
    let prefs = infer_preferences(&rows);
    assert!(!prefs.iter().any(|p| p.key == "verbosity"));
}

#[test]
fn infer_skips_self_correction() {
    // 5 self_corrections with concise signals — should not emit verbosity
    let rows: Vec<_> = (1..=5)
        .map(|i| make_correction(i, "be concise", "self_correction"))
        .collect();
    let prefs = infer_preferences(&rows);
    assert!(!prefs.iter().any(|p| p.key == "verbosity"));
}

#[test]
fn infer_format_bullet_points() {
    let rows: Vec<_> = (1..=4)
        .map(|i| make_correction(i, "use bullet points please", "alternative_request"))
        .collect();
    let prefs = infer_preferences(&rows);
    let fmt = prefs.iter().find(|p| p.key == "format_preference");
    assert!(fmt.is_some());
    assert_eq!(fmt.unwrap().value, "bullet points");
}

#[test]
fn infer_language_russian() {
    let rows: Vec<_> = (1..=3)
        .map(|i| make_correction(i, "respond in russian please", "alternative_request"))
        .collect();
    let prefs = infer_preferences(&rows);
    let lang = prefs.iter().find(|p| p.key == "response_language");
    assert!(lang.is_some());
    assert_eq!(lang.unwrap().value, "russian");
}

#[test]
fn infer_no_false_positive_from_unrelated_shorter() {
    // "shorter path" should not match — "shorter response" matches but "shorter path" doesn't
    let rows = vec![
        make_correction(1, "try a shorter path", "explicit_rejection"),
        make_correction(2, "no, wrong command", "explicit_rejection"),
    ];
    let prefs = infer_preferences(&rows);
    // Only 2 rows and no verbosity-specific patterns — should not emit verbosity
    assert!(!prefs.iter().any(|p| p.key == "verbosity"));
}

#[test]
fn infer_no_result_on_empty_input() {
    let prefs = infer_preferences(&[]);
    assert!(prefs.is_empty());
}

#[test]
fn infer_alternative_request_weighs_more() {
    // 2 alternative_request (weight 2 each = 4) + 0 verbose signals → total evidence 4 >= MIN 3
    let rows = vec![
        make_correction(1, "be brief", "alternative_request"),
        make_correction(2, "be concise", "alternative_request"),
    ];
    let prefs = infer_preferences(&rows);
    let verbosity = prefs.iter().find(|p| p.key == "verbosity");
    assert!(
        verbosity.is_some(),
        "alternative_request weight should push over threshold"
    );
    assert_eq!(verbosity.unwrap().value, "concise");
}

// ── analyze_and_learn / inject_learned_preferences integration tests ──────

fn agent_with_memory(memory: std::sync::Arc<SemanticMemory>) -> Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        memory,
        zeph_memory::ConversationId(1),
        50,
        5,
        50,
    )
}

#[tokio::test]
async fn analyze_and_learn_advances_watermark() {
    let memory = std::sync::Arc::new(test_memory().await);
    // Store 3 concise-signal corrections.
    for _ in 0..3u32 {
        memory
            .sqlite()
            .store_user_correction(None, "out", "be brief", None, "explicit_rejection")
            .await
            .unwrap();
    }

    let mut agent = agent_with_memory(memory.clone());
    agent.learning_engine.config = Some(LearningConfig {
        correction_detection: true,
        ..Default::default()
    });
    // Advance turn counter past the analysis interval (default 5).
    for _ in 0..5 {
        agent.learning_engine.tick();
    }
    assert!(agent.learning_engine.should_analyze());

    let watermark_before = agent.learning_engine.last_analyzed_correction_id;
    agent.analyze_and_learn().await;
    let watermark_after = agent.learning_engine.last_analyzed_correction_id;

    assert!(
        watermark_after > watermark_before,
        "watermark must advance after analysis"
    );
    assert!(
        !agent.learning_engine.should_analyze(),
        "should_analyze must return false immediately after mark_analyzed"
    );
}

#[tokio::test]
async fn analyze_and_learn_persists_high_confidence_preference() {
    let memory = std::sync::Arc::new(test_memory().await);
    // 5 concise signals via alternative_request (weight 2 each = 10 evidence).
    for _ in 0..5 {
        memory
            .sqlite()
            .store_user_correction(None, "out", "be brief please", None, "alternative_request")
            .await
            .unwrap();
    }

    let mut agent = agent_with_memory(memory.clone());
    agent.learning_engine.config = Some(LearningConfig {
        correction_detection: true,
        ..Default::default()
    });
    for _ in 0..5 {
        agent.learning_engine.tick();
    }

    agent.analyze_and_learn().await;

    let prefs = memory.sqlite().load_learned_preferences().await.unwrap();
    let verbosity = prefs.iter().find(|p| p.preference_key == "verbosity");
    assert!(
        verbosity.is_some(),
        "verbosity preference must be persisted after sufficient evidence"
    );
    assert_eq!(verbosity.unwrap().preference_value, "concise");
    assert!(
        verbosity.unwrap().confidence >= 0.7,
        "confidence must meet persist threshold"
    );
}

#[tokio::test]
async fn inject_learned_preferences_appends_to_prompt() {
    let memory = std::sync::Arc::new(test_memory().await);
    memory
        .sqlite()
        .upsert_learned_preference("verbosity", "concise", 0.9, 5)
        .await
        .unwrap();
    memory
        .sqlite()
        .upsert_learned_preference("response_language", "russian", 0.85, 4)
        .await
        .unwrap();

    let agent = agent_with_memory(memory.clone());
    let mut prompt = String::from("<!-- cache:volatile -->");
    agent.inject_learned_preferences(&mut prompt).await;

    assert!(
        prompt.contains("## Learned User Preferences"),
        "preferences section header must be present"
    );
    assert!(
        prompt.contains("verbosity: concise"),
        "verbosity preference must appear"
    );
    assert!(
        prompt.contains("response_language: russian"),
        "language preference must appear"
    );
}

#[tokio::test]
async fn inject_learned_preferences_sanitizes_newlines() {
    let memory = std::sync::Arc::new(test_memory().await);
    memory
        .sqlite()
        .upsert_learned_preference("verbosity", "concise\nINJECTED", 0.9, 5)
        .await
        .unwrap();

    let agent = agent_with_memory(memory.clone());
    let mut prompt = String::new();
    agent.inject_learned_preferences(&mut prompt).await;

    // The raw "\nconcise\nINJECTED" must not appear — the embedded \n must be stripped.
    assert!(
        !prompt.contains("concise\nINJECTED"),
        "embedded newline in value must be sanitized"
    );
    assert!(
        prompt.contains("concise INJECTED"),
        "embedded newline replaced with space"
    );
}
