// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Periodic heuristic promotion background task (`AutoSkill A6`, spec 061).
//!
//! When `heuristic_promotion_enabled = true` in `[skills.learning]`, a background
//! `tokio` task is spawned at agent startup. It sleeps for the configured interval,
//! then scans `skill_heuristics` for skills with enough heuristics to trigger an
//! LLM evaluation. Evaluation results are written as quarantined drafts and recorded
//! in `skill_heuristic_promotions` for idempotency.
//!
//! The task is aborted (not awaited) at agent shutdown — this is safe because
//! `skill_heuristic_promotions` uses `INSERT OR IGNORE` on the primary key, so
//! a partially written evaluation is retried on next startup.

use std::path::PathBuf;
use std::time::Duration;

use tracing::Instrument as _;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_skills::embedding::SkillEmbedding;
use zeph_skills::generator::{GeneratedSkill, SkillGenerator};
use zeph_skills::merger::{MergeDecision, decide, find_nearest};
use zeph_skills::promoter::{
    PROMOTION_SYSTEM_PROMPT, PromotionRecommendation, build_promotion_prompt, compute_batch_hash,
    parse_promotion_response,
};
use zeph_skills::scanner::scan_skill_body;

use crate::agent::Channel;

/// Initial delay before the first promotion scan.
///
/// Gives the agent time to finish initialization (load skills, connect to DB, etc.)
/// before the first potentially expensive LLM scan.
const INITIAL_DELAY_SECS: u64 = 60;

/// Maximum seconds to wait for the LLM promotion call.
const LLM_TIMEOUT_SECS: u64 = 120;

impl<C: Channel> super::Agent<C> {
    /// Spawn the periodic heuristic promotion background task if configured.
    ///
    /// Called once at agent startup (after learning config is loaded). When
    /// `heuristic_promotion_enabled = false`, this is a no-op.
    pub(super) fn maybe_start_heuristic_promotion(&mut self) {
        let Some(ref learning_cfg) = self.services.learning_engine.config else {
            return;
        };
        if !learning_cfg.heuristic_promotion_enabled {
            return;
        }

        let provider =
            self.resolve_background_provider(learning_cfg.heuristic_promotion_provider.as_str());
        // Reuse the trace extraction embed provider for Add/Merge/Discard flow.
        let embed_provider =
            self.resolve_background_provider(learning_cfg.trace_extraction_embed_provider.as_str());

        let Some(ref output_dir) = self.services.skill.managed_dir else {
            tracing::debug!("heuristic_promotion: no managed_dir configured, skipping");
            return;
        };
        let output_dir = output_dir.clone();

        let interval_hours = learning_cfg.heuristic_promotion_interval_hours;
        let threshold = learning_cfg.heuristic_promotion_threshold;
        let min_confidence = learning_cfg.erl_min_confidence;
        let merge_threshold = learning_cfg.merge_threshold;
        let dedup_threshold = learning_cfg.dedup_threshold;
        let merge_enabled = learning_cfg.skill_merge_enabled;

        let status_tx = self.services.session.status_tx.clone();
        let db_pool = self
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .map(|m| m.sqlite().pool().clone());

        tracing::info!(
            interval_hours,
            threshold,
            "heuristic_promotion: starting background task"
        );

        self.services.learning_engine.heuristic_promotion_handle = Some(tokio::spawn(
            run_promotion_loop(
                provider,
                embed_provider,
                output_dir,
                db_pool,
                interval_hours,
                threshold,
                min_confidence,
                merge_threshold,
                dedup_threshold,
                merge_enabled,
                status_tx,
            )
            .in_current_span(),
        ));
    }
}

