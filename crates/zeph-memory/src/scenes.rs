// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemScene` consolidation (#2332).
//!
//! Groups semantically related semantic-tier messages into stable entity profiles (scenes).
//! Runs as a separate background loop, decoupled from tier promotion timing.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::types::{MemSceneId, MessageId};
use zeph_common::math::cosine_similarity;

/// A `MemScene` groups semantically related semantic-tier messages with a stable entity profile.
///
/// Scenes are created and updated by [`start_scene_consolidation_loop`] and can be
/// listed via [`list_scenes`].
#[derive(Debug, Clone)]
pub struct MemScene {
    /// `SQLite` row ID of the scene.
    pub id: MemSceneId,
    /// Short human-readable label for the scene (e.g. `"Rust programming"`).
    pub label: String,
    /// LLM-generated entity profile summarising the scene's members.
    pub profile: String,
    /// Number of messages currently assigned to this scene.
    pub member_count: u32,
    /// Unix timestamp when the scene was first created.
    pub created_at: i64,
    /// Unix timestamp of the last profile update.
    pub updated_at: i64,
}

/// Configuration for the scene consolidation background loop.
#[derive(Debug, Clone)]
pub struct SceneConfig {
    /// Enable or disable the scene consolidation loop.
    pub enabled: bool,
    /// Minimum cosine similarity for two messages to be assigned to the same scene.
    pub similarity_threshold: f32,
    /// Maximum number of unassigned messages to process per sweep.
    pub batch_size: usize,
    /// How often to run a sweep, in seconds.
    pub sweep_interval_secs: u64,
}

/// Start the background scene consolidation loop.
///
/// Each sweep clusters unassigned semantic-tier messages into `MemScenes`.
/// Runs independently from the tier promotion loop.
pub async fn start_scene_consolidation_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: SceneConfig,
    cancel: CancellationToken,
) {
    if !config.enabled {
        tracing::debug!("scene consolidation disabled (tiers.scene_enabled = false)");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs));
    // Skip first tick to avoid running immediately at startup.
    ticker.tick().await;

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("scene consolidation loop shutting down");
                return;
            }
            _ = ticker.tick() => {}
        }

        tracing::debug!("scene consolidation: starting sweep");
        let start = std::time::Instant::now();

        match consolidate_scenes(&store, &provider, &config).await {
            Ok(stats) => {
                tracing::info!(
                    candidates = stats.candidates,
                    scenes_created = stats.scenes_created,
                    messages_assigned = stats.messages_assigned,
                    elapsed_ms = start.elapsed().as_millis(),
                    "scene consolidation: sweep complete"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "scene consolidation: sweep failed, will retry"
                );
            }
        }
    }
}

/// Stats collected during a single scene consolidation sweep.
#[derive(Debug, Default)]
pub struct SceneStats {
    pub candidates: usize,
    pub scenes_created: usize,
    pub messages_assigned: usize,
}

/// Execute one full scene consolidation sweep.
///
/// # Errors
///
/// Returns an error if the `SQLite` query fails. LLM and embedding errors are logged but skipped.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.consolidate_scenes", skip_all)
)]
pub async fn consolidate_scenes(
    store: &SqliteStore,
    provider: &AnyProvider,
    config: &SceneConfig,
) -> Result<SceneStats, MemoryError> {
    let candidates = store
        .find_unscened_semantic_messages(config.batch_size)
        .await?;

    if candidates.len() < 2 {
        return Ok(SceneStats::default());
    }

    let mut stats = SceneStats {
        candidates: candidates.len(),
        ..SceneStats::default()
    };

    // Embed all candidates.
    let mut embedded: Vec<(MessageId, String, Vec<f32>)> = Vec::with_capacity(candidates.len());
    if provider.supports_embeddings() {
        for (msg_id, content) in candidates {
            match provider.embed(&content).await {
                Ok(vec) => embedded.push((msg_id, content, vec)),
                Err(e) => {
                    tracing::warn!(
                        message_id = msg_id.0,
                        error = %e,
                        "scene consolidation: failed to embed candidate, skipping"
                    );
                }
            }
        }
    } else {
        return Ok(stats);
    }

    if embedded.len() < 2 {
        return Ok(stats);
    }

    // Cluster by cosine similarity.
    let clusters = cluster_messages(embedded, config.similarity_threshold);

    for cluster in clusters {
        if cluster.len() < 2 {
            continue;
        }

        let contents: Vec<&str> = cluster.iter().map(|(_, c, _)| c.as_str()).collect();
        let msg_ids: Vec<MessageId> = cluster.iter().map(|(id, _, _)| *id).collect();

        match generate_scene_label_and_profile(provider, &contents).await {
            Ok((label, profile)) => {
                let label = label.chars().take(100).collect::<String>();
                let profile = profile.chars().take(2000).collect::<String>();
                match store.insert_mem_scene(&label, &profile, &msg_ids).await {
                    Ok(_scene_id) => {
                        stats.scenes_created += 1;
                        stats.messages_assigned += msg_ids.len();
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            cluster_size = msg_ids.len(),
                            "scene consolidation: failed to insert scene"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    cluster_size = msg_ids.len(),
                    "scene consolidation: LLM label generation failed, skipping cluster"
                );
            }
        }
    }

    Ok(stats)
}

fn cluster_messages(
    candidates: Vec<(MessageId, String, Vec<f32>)>,
    threshold: f32,
) -> Vec<Vec<(MessageId, String, Vec<f32>)>> {
    let mut clusters: Vec<Vec<(MessageId, String, Vec<f32>)>> = Vec::new();

    'outer: for candidate in candidates {
        for cluster in &mut clusters {
            let rep = &cluster[0].2;
            if cosine_similarity(&candidate.2, rep) >= threshold {
                cluster.push(candidate);
                continue 'outer;
            }
        }
        clusters.push(vec![candidate]);
    }

    clusters
}

