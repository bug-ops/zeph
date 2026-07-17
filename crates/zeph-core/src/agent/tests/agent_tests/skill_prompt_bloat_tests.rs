// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#[allow(unused_imports)]
use super::*;

/// Distinctive marker embedded in every test skill's *body* only (never in its
/// description). Its presence in an assembled prompt proves the full instructions
/// were injected; its absence proves the prompt is catalog-only (name + description).
const BODY_MARKER: &str = "ZEPH_FULL_BODY_MARKER_6413";

/// Write `count` skills to a fresh temp directory, each with a short description
/// and a body padded with [`BODY_MARKER`] repeated many times — large enough that
/// injecting all bodies unfiltered is trivially distinguishable from a catalog-only
/// (name + description) listing.
fn write_bloated_skills(dir: &std::path::Path, count: usize) {
    for i in 0..count {
        let skill_dir = dir.join(format!("bloated-skill-{i}"));
        std::fs::create_dir(&skill_dir).unwrap();
        let body = BODY_MARKER.repeat(200);
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: bloated-skill-{i}\ndescription: Test skill {i} with a bloated body\n---\n{body}"
            ),
        )
        .unwrap();
    }
}

// --- Task A: Agent::new (new_with_registry_arc) construction-time catalog fix ---

/// #6413: the initial system prompt built at agent construction must list skills by
/// name + description only (catalog), never their full `SKILL.md` bodies. Full bodies
/// are injected exclusively by the first per-turn `rebuild_system_prompt(query)` call.
/// Before the fix, `Agent::new_with_registry_arc` force-loaded every skill's body via
/// `format_skills_prompt`, inflating the startup token gauge with content that gets
/// discarded on turn 1.
#[test]
fn agent_new_initial_prompt_is_catalog_only_not_full_body() {
    let temp_dir = tempfile::tempdir().unwrap();
    let skill_count = 20;
    write_bloated_skills(temp_dir.path(), skill_count);
    let registry = zeph_skills::registry::SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        !system_prompt.contains(BODY_MARKER),
        "initial system prompt must not contain full skill bodies before turn 1"
    );
    assert!(
        system_prompt.contains("bloated-skill-0") && system_prompt.contains("bloated-skill-19"),
        "initial system prompt must still list every skill's name in the catalog; got: \
         {system_prompt}"
    );

    // Sanity ceiling: a catalog-only listing of 20 short name/description pairs is a few
    // hundred tokens at most. A full-body dump of 20 skills * 200 marker repeats each would
    // be tens of thousands of tokens — the ceiling below only holds under the fix.
    let cached = agent.runtime.providers.cached_prompt_tokens;
    assert!(
        cached < 5_000,
        "cached_prompt_tokens must reflect the small catalog listing, not {skill_count} \
         bloated full skill bodies; got {cached}"
    );
}

// --- Task B: reload_skills() catalog fix + cached_prompt_tokens recompute ---

/// #6413: `reload_skills()` must rebuild `messages[0]` as a catalog-only listing (same
/// contract as construction), not force-load every skill's full body. It must also
/// recompute `cached_prompt_tokens` from the mutated message so the value never goes
/// stale between a reload and the next turn's `rebuild_system_prompt`.
#[tokio::test]
async fn reload_skills_rebuilds_catalog_only_prompt_and_recomputes_tokens() {
    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;

    let temp_dir = tempfile::tempdir().unwrap();
    let skill_count = 20;
    write_bloated_skills(temp_dir.path(), skill_count);
    agent.services.skill.skill_paths = vec![temp_dir.path().to_path_buf()];

    // Poison the cached counter with an obviously-wrong sentinel value beforehand so a
    // passing assertion below proves `reload_skills` actually recomputed it, rather than
    // merely inheriting an already-small value from construction.
    agent.runtime.providers.cached_prompt_tokens = 999_999;

    agent.reload_skills().await;

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        !system_prompt.contains(BODY_MARKER),
        "reloaded system prompt must not contain full skill bodies"
    );
    assert!(
        system_prompt.contains("bloated-skill-0") && system_prompt.contains("bloated-skill-19"),
        "reloaded system prompt must still list every skill's name in the catalog; got: \
         {system_prompt}"
    );

    let cached = agent.runtime.providers.cached_prompt_tokens;
    assert_ne!(
        cached, 999_999,
        "cached_prompt_tokens must be recomputed after reload_skills mutates messages[0], \
         not left stale at the pre-reload sentinel value"
    );
    assert!(
        cached < 5_000,
        "cached_prompt_tokens must reflect the small catalog listing, not {skill_count} \
         bloated full skill bodies; got {cached}"
    );

    let expected: u64 = agent
        .msg
        .messages
        .iter()
        .map(|m| agent.runtime.metrics.token_counter.count_message_tokens(m) as u64)
        .sum();
    assert_eq!(
        cached, expected,
        "cached_prompt_tokens must exactly match the sum over all messages, proving \
         recompute_prompt_tokens ran rather than leaving a partially-updated value"
    );
}