/// The periodic background loop for heuristic promotion.
#[allow(clippy::too_many_arguments)]
async fn run_promotion_loop(
    provider: AnyProvider,
    embed_provider: AnyProvider,
    output_dir: PathBuf,
    db_pool: Option<zeph_db::DbPool>,
    interval_hours: u64,
    threshold: u32,
    min_confidence: f64,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    // Delay startup to let the agent finish initialization.
    tokio::time::sleep(Duration::from_secs(INITIAL_DELAY_SECS)).await;

    loop {
        let span = tracing::info_span!("skills.heuristic_promotion.scan");
        run_promotion_scan(
            &provider,
            &embed_provider,
            output_dir.as_path(),
            db_pool.as_ref(),
            threshold,
            min_confidence,
            merge_threshold,
            dedup_threshold,
            merge_enabled,
            status_tx.as_ref(),
        )
        .instrument(span)
        .await;

        tokio::time::sleep(Duration::from_hours(interval_hours)).await;
    }
}

/// Execute a single promotion scan pass across all qualifying skills.
#[allow(clippy::too_many_arguments)]
async fn run_promotion_scan(
    provider: &AnyProvider,
    embed_provider: &AnyProvider,
    output_dir: &std::path::Path,
    db_pool: Option<&zeph_db::DbPool>,
    threshold: u32,
    min_confidence: f64,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
    status_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let Some(pool) = db_pool else {
        tracing::debug!("heuristic_promotion: no DB pool, skipping scan");
        return;
    };

    let store = zeph_memory::store::SqliteStore::from_pool(pool.clone());

    let candidates = match store
        .count_heuristics_by_skill(min_confidence, threshold)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "heuristic_promotion: failed to query qualifying skills");
            return;
        }
    };

    if candidates.is_empty() {
        tracing::debug!("heuristic_promotion: no qualifying skills found");
        return;
    }

    tracing::info!(
        count = candidates.len(),
        "heuristic_promotion: evaluating qualifying skills"
    );

    let mut promoted_count = 0usize;

    for (skill_name, _heuristic_count) in candidates {
        evaluate_skill(
            provider,
            embed_provider,
            output_dir,
            &store,
            &skill_name,
            min_confidence,
            merge_threshold,
            dedup_threshold,
            merge_enabled,
            &mut promoted_count,
        )
        .await;
    }

    if promoted_count > 0 {
        let msg = format!("Heuristic promotion: {promoted_count} draft(s) queued for review");
        tracing::info!("{msg}");
        if let Some(tx) = status_tx {
            let _ = tx.send(msg);
        }
    }
}

