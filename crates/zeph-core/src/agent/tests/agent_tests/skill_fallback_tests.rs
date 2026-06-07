// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression tests for `skill_fallback_mode` compact-prompt paths in `assembly.rs`.
//!
//! Three scenarios trigger `format_skills_prompt_compact`:
//! 1. No embedding matcher configured → immediate compact fallback.
//! 2. Matcher present but embedding provider returns infrastructure error → compact fallback.
//! 3. Healthy matcher and provider → full `<instructions>` prompt.
//!
//! Spec ref: assembly.rs lines 588–810, `skill_fallback_mode` flag.

use std::sync::{Arc, Mutex};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{Message, Role};
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::registry::SkillRegistry;

use crate::agent::Agent;

use super::{MockChannel, MockToolExecutor};

/// Create a test registry and return the tempdir so the skill files stay on disk.
///
/// `create_test_registry()` from common.rs drops the `TempDir`, which deletes the
/// `SKILL.md` file.  `registry.skill(name)` lazily reads from disk, so if the dir
/// is gone the call returns `Err` and the skill is filtered from `active_skills`.
/// Keeping the `TempDir` alive for the duration of the test prevents this.
fn create_registry_with_live_dir() -> (SkillRegistry, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_dir = temp_dir.path().join("test-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: A test skill\n---\n<instructions>\nTest skill body\n</instructions>",
    )
    .unwrap();
    let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
    (registry, temp_dir)
}

/// Extract the system prompt content from a recorded message log.
fn extract_system_prompt(recorded: &Arc<Mutex<Vec<Vec<Message>>>>) -> String {
    let guard = recorded.lock().unwrap();
    guard
        .iter()
        .flatten()
        .find(|m| m.role == Role::System)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

// ---------- Test 1: no matcher → compact fallback ----------

/// When no embedding matcher is configured, `assembly.rs` must inject skills via the
/// compact `<available_skills mode="compact">` path rather than the full `<instructions>` form.
#[tokio::test]
async fn skill_fallback_compact_when_no_matcher() {
    let (mock, recorded) = MockProvider::with_responses(vec!["ok".to_string()]).with_recording();
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["hello".to_string()]);
    let (registry, _dir) = create_registry_with_live_dir();
    let executor = MockToolExecutor::no_tools();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    // No matcher set → services.skill.matcher = None (default).

    agent.run().await.unwrap();

    let sys = extract_system_prompt(&recorded);
    assert!(
        sys.contains("mode=\"compact\""),
        "expected compact skill prompt when no matcher is configured, got system prompt: {sys}"
    );
    assert!(
        !sys.contains("<instructions>"),
        "full instructions prompt must not appear in compact fallback mode, got: {sys}"
    );
}

// ---------- Test 2: embed infra error → compact fallback ----------

/// When the embedding provider returns an infrastructure error during `match_skills`,
/// `assembly.rs` must fall back to the compact prompt rather than silently running without skills.
#[tokio::test]
async fn skill_fallback_compact_on_embed_infra_error() {
    // M1: Agent embedding provider fails (LlmError::InvalidInput on embed()).
    //     Matcher is built separately with a succeeding closure so it initialises correctly.
    let (chat_mock, recorded) =
        MockProvider::with_responses(vec!["ok".to_string()]).with_recording();
    // Failing embed provider — used both as chat+embed provider via AnyProvider::Mock.
    // embed() returns InvalidInput due to with_embed_invalid_input().
    let embed_failing = AnyProvider::Mock(chat_mock.with_embed_invalid_input());

    let channel = MockChannel::new(vec!["hello".to_string()]);
    let executor = MockToolExecutor::no_tools();
    let (registry, _dir) = create_registry_with_live_dir();

    let mut agent = Agent::new(embed_failing, channel, registry, None, 5, executor);

    // Read meta from the agent's own registry (which wraps SkillRegistry in Arc<RwLock<>>).
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
        let registry_guard = agent.services.skill.registry.read();
        registry_guard.all_meta().into_iter().cloned().collect()
    };
    // Matcher built with a succeeding embed closure so SkillMatcher::new completes.
    let succeed_embed_fn = |_text: &str| -> zeph_skills::matcher::EmbedFuture {
        Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
    };
    let matcher = SkillMatcher::new(&all_meta_owned.iter().collect::<Vec<_>>(), succeed_embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory);
    // Inject the healthy in-memory matcher. assembly.rs will call agent.embedding_provider.embed()
    // which returns InvalidInput → MatchResult::InfraError → skill_fallback_mode = true.
    agent.services.skill.matcher = matcher;

    agent.run().await.unwrap();

    let sys = extract_system_prompt(&recorded);
    assert!(
        sys.contains("mode=\"compact\""),
        "expected compact fallback on embed infra error, got system prompt: {sys}"
    );
}

// ---------- Test 3: healthy matcher → full prompt ----------

/// When a healthy matcher and embedding provider are wired, `assembly.rs` must inject the
/// full skill prompt containing `<instructions>` rather than the compact fallback.
#[tokio::test]
async fn skill_fallback_full_when_matcher_healthy() {
    let (mock, recorded) = MockProvider::with_responses(vec!["ok".to_string()])
        .with_embedding(vec![1.0_f32, 0.0])
        .with_recording();
    let provider = AnyProvider::Mock(mock);

    let channel = MockChannel::new(vec!["hello".to_string()]);
    let executor = MockToolExecutor::no_tools();
    let (registry, _dir) = create_registry_with_live_dir();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
        let registry_guard = agent.services.skill.registry.read();
        registry_guard.all_meta().into_iter().cloned().collect()
    };
    let embed_fn = |_text: &str| -> zeph_skills::matcher::EmbedFuture {
        Box::pin(async { Ok(vec![1.0_f32, 0.0]) })
    };
    let matcher = SkillMatcher::new(&all_meta_owned.iter().collect::<Vec<_>>(), embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory);
    agent.services.skill.matcher = matcher;

    agent.run().await.unwrap();

    let sys = extract_system_prompt(&recorded);
    assert!(
        sys.contains("<instructions>"),
        "expected full skill prompt with <instructions> when matcher is healthy, got: {sys}"
    );
    assert!(
        !sys.contains("mode=\"compact\""),
        "compact fallback must not appear when matcher is healthy, got: {sys}"
    );
}