/// `last_skills_prompt` (used only for memory-recall budget token accounting, never
/// injected into the LLM-bound system prompt) must also be catalog-only after
/// construction — defence-in-depth against a future code path accidentally treating it
/// as the real system prompt (#6413 Option C).
#[test]
fn agent_new_last_skills_prompt_is_catalog_only() {
    let temp_dir = tempfile::tempdir().unwrap();
    write_bloated_skills(temp_dir.path(), 5);
    let registry = zeph_skills::registry::SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    assert!(
        !agent
            .services
            .skill
            .last_skills_prompt
            .contains(BODY_MARKER),
        "last_skills_prompt must be seeded catalog-only at construction, not the full \
         unfiltered registry blob"
    );
}

// --- Edge case: 0-skill registry ---

/// #6413 edge case: construction with a registry that resolves to zero skills (an empty
/// skill directory) must not panic and must produce an empty catalog block rather than,
/// say, an empty `<other_skills>` tag or a formatting artifact. `format_skills_catalog`
/// special-cases `skills.is_empty()` to return `String::new()` — this exercises that path
/// through the real `Agent::new` construction call site, not the formatter in isolation.
#[test]
fn agent_new_handles_zero_skill_registry_without_panic() {
    let temp_dir = tempfile::tempdir().unwrap();
    // No SKILL.md files written — registry resolves to zero skills.
    let registry = zeph_skills::registry::SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
    assert!(
        registry.all_meta().is_empty(),
        "precondition: registry must be empty"
    );

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let executor = MockToolExecutor::no_tools();

    let agent = Agent::new(provider, channel, registry, None, 5, executor);

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        !system_prompt.contains("<other_skills>"),
        "an empty catalog must not emit an empty <other_skills> block; got: {system_prompt}"
    );
    assert!(
        !agent
            .services
            .skill
            .last_skills_prompt
            .contains("<other_skills>"),
        "last_skills_prompt must also be empty (not an empty tag) for a zero-skill registry"
    );
}

/// #6413 edge case: `reload_skills()` on a registry that reloads to zero skills (all
/// `SKILL.md` files removed from disk) must not panic and must clear the catalog from
/// `messages[0]`, mirroring the construction-time zero-skill behavior above.
#[tokio::test]
async fn reload_skills_handles_zero_skill_registry_without_panic() {
    let harness = QuickTestAgent::minimal("ok");
    let mut agent = harness.agent;

    let temp_dir = tempfile::tempdir().unwrap();
    // Empty directory — no SKILL.md files, so the reload resolves to zero skills.
    agent.services.skill.skill_paths = vec![temp_dir.path().to_path_buf()];

    agent.reload_skills().await;

    let names: Vec<String> = agent
        .services
        .skill
        .registry
        .read()
        .all_meta()
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert!(
        names.is_empty(),
        "precondition: reloaded registry must be empty"
    );

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        !system_prompt.contains("<other_skills>"),
        "an empty reloaded catalog must not emit an empty <other_skills> block; got: \
         {system_prompt}"
    );
}