/// Evaluate one skill's heuristics and write a quarantined draft if recommended.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn evaluate_skill(
    provider: &AnyProvider,
    embed_provider: &AnyProvider,
    output_dir: &std::path::Path,
    store: &zeph_memory::store::SqliteStore,
    skill_name: &str,
    min_confidence: f64,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
    promoted_count: &mut usize,
) {
    let heuristics = match store
        .load_heuristic_texts_for_promotion(skill_name, min_confidence)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(skill = skill_name, error = %e, "heuristic_promotion: failed to load heuristics");
            return;
        }
    };

    if heuristics.is_empty() {
        return;
    }

    let batch_hash = compute_batch_hash(&heuristics);

    match store
        .promotion_already_evaluated(skill_name, &batch_hash)
        .await
    {
        Ok(true) => {
            tracing::debug!(
                skill = skill_name,
                batch_hash = %batch_hash,
                "heuristic_promotion: batch already evaluated, skipping"
            );
            return;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(skill = skill_name, error = %e, "heuristic_promotion: idempotency check failed");
            return;
        }
    }

    // Load parent skill body from disk.
    let skill_md_path = output_dir.join(skill_name).join("SKILL.md");
    let parent_body = match tokio::fs::read_to_string(&skill_md_path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(
                skill = skill_name,
                path = %skill_md_path.display(),
                error = %e,
                "heuristic_promotion: parent skill not found on disk, skipping"
            );
            return;
        }
    };

    // Parse parent version for body_enrichment draft versioning.
    let parent_version = zeph_skills::loader::load_skill_meta_from_str(&parent_body)
        .ok()
        .map_or(0, |(m, _)| m.version);

    let user_prompt = build_promotion_prompt(&parent_body, &heuristics);
    let messages = vec![
        Message::from_legacy(Role::System, PROMOTION_SYSTEM_PROMPT),
        Message::from_legacy(Role::User, &user_prompt),
    ];

    let raw_response = {
        let span = tracing::info_span!("skills.heuristic_promotion.llm_call", skill = skill_name);
        match tokio::time::timeout(
            Duration::from_secs(LLM_TIMEOUT_SECS),
            provider.chat(&messages).instrument(span),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(skill = skill_name, error = %e, "heuristic_promotion: LLM call failed");
                let _ = store
                    .record_promotion_evaluation(skill_name, &batch_hash, "none", None)
                    .await;
                return;
            }
            Err(_) => {
                tracing::warn!(
                    skill = skill_name,
                    "heuristic_promotion: LLM call timed out after {LLM_TIMEOUT_SECS}s"
                );
                return;
            }
        }
    };

    let (recommendation, draft_name) = parse_promotion_response(&raw_response);

    let write_span = tracing::info_span!("skills.heuristic_promotion.write", skill = skill_name);
    let written_draft = write_draft(
        output_dir,
        embed_provider,
        skill_name,
        parent_version,
        &recommendation,
        merge_threshold,
        dedup_threshold,
        merge_enabled,
    )
    .instrument(write_span)
    .await;

    let rec_str = match &recommendation {
        PromotionRecommendation::BodyEnrichment { .. } => "body_enrichment",
        PromotionRecommendation::NewSkill { .. } => "new_skill",
        PromotionRecommendation::None => "none",
    };

    let final_draft_name = written_draft.as_deref().or(draft_name.as_deref());

    if let Err(e) = store
        .record_promotion_evaluation(skill_name, &batch_hash, rec_str, final_draft_name)
        .await
    {
        tracing::warn!(skill = skill_name, error = %e, "heuristic_promotion: failed to record evaluation");
    }

    if written_draft.is_some() {
        *promoted_count += 1;
    }
}

/// Timeout for embed calls inside the promotion write path.
const EMBED_TIMEOUT_SECS: u64 = 30;

/// Write a quarantined draft based on the LLM recommendation.
///
/// For `new_skill` recommendations, runs the Add/Merge/Discard flow (spec 057) via
/// embedding similarity before writing to quarantine.
///
/// Returns `Some(draft_name)` when a draft was written, `None` otherwise.
#[allow(clippy::too_many_arguments)]
async fn write_draft(
    output_dir: &std::path::Path,
    embed_provider: &AnyProvider,
    skill_name: &str,
    parent_version: u32,
    recommendation: &PromotionRecommendation,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
) -> Option<String> {
    let generator = SkillGenerator::new(embed_provider.clone(), output_dir.to_path_buf());

    match recommendation {
        PromotionRecommendation::BodyEnrichment { integrated_body } => {
            // Patch version and inject required frontmatter fields.
            let versioned_body = patch_version(integrated_body, parent_version + 1);
            let patched_body =
                patch_frontmatter(&versioned_body, "heuristic_promotion", skill_name);
            let skill = build_generated_skill(skill_name, &patched_body)?;
            match generator.write_quarantined(&skill).await {
                Ok(_) => {
                    tracing::info!(
                        skill = skill_name,
                        draft = skill_name,
                        "heuristic_promotion: body_enrichment draft written"
                    );
                    Some(skill_name.to_string())
                }
                Err(e) => {
                    tracing::warn!(skill = skill_name, error = %e, "heuristic_promotion: failed to write body_enrichment draft");
                    None
                }
            }
        }

        PromotionRecommendation::NewSkill { name, body } => {
            // Spec 061: new skills start at version 0. Inject required frontmatter fields.
            let versioned_body = patch_version(body, 0);
            let patched_body =
                patch_frontmatter(&versioned_body, "heuristic_promotion", skill_name);
            let draft_skill = build_generated_skill(name, &patched_body)?;

            // FR-005: run Add/Merge/Discard (spec 057) before quarantine write.
            let decision = add_merge_discard_decision(
                embed_provider,
                output_dir,
                &draft_skill,
                merge_threshold,
                dedup_threshold,
                merge_enabled,
            )
            .await;

            match decision {
                MergeDecision::Discard => {
                    tracing::debug!(
                        parent_skill = skill_name,
                        candidate = name,
                        "heuristic_promotion: new_skill candidate discarded by dedup"
                    );
                    return None;
                }
                MergeDecision::Merge {
                    ref nearest_name, ..
                } => {
                    tracing::debug!(
                        parent_skill = skill_name,
                        candidate = name,
                        nearest = nearest_name,
                        "heuristic_promotion: new_skill candidate merged, writing quarantined draft"
                    );
                }
                MergeDecision::Add => {}
            }

            match generator.write_quarantined(&draft_skill).await {
                Ok(_) => {
                    tracing::info!(
                        parent_skill = skill_name,
                        draft = name,
                        "heuristic_promotion: new_skill draft written"
                    );
                    Some(name.clone())
                }
                Err(e) => {
                    tracing::warn!(skill = name, error = %e, "heuristic_promotion: failed to write new_skill draft");
                    None
                }
            }
        }

        PromotionRecommendation::None => None,
    }
}