async fn generate_scene_label_and_profile(
    provider: &AnyProvider,
    contents: &[&str],
) -> Result<(String, String), MemoryError> {
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let bullet_list: String = contents
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    let system_content = "You are a memory scene architect. \
        Given a set of related semantic facts, generate:\n\
        1. A short label (5 words max) identifying the core entity or topic.\n\
        2. A 2-3 sentence entity profile summarizing the key facts.\n\
        Respond in JSON: {\"label\": \"...\", \"profile\": \"...\"}";

    let user_content =
        format!("Generate a label and profile for these related facts:\n\n{bullet_list}");

    let messages = vec![
        Message {
            role: Role::System,
            content: system_content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user_content,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let result = tokio::time::timeout(Duration::from_secs(15), provider.chat(&messages))
        .await
        .map_err(|_| MemoryError::Other("scene LLM call timed out after 15s".into()))?
        .map_err(MemoryError::Llm)?;

    parse_label_profile(&result)
}

fn parse_label_profile(response: &str) -> Result<(String, String), MemoryError> {
    // Try JSON parsing first.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(response) {
        let label = val
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let profile = val
            .get("profile")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if !label.is_empty() && !profile.is_empty() {
            return Ok((label, profile));
        }
    }
    // Fallback: treat first line as label, rest as profile.
    let trimmed = response.trim();
    let mut lines = trimmed.splitn(2, '\n');
    let label = lines.next().unwrap_or("").trim().to_owned();
    let profile = lines.next().unwrap_or(trimmed).trim().to_owned();
    if label.is_empty() {
        return Err(MemoryError::Other("scene LLM returned empty label".into()));
    }
    let profile = if profile.is_empty() {
        label.clone()
    } else {
        profile
    };
    Ok((label, profile))
}

/// List all `MemScenes` from the store.
///
/// # Errors
///
/// Returns an error if the `SQLite` query fails.
pub async fn list_scenes(store: &SqliteStore) -> Result<Vec<MemScene>, MemoryError> {
    store.list_mem_scenes().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_messages_groups_similar() {
        let v1 = vec![1.0f32, 0.0, 0.0];
        let v2 = vec![1.0f32, 0.0, 0.0];
        let v3 = vec![0.0f32, 1.0, 0.0];

        let candidates = vec![
            (MessageId(1), "a".to_owned(), v1),
            (MessageId(2), "b".to_owned(), v2),
            (MessageId(3), "c".to_owned(), v3),
        ];

        let clusters = cluster_messages(candidates, 0.80);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].len(), 2);
        assert_eq!(clusters[1].len(), 1);
    }

    #[test]
    fn parse_label_profile_valid_json() {
        let json = r#"{"label": "Rust Auth JWT", "profile": "The project uses JWT for auth."}"#;
        let (label, profile) = parse_label_profile(json).unwrap();
        assert_eq!(label, "Rust Auth JWT");
        assert_eq!(profile, "The project uses JWT for auth.");
    }

    #[test]
    fn parse_label_profile_fallback_lines() {
        let text = "Rust Auth\nJWT tokens used for authentication. Rate limited at 100 rps.";
        let (label, profile) = parse_label_profile(text).unwrap();
        assert_eq!(label, "Rust Auth");
        assert!(profile.contains("JWT"));
    }

    #[test]
    fn parse_label_profile_empty_fails() {
        assert!(parse_label_profile("").is_err());
    }
}