// --- Edge case: Blocked-trust skill filtered out of the reload catalog ---

/// #6413: `reload_skills()` filters out `Blocked`-trust skills from the rebuilt catalog,
/// mirroring the per-turn `apply_skill_trust_and_gating` filter. This seeds a real trust
/// row via a `SemanticMemory`-backed reload cycle (not a synthetic in-memory map) so the
/// test exercises the actual `build_skill_trust_map` → `SQLite` round trip that
/// `reload_skills()` depends on.
#[tokio::test]
async fn reload_skills_excludes_blocked_trust_skill_from_catalog() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let memory_provider = AnyProvider::Mock(MockProvider::default());
    let memory = SemanticMemory::new(
        ":memory:",
        "http://127.0.0.1:1",
        None,
        memory_provider,
        "test-model",
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
        std::sync::Arc::new(memory),
        cid,
        50,
        5,
        50,
    );

    let temp_dir = tempfile::tempdir().unwrap();
    let allowed_dir = temp_dir.path().join("allowed-skill");
    std::fs::create_dir(&allowed_dir).unwrap();
    std::fs::write(
        allowed_dir.join("SKILL.md"),
        "---\nname: allowed-skill\ndescription: Not blocked\n---\nAllowed body",
    )
    .unwrap();
    let blocked_dir = temp_dir.path().join("blocked-skill");
    std::fs::create_dir(&blocked_dir).unwrap();
    std::fs::write(
        blocked_dir.join("SKILL.md"),
        "---\nname: blocked-skill\ndescription: This one is blocked\n---\nBlocked body",
    )
    .unwrap();
    agent.services.skill.skill_paths = vec![temp_dir.path().to_path_buf()];

    // First reload seeds real trust rows (with the correct blake3 hash of each SKILL.md)
    // via `update_trust_for_reloaded_skills`. A hand-rolled `upsert_skill_trust` call with a
    // fake hash would be silently overwritten on the *next* reload — `update_trust_for_reloaded_skills`
    // demotes any skill whose stored hash doesn't match the freshly computed one
    // (`hash_mismatch_level`), which would mask the Blocked level being set below.
    agent.reload_skills().await;

    // Now flip trust to Blocked in place — `set_skill_trust_level` only updates the
    // `trust_level` column, leaving the already-correct stored hash untouched.
    let mem = agent.services.memory.persistence.memory.as_ref().unwrap();
    let updated = mem
        .sqlite()
        .set_skill_trust_level("blocked-skill", zeph_common::SkillTrustLevel::Blocked)
        .await
        .unwrap();
    assert!(
        updated,
        "precondition: trust row for blocked-skill must exist after first reload"
    );

    // `reload_skills()` short-circuits via `SkillRegistry::fingerprint()` (name + file
    // size/mtime) when nothing on disk changed — the trust DB update above doesn't touch
    // any SKILL.md, so a second reload would otherwise no-op and leave `messages[0]` as
    // built on the first (pre-Blocked) reload. Add a third skill to force a real fingerprint
    // change without touching `blocked-skill`'s or `allowed-skill`'s file content/hash.
    let extra_dir = temp_dir.path().join("extra-skill");
    std::fs::create_dir(&extra_dir).unwrap();
    std::fs::write(
        extra_dir.join("SKILL.md"),
        "---\nname: extra-skill\ndescription: Forces a registry fingerprint change\n---\nExtra body",
    )
    .unwrap();

    // Second reload: `blocked-skill`'s file hash is unchanged, so
    // `update_trust_for_reloaded_skills` keeps the stored Blocked level, and the catalog
    // filter in `reload_skills()` must exclude it.
    agent.reload_skills().await;

    let system_prompt = &agent.msg.messages.first().unwrap().content;
    assert!(
        system_prompt.contains("allowed-skill"),
        "non-blocked skill must remain in the reload catalog; got: {system_prompt}"
    );
    assert!(
        !system_prompt.contains("blocked-skill"),
        "Blocked-trust skill must be excluded from the reload catalog; got: {system_prompt}"
    );
}