/// Run the Add/Merge/Discard decision for a `new_skill` candidate.
///
/// Loads existing skills from `output_dir`, embeds them and the candidate, then calls
/// [`decide`]. Falls back to [`MergeDecision::Add`] when embedding fails so the
/// quarantined draft is still written for human review.
async fn add_merge_discard_decision(
    embed_provider: &AnyProvider,
    output_dir: &std::path::Path,
    candidate: &GeneratedSkill,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
) -> MergeDecision {
    let span = tracing::info_span!(
        "skills.heuristic_promotion.amd_decision",
        candidate = %candidate.name
    );
    async move {
        // Embed the candidate description.
        let candidate_emb = match tokio::time::timeout(
            Duration::from_secs(EMBED_TIMEOUT_SECS),
            embed_provider.embed(&candidate.meta.description),
        )
        .await
        {
            Ok(Ok(v)) => SkillEmbedding::from_raw(v),
            Ok(Err(e)) => {
                tracing::warn!(candidate = %candidate.name, error = %e, "heuristic_promotion: embed failed, defaulting to Add");
                return MergeDecision::Add;
            }
            Err(_) => {
                tracing::warn!(candidate = %candidate.name, "heuristic_promotion: embed timed out, defaulting to Add");
                return MergeDecision::Add;
            }
        };

        // Load and embed existing skills from the managed directory.
        // SkillRegistry::load uses std::fs blocking I/O; run it off the async thread.
        let output_dir_clone = output_dir.to_path_buf();
        let registry = tokio::task::spawn_blocking(move || {
            zeph_skills::registry::SkillRegistry::load(&[output_dir_clone])
        })
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("heuristic_promotion: spawn_blocking for SkillRegistry::load panicked, using empty registry");
            zeph_skills::registry::SkillRegistry::default()
        });
        let existing_meta: Vec<_> = registry.all_meta().into_iter().cloned().collect();

        let mut existing_embeddings: Vec<(zeph_skills::loader::SkillMeta, SkillEmbedding)> =
            Vec::with_capacity(existing_meta.len());
        let timeout = Duration::from_secs(EMBED_TIMEOUT_SECS);
        for meta in &existing_meta {
            match tokio::time::timeout(timeout, embed_provider.embed(&meta.description)).await {
                Ok(Ok(v)) => existing_embeddings.push((meta.clone(), SkillEmbedding::from_raw(v))),
                Ok(Err(e)) => {
                    tracing::debug!(skill = %meta.name, error = %e, "heuristic_promotion: skipping skill in dedup (embed failed)");
                }
                Err(_) => {
                    tracing::debug!(skill = %meta.name, "heuristic_promotion: skipping skill in dedup (embed timeout)");
                }
            }
        }

        match find_nearest(&candidate_emb, &existing_embeddings) {
            None => MergeDecision::Add,
            Some((nearest, sim)) => decide(sim, merge_threshold, dedup_threshold, merge_enabled, nearest),
        }
    }
    .instrument(span)
    .await
}

/// Construct a `GeneratedSkill` from a name and raw SKILL.md content.
///
/// Runs the injection scanner on the content and warns (but does not block) if
/// patterns are found — the write still proceeds to quarantine for human review.
fn build_generated_skill(name: &str, content: &str) -> Option<GeneratedSkill> {
    match zeph_skills::loader::load_skill_meta_from_str(content) {
        Ok((meta, body)) => {
            let scan = scan_skill_body(&body);
            if scan.has_matches() {
                tracing::warn!(
                    name = name,
                    patterns = ?scan.matched_patterns,
                    "heuristic_promotion: injection patterns detected in draft body (writing to quarantine for human review)"
                );
            }
            Some(GeneratedSkill {
                name: name.to_string(),
                content: content.to_string(),
                meta,
                warnings: if scan.has_matches() {
                    vec![format!(
                        "injection patterns: {}",
                        scan.matched_patterns.join(", ")
                    )]
                } else {
                    vec![]
                },
                has_injection_patterns: scan.has_matches(),
            })
        }
        Err(e) => {
            tracing::warn!(name = name, error = %e, "heuristic_promotion: failed to parse draft frontmatter");
            None
        }
    }
}

/// Inject or overwrite `source` and `parent_skill` fields in SKILL.md frontmatter.
///
/// These fields are required by spec 061 for traceability of promoted skills.
/// Existing values are replaced; missing values are inserted after `name:`.
fn patch_frontmatter(skill_md: &str, source: &str, parent_skill: &str) -> String {
    let Some(after_open) = skill_md.strip_prefix("---") else {
        return skill_md.to_string();
    };
    let Some(close_pos) = after_open.find("---") else {
        return skill_md.to_string();
    };
    let yaml = &after_open[..close_pos];
    let rest = &after_open[close_pos..];

    let mut lines: Vec<String> = yaml
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("source:") && !t.starts_with("parent_skill:")
        })
        .map(str::to_string)
        .collect();

    let insert_after = lines
        .iter()
        .position(|l| l.trim_start().starts_with("name:"))
        .map_or(lines.len(), |p| p + 1);

    lines.insert(insert_after, format!("parent_skill: {parent_skill}"));
    lines.insert(insert_after, format!("source: {source}"));

    format!(
        "---{}---{}",
        lines.join("\n"),
        rest.trim_start_matches("---")
    )
}

/// Replace or insert the `version:` field in SKILL.md frontmatter.
fn patch_version(skill_md: &str, new_version: u32) -> String {
    // Only modify the frontmatter block (between the first pair of `---` delimiters).
    if let Some(after_open) = skill_md.strip_prefix("---")
        && let Some(close_pos) = after_open.find("---")
    {
        let yaml = &after_open[..close_pos];
        let rest = &after_open[close_pos..];

        let new_yaml = if yaml.contains("\nversion:") || yaml.starts_with("version:") {
            yaml.lines()
                .map(|line| {
                    if line.trim_start().starts_with("version:") {
                        format!("version: {new_version}")
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            let mut lines: Vec<String> = yaml.lines().map(str::to_string).collect();
            if let Some(pos) = lines
                .iter()
                .position(|l| l.trim_start().starts_with("name:"))
            {
                lines.insert(pos + 1, format!("version: {new_version}"));
            } else {
                lines.push(format!("version: {new_version}"));
            }
            lines.join("\n")
        };

        return format!("---{new_yaml}{rest}");
    }
    skill_md.to_string()
}

#[cfg(test)]
mod tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_skills::generator::GeneratedSkill;
    use zeph_skills::loader::SkillMeta;
    use zeph_skills::merger::MergeDecision;

    use super::*;

    fn mock_embed_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default().with_embedding(vec![0.1_f32; 384]))
    }

    fn dummy_candidate(name: &str) -> GeneratedSkill {
        GeneratedSkill {
            name: name.to_string(),
            content: format!("---\nname: {name}\ndescription: Test skill.\n---\n\n# Body\n"),
            meta: SkillMeta {
                name: name.to_string(),
                description: "Test skill.".to_string(),
                ..SkillMeta::default()
            },
            warnings: vec![],
            has_injection_patterns: false,
        }
    }

    /// Regression test for #4530: `add_merge_discard_decision` must call
    /// `spawn_blocking` for `SkillRegistry::load` without panicking.
    /// With an empty managed directory there are no existing skills, so the
    /// function should return `MergeDecision::Add`.
    #[tokio::test]
    async fn add_merge_discard_decision_empty_dir_returns_add() {
        let tmp = tempfile::TempDir::new().unwrap();
        let provider = mock_embed_provider();
        let candidate = dummy_candidate("new-skill");

        let decision = add_merge_discard_decision(
            &provider,
            tmp.path(),
            &candidate,
            0.9,  // merge_threshold
            0.98, // dedup_threshold
            true, // merge_enabled
        )
        .await;

        assert!(
            matches!(decision, MergeDecision::Add),
            "expected MergeDecision::Add for empty directory, got {decision:?}"
        );
    }

    #[test]
    fn patch_version_replaces_existing() {
        let md = "---\nname: foo\nversion: 1\ndescription: Foo.\n---\n\n# Body\n";
        let patched = patch_version(md, 2);
        assert!(patched.contains("version: 2"), "patched: {patched}");
        assert!(
            !patched.contains("version: 1"),
            "old version still present: {patched}"
        );
    }

    #[test]
    fn patch_version_inserts_when_missing() {
        let md = "---\nname: foo\ndescription: Foo.\n---\n\n# Body\n";
        let patched = patch_version(md, 3);
        assert!(patched.contains("version: 3"), "patched: {patched}");
    }

    #[test]
    fn patch_version_no_frontmatter_unchanged() {
        let md = "# No frontmatter here\n\nbody\n";
        let patched = patch_version(md, 1);
        assert_eq!(patched, md);
    }

    #[test]
    fn patch_frontmatter_inserts_source_and_parent_skill() {
        let md = "---\nname: foo\ndescription: Foo.\n---\n\n# Body\n";
        let patched = patch_frontmatter(md, "heuristic_promotion", "code-review");
        assert!(
            patched.contains("source: heuristic_promotion"),
            "patched: {patched}"
        );
        assert!(
            patched.contains("parent_skill: code-review"),
            "patched: {patched}"
        );
        assert!(
            patched.contains("name: foo"),
            "name field missing: {patched}"
        );
    }

    #[test]
    fn patch_frontmatter_replaces_existing_source_and_parent_skill() {
        let md = "---\nname: bar\nsource: old_source\nparent_skill: old-parent\ndescription: Bar.\n---\n\n# Body\n";
        let patched = patch_frontmatter(md, "heuristic_promotion", "new-parent");
        assert!(
            patched.contains("source: heuristic_promotion"),
            "patched: {patched}"
        );
        assert!(
            patched.contains("parent_skill: new-parent"),
            "patched: {patched}"
        );
        assert!(
            !patched.contains("old_source"),
            "old source not removed: {patched}"
        );
        assert!(
            !patched.contains("old-parent"),
            "old parent not removed: {patched}"
        );
    }

    #[test]
    fn patch_frontmatter_no_frontmatter_unchanged() {
        let md = "# No frontmatter\n\nbody";
        let patched = patch_frontmatter(md, "heuristic_promotion", "parent");
        assert_eq!(patched, md);
    }
}
