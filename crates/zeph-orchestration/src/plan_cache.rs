// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plan template caching for the LLM planner.
//!
//! Caches completed `TaskGraph` plans as reusable `PlanTemplate` skeletons.
//! On subsequent semantically similar goals, retrieves the closest template
//! and uses a lightweight LLM adaptation call instead of full decomposition.

use blake3;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use zeph_config::PlanCacheConfig;
use zeph_llm::provider::{LlmProvider, Message, Role};

use super::dag;
use super::error::OrchestrationError;
use super::graph::TaskGraph;
use super::planner::{PlannerResponse, convert_response_pub};
use zeph_subagent::SubAgentDef;

/// Structural skeleton of a single task, stripped of all runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateTask {
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_strategy: Option<String>,
    /// Stable kebab-case `task_id` assigned during template extraction.
    pub task_id: String,
}

/// Reusable plan skeleton extracted from a successfully completed `TaskGraph`.
///
/// Contains only the structural information (task titles, descriptions,
/// dependencies, agent hints) — all runtime state (status, results,
/// `retry_count`, `assigned_agent`, timestamps) is stripped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTemplate {
    /// Normalized goal text used for exact-match fallback.
    pub goal: String,
    /// Structural task skeleton.
    pub tasks: Vec<TemplateTask>,
}

impl PlanTemplate {
    /// Extract a `PlanTemplate` from a completed `TaskGraph`.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::PlanningFailed` if the graph has no tasks.
    pub fn from_task_graph(graph: &TaskGraph) -> Result<Self, OrchestrationError> {
        if graph.tasks.is_empty() {
            return Err(OrchestrationError::PlanningFailed(
                "cannot cache a plan with zero tasks".into(),
            ));
        }

        // Build task_id strings indexed by position for depends_on reconstruction.
        let id_to_slug: Vec<String> = graph
            .tasks
            .iter()
            .map(|n| slugify_title(&n.title, n.id.as_u32()))
            .collect();

        let tasks = graph
            .tasks
            .iter()
            .enumerate()
            .map(|(i, node)| TemplateTask {
                title: node.title.clone(),
                description: node.description.clone(),
                agent_hint: node.agent_hint.clone(),
                depends_on: node
                    .depends_on
                    .iter()
                    .map(|dep| id_to_slug[dep.index()].clone())
                    .collect(),
                failure_strategy: node.failure_strategy.map(|fs| fs.to_string()),
                task_id: id_to_slug[i].clone(),
            })
            .collect();

        Ok(Self {
            goal: normalize_goal(&graph.goal),
            tasks,
        })
    }
}

/// Normalize goal text: trim + collapse internal whitespace + lowercase.
///
/// Used consistently for hash computation and embedding input so that
/// trivially different goal strings (capitalization, extra spaces) map
/// to the same cache entry.
#[must_use]
pub fn normalize_goal(text: &str) -> String {
    let trimmed = text.trim();
    let mut result = String::with_capacity(trimmed.len());
    let mut prev_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_space && !result.is_empty() {
                result.push(' ');
                prev_space = true;
            }
        } else {
            for lc in ch.to_lowercase() {
                result.push(lc);
            }
            prev_space = false;
        }
    }
    result
}

/// Compute a BLAKE3 hex hash of a normalized goal string.
#[must_use]
pub fn goal_hash(normalized: &str) -> String {
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

/// Convert a task title + index into a stable kebab-case `task_id` for template use.
fn slugify_title(title: &str, idx: u32) -> String {
    let slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    // Cap at 32 chars, then append index to ensure uniqueness.
    let capped = if slug.len() > 32 { &slug[..32] } else { &slug };
    // Trim trailing dashes after cap.
    let capped = capped.trim_end_matches('-');
    if capped.is_empty() {
        format!("task-{idx}")
    } else {
        format!("{capped}-{idx}")
    }
}

/// Serialize an `f32` slice to a `Vec<u8>` BLOB using explicit little-endian encoding.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize an `f32` slice from a BLOB using chunk-based little-endian decoding.
///
/// Returns `None` and logs a warning if the BLOB length is not a multiple of 4.
/// Does not require aligned memory — safe for `Vec<u8>` returned by `SQLite`.
fn blob_to_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if !blob.len().is_multiple_of(4) {
        tracing::warn!(
            len = blob.len(),
            "plan cache: embedding blob length not a multiple of 4"
        );
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("chunk is exactly 4 bytes")))
            .collect(),
    )
}

fn unix_now() -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }
}

/// Error type for plan cache operations.
#[derive(Debug, thiserror::Error)]
pub enum PlanCacheError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("plan template extraction failed: {0}")]
    Extraction(String),
}

/// Plan template cache backed by `SQLite` with in-process cosine similarity search.
///
/// Stores embeddings as BLOB columns and computes cosine similarity in-process
/// (same pattern as `ResponseCache`). Graceful degradation: all failures are
/// logged as WARN and never block the planning critical path.
pub struct PlanCache {
    pool: SqlitePool,
    config: PlanCacheConfig,
}

impl PlanCache {
    /// Create a new `PlanCache` and invalidate stale embeddings for the given model.
    ///
    /// # Errors
    ///
    /// Returns `PlanCacheError` if the stale embedding invalidation query fails.
    pub async fn new(
        pool: SqlitePool,
        config: PlanCacheConfig,
        current_embedding_model: &str,
    ) -> Result<Self, PlanCacheError> {
        let cache = Self { pool, config };
        cache
            .invalidate_stale_embeddings(current_embedding_model)
            .await?;
        Ok(cache)
    }

    /// NULL-ify embeddings stored under a different model to prevent cross-model false hits.
    ///
    /// # Errors
    ///
    /// Returns `PlanCacheError::Database` on query failure.
    async fn invalidate_stale_embeddings(&self, current_model: &str) -> Result<(), PlanCacheError> {
        let affected = sqlx::query(
            "UPDATE plan_cache SET embedding = NULL, embedding_model = NULL \
             WHERE embedding IS NOT NULL AND embedding_model != ?",
        )
        .bind(current_model)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if affected > 0 {
            tracing::info!(
                rows = affected,
                current_model,
                "plan cache: invalidated stale embeddings for model change"
            );
        }
        Ok(())
    }

    /// Find the most similar cached plan template for the given goal embedding.
    ///
    /// Fetches all rows with matching `embedding_model`, computes cosine similarity
    /// in-process, and returns the best match if it meets `similarity_threshold`.
    ///
    /// Also updates `last_accessed_at` on a hit.
    ///
    /// # Errors
    ///
    /// Returns `PlanCacheError::Database` on query failure or
    /// `PlanCacheError::Serialization` on template JSON deserialization failure.
    pub async fn find_similar(
        &self,
        goal_embedding: &[f32],
        embedding_model: &str,
    ) -> Result<Option<(PlanTemplate, f32)>, PlanCacheError> {
        let rows: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT id, template, embedding FROM plan_cache \
             WHERE embedding IS NOT NULL AND embedding_model = ? \
             ORDER BY last_accessed_at DESC LIMIT ?",
        )
        .bind(embedding_model)
        .bind(self.config.max_templates)
        .fetch_all(&self.pool)
        .await?;

        let mut best_score = -1.0_f32;
        let mut best_id: Option<String> = None;
        let mut best_template_json: Option<String> = None;

        for (id, template_json, blob) in rows {
            if let Some(stored) = blob_to_embedding(&blob) {
                let score = zeph_memory::cosine_similarity(goal_embedding, &stored);
                if score > best_score {
                    best_score = score;
                    best_id = Some(id);
                    best_template_json = Some(template_json);
                }
            }
        }

        if best_score >= self.config.similarity_threshold
            && let (Some(id), Some(json)) = (best_id, best_template_json)
        {
            // Update last_accessed_at on hit.
            let now = unix_now();
            if let Err(e) = sqlx::query(
                "UPDATE plan_cache SET last_accessed_at = ?, adapted_count = adapted_count + 1 \
                 WHERE id = ?",
            )
            .bind(now)
            .bind(&id)
            .execute(&self.pool)
            .await
            {
                tracing::warn!(error = %e, "plan cache: failed to update last_accessed_at");
            }
            let template: PlanTemplate = serde_json::from_str(&json)?;
            return Ok(Some((template, best_score)));
        }

        Ok(None)
    }

    /// Store a completed plan as a reusable template.
    ///
    /// Extracts a `PlanTemplate` from the `TaskGraph`, serializes it to JSON,
    /// and upserts into `SQLite` using `INSERT OR REPLACE ON CONFLICT(goal_hash)`.
    /// Deduplication is enforced by the `UNIQUE` constraint on `goal_hash`.
    ///
    /// # Errors
    ///
    /// Returns `PlanCacheError` on extraction, serialization, or database failure.
    pub async fn cache_plan(
        &self,
        graph: &TaskGraph,
        goal_embedding: &[f32],
        embedding_model: &str,
    ) -> Result<(), PlanCacheError> {
        let template = PlanTemplate::from_task_graph(graph)
            .map_err(|e| PlanCacheError::Extraction(e.to_string()))?;

        let normalized = normalize_goal(&graph.goal);
        let hash = goal_hash(&normalized);
        let template_json = serde_json::to_string(&template)?;
        let task_count = i64::try_from(template.tasks.len()).unwrap_or(i64::MAX);
        let now = unix_now();
        let id = uuid::Uuid::new_v4().to_string();
        let blob = embedding_to_blob(goal_embedding);

        sqlx::query(
            "INSERT INTO plan_cache \
             (id, goal_hash, goal_text, template, task_count, success_count, adapted_count, \
              embedding, embedding_model, created_at, last_accessed_at) \
             VALUES (?, ?, ?, ?, ?, 1, 0, ?, ?, ?, ?) \
             ON CONFLICT(goal_hash) DO UPDATE SET \
               success_count = success_count + 1, \
               template = excluded.template, \
               task_count = excluded.task_count, \
               embedding = excluded.embedding, \
               embedding_model = excluded.embedding_model, \
               last_accessed_at = excluded.last_accessed_at",
        )
        .bind(&id)
        .bind(&hash)
        .bind(&normalized)
        .bind(&template_json)
        .bind(task_count)
        .bind(&blob)
        .bind(embedding_model)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        // Evict after inserting to keep within size bounds.
        if let Err(e) = self.evict().await {
            tracing::warn!(error = %e, "plan cache: eviction failed after cache_plan");
        }

        Ok(())
    }

    /// Run TTL + size-cap LRU eviction.
    ///
    /// Phase 1: Delete rows where `last_accessed_at < now - ttl_days * 86400`.
    /// Phase 2: If count exceeds `max_templates`, delete the least-recently-accessed rows.
    ///
    /// Returns the total number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns `PlanCacheError::Database` on query failure.
    pub async fn evict(&self) -> Result<u32, PlanCacheError> {
        let now = unix_now();
        let ttl_secs = i64::from(self.config.ttl_days) * 86_400;
        let cutoff = now.saturating_sub(ttl_secs);

        let ttl_deleted = sqlx::query("DELETE FROM plan_cache WHERE last_accessed_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();

        // Count remaining rows.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_cache")
            .fetch_one(&self.pool)
            .await?;

        let max = i64::from(self.config.max_templates);
        let lru_deleted = if count > max {
            let excess = count - max;
            sqlx::query(
                "DELETE FROM plan_cache WHERE id IN \
                 (SELECT id FROM plan_cache ORDER BY last_accessed_at ASC LIMIT ?)",
            )
            .bind(excess)
            .execute(&self.pool)
            .await?
            .rows_affected()
        } else {
            0
        };

        let total = ttl_deleted + lru_deleted;
        if total > 0 {
            tracing::debug!(ttl_deleted, lru_deleted, "plan cache: eviction complete");
        }
        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }
}

/// Wrapper that checks the plan cache before calling the planner.
///
/// On a cache hit, calls `adapt_plan` with the cached template and the given
/// `LlmProvider`. Falls back to full `planner.plan()` on any failure.
///
/// # Errors
///
/// Returns `OrchestrationError` from the planner on full-decomposition fallback.
#[allow(clippy::too_many_arguments)]
pub async fn plan_with_cache<P>(
    planner: &P,
    plan_cache: Option<&PlanCache>,
    provider: &impl LlmProvider,
    embedding: Option<&[f32]>,
    embedding_model: &str,
    goal: &str,
    available_agents: &[SubAgentDef],
    max_tasks: u32,
) -> Result<(TaskGraph, Option<(u64, u64)>), OrchestrationError>
where
    P: super::planner::Planner,
{
    if let (Some(cache), Some(emb)) = (plan_cache, embedding)
        && cache.config.enabled
    {
        match cache.find_similar(emb, embedding_model).await {
            Ok(Some((template, score))) => {
                tracing::info!(
                    similarity = score,
                    tasks = template.tasks.len(),
                    "plan cache hit, adapting template"
                );
                match adapt_plan(provider, goal, &template, available_agents, max_tasks).await {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "plan cache: adaptation failed, falling back to full decomposition"
                        );
                    }
                }
            }
            Ok(None) => {
                tracing::debug!("plan cache miss");
            }
            Err(e) => {
                tracing::warn!(error = %e, "plan cache: find_similar failed, using full decomposition");
            }
        }
    }

    planner.plan(goal, available_agents).await
}

/// Build an adaptation prompt and call the LLM to produce a `TaskGraph` adapted
/// from a cached template for the new goal.
///
/// Uses `LlmProvider::chat_typed` with the same `PlannerResponse` schema as the
/// full planner, so the existing `convert_response + dag::validate` pipeline applies.
///
/// # Errors
///
/// Returns `OrchestrationError::PlanningFailed` if the LLM call fails or the
/// adapted graph fails DAG validation.
async fn adapt_plan(
    provider: &impl LlmProvider,
    goal: &str,
    template: &PlanTemplate,
    available_agents: &[SubAgentDef],
    max_tasks: u32,
) -> Result<(TaskGraph, Option<(u64, u64)>), OrchestrationError> {
    use zeph_subagent::ToolPolicy;

    let agent_catalog = available_agents
        .iter()
        .map(|a| {
            let tools = match &a.tools {
                ToolPolicy::AllowList(list) => list.join(", "),
                ToolPolicy::DenyList(excluded) => {
                    format!("all except: [{}]", excluded.join(", "))
                }
                ToolPolicy::InheritAll => "all".to_string(),
            };
            format!(
                "- name: \"{}\", description: \"{}\", tools: [{}]",
                a.name, a.description, tools
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let template_json = serde_json::to_string(&template.tasks)
        .map_err(|e| OrchestrationError::PlanningFailed(e.to_string()))?;

    let system = format!(
        "You are a task planner. A cached plan template exists for a similar goal. \
         Adapt it for the new goal by adjusting task descriptions and adding or removing \
         tasks as needed. Keep the same JSON structure.\n\n\
         Available agents:\n{agent_catalog}\n\n\
         Rules:\n\
         - Each task must have a unique task_id (short, descriptive, kebab-case: [a-z0-9-]).\n\
         - Specify dependencies using task_id strings in depends_on.\n\
         - Do not create more than {max_tasks} tasks.\n\
         - failure_strategy is optional: \"abort\", \"retry\", \"skip\", \"ask\"."
    );

    let user = format!(
        "New goal:\n{goal}\n\nCached template (for similar goal \"{}\"):\n{template_json}\n\n\
         Adapt the template for the new goal. Return JSON: {{\"tasks\": [...]}}",
        template.goal
    );

    let messages = vec![
        Message::from_legacy(Role::System, system),
        Message::from_legacy(Role::User, user),
    ];

    let response: PlannerResponse = provider
        .chat_typed(&messages)
        .await
        .map_err(|e| OrchestrationError::PlanningFailed(e.to_string()))?;

    let usage = provider.last_usage();

    let graph = convert_response_pub(response, goal, available_agents, max_tasks)?;

    dag::validate(&graph.tasks, max_tasks as usize)?;

    Ok((graph, usage))
}

#[cfg(test)]
mod tests {
    use super::super::graph::{TaskId, TaskNode};
    use super::*;
    use zeph_memory::sqlite::SqliteStore;

    async fn test_pool() -> SqlitePool {
        let store = SqliteStore::new(":memory:").await.unwrap();
        store.pool().clone()
    }

    async fn test_cache(pool: SqlitePool) -> PlanCache {
        PlanCache::new(pool, PlanCacheConfig::default(), "test-model")
            .await
            .unwrap()
    }

    fn make_graph(goal: &str, tasks: &[(&str, &str, &[u32])]) -> TaskGraph {
        let mut graph = TaskGraph::new(goal);
        for (i, (title, desc, deps)) in tasks.iter().enumerate() {
            let mut node = TaskNode::new(i as u32, *title, *desc);
            node.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
            graph.tasks.push(node);
        }
        graph
    }

    // --- normalize_goal tests ---

    #[test]
    fn normalize_trims_and_lowercases() {
        assert_eq!(normalize_goal("  Hello World  "), "hello world");
    }

    #[test]
    fn normalize_collapses_internal_whitespace() {
        assert_eq!(normalize_goal("hello   world"), "hello world");
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(normalize_goal(""), "");
    }

    #[test]
    fn normalize_whitespace_only() {
        assert_eq!(normalize_goal("   "), "");
    }

    // --- goal_hash tests ---

    #[test]
    fn goal_hash_is_deterministic() {
        let h1 = goal_hash("deploy service");
        let h2 = goal_hash("deploy service");
        assert_eq!(h1, h2);
    }

    #[test]
    fn goal_hash_differs_for_different_goals() {
        assert_ne!(goal_hash("deploy service"), goal_hash("build artifact"));
    }

    #[test]
    fn goal_hash_nonempty() {
        assert!(!goal_hash("goal").is_empty());
    }

    // --- PlanTemplate extraction tests ---

    #[test]
    fn template_from_empty_graph_returns_error() {
        let graph = TaskGraph::new("goal");
        assert!(PlanTemplate::from_task_graph(&graph).is_err());
    }

    #[test]
    fn template_strips_runtime_fields() {
        use crate::graph::TaskStatus;
        let mut graph = make_graph("goal", &[("Fetch data", "Download it", &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[0].retry_count = 3;
        graph.tasks[0].assigned_agent = Some("agent-x".to_string());
        let template = PlanTemplate::from_task_graph(&graph).unwrap();
        // Template only has structural data — no TaskStatus, retry_count, etc.
        assert_eq!(template.tasks[0].title, "Fetch data");
        assert_eq!(template.tasks[0].description, "Download it");
    }

    #[test]
    fn template_preserves_dependencies() {
        let graph = make_graph("goal", &[("Task A", "do A", &[]), ("Task B", "do B", &[0])]);
        let template = PlanTemplate::from_task_graph(&graph).unwrap();
        assert_eq!(template.tasks.len(), 2);
        assert!(template.tasks[0].depends_on.is_empty());
        assert_eq!(template.tasks[1].depends_on.len(), 1);
        assert_eq!(template.tasks[1].depends_on[0], template.tasks[0].task_id);
    }

    #[test]
    fn template_serde_roundtrip() {
        let graph = make_graph("goal", &[("Step one", "do step one", &[])]);
        let template = PlanTemplate::from_task_graph(&graph).unwrap();
        let json = serde_json::to_string(&template).unwrap();
        let restored: PlanTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(template.tasks[0].title, restored.tasks[0].title);
        assert_eq!(template.goal, restored.goal);
    }

    // --- BLOB serialization tests ---

    #[test]
    fn embedding_blob_roundtrip() {
        let embedding = vec![1.0_f32, 0.5, 0.25, -1.0];
        let blob = embedding_to_blob(&embedding);
        let restored = blob_to_embedding(&blob).unwrap();
        assert_eq!(embedding, restored);
    }

    #[test]
    fn blob_to_embedding_odd_length_returns_none() {
        let bad_blob = vec![0u8; 5]; // not a multiple of 4
        assert!(blob_to_embedding(&bad_blob).is_none());
    }

    // --- PlanCache integration tests ---

    #[tokio::test]
    async fn cache_miss_on_empty_cache() {
        let pool = test_pool().await;
        let cache = test_cache(pool).await;
        let result = cache
            .find_similar(&[1.0, 0.0, 0.0], "test-model")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cache_store_and_hit() {
        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        config.similarity_threshold = 0.9;
        let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

        let graph = make_graph("deploy service", &[("Build", "build it", &[])]);
        let embedding = vec![1.0_f32, 0.0, 0.0];
        cache
            .cache_plan(&graph, &embedding, "test-model")
            .await
            .unwrap();

        // Same embedding should hit.
        let result = cache
            .find_similar(&[1.0, 0.0, 0.0], "test-model")
            .await
            .unwrap();
        assert!(result.is_some());
        let (template, score) = result.unwrap();
        assert!((score - 1.0).abs() < 1e-5);
        assert_eq!(template.tasks.len(), 1);
    }

    #[tokio::test]
    async fn cache_miss_on_dissimilar_goal() {
        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        config.similarity_threshold = 0.9;
        let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

        let graph = make_graph("goal a", &[("Task", "do it", &[])]);
        cache
            .cache_plan(&graph, &[1.0_f32, 0.0, 0.0], "test-model")
            .await
            .unwrap();

        // Orthogonal vector — should not hit at threshold 0.9.
        let result = cache
            .find_similar(&[0.0, 1.0, 0.0], "test-model")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn deduplication_increments_success_count() {
        let pool = test_pool().await;
        let cache = test_cache(pool.clone()).await;

        let graph = make_graph("same goal", &[("Task", "do it", &[])]);
        let emb = vec![1.0_f32, 0.0];

        cache.cache_plan(&graph, &emb, "test-model").await.unwrap();
        cache.cache_plan(&graph, &emb, "test-model").await.unwrap();

        // Only one row due to UNIQUE goal_hash.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let success: i64 = sqlx::query_scalar("SELECT success_count FROM plan_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(success, 2);
    }

    #[tokio::test]
    async fn eviction_removes_ttl_expired_rows() {
        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        // TTL of 0 days means everything is immediately expired.
        config.ttl_days = 0;
        let cache = PlanCache::new(pool.clone(), config, "test-model")
            .await
            .unwrap();

        // Insert a row by bypassing the API to set last_accessed_at in the past.
        let now = unix_now() - 1;
        sqlx::query(
            "INSERT INTO plan_cache \
             (id, goal_hash, goal_text, template, task_count, created_at, last_accessed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("test-id")
        .bind("hash-1")
        .bind("goal")
        .bind("{\"goal\":\"goal\",\"tasks\":[]}")
        .bind(0_i64)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let deleted = cache.evict().await.unwrap();
        assert!(deleted >= 1);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn eviction_lru_when_over_max() {
        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        config.max_templates = 2;
        config.ttl_days = 365;
        let cache = PlanCache::new(pool.clone(), config, "test-model")
            .await
            .unwrap();

        let now = unix_now();
        // Insert 3 rows with different last_accessed_at, all recent enough to survive TTL.
        for i in 0..3_i64 {
            sqlx::query(
                "INSERT INTO plan_cache \
                 (id, goal_hash, goal_text, template, task_count, created_at, last_accessed_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("id-{i}"))
            .bind(format!("hash-{i}"))
            .bind(format!("goal-{i}"))
            .bind("{\"goal\":\"g\",\"tasks\":[]}")
            .bind(0_i64)
            .bind(now)
            .bind(now + i) // i=0 is least recently accessed, i=2 most recent
            .execute(&pool)
            .await
            .unwrap();
        }

        let deleted = cache.evict().await.unwrap();
        assert_eq!(deleted, 1);

        // The row with smallest last_accessed_at (id-0) should be gone.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_cache")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn stale_embedding_invalidated_on_new() {
        let pool = test_pool().await;
        let now = unix_now();

        // Insert a row with "old-model" embedding.
        let emb = embedding_to_blob(&[1.0_f32, 0.0]);
        sqlx::query(
            "INSERT INTO plan_cache \
             (id, goal_hash, goal_text, template, task_count, embedding, embedding_model, \
              created_at, last_accessed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("id-old")
        .bind("hash-old")
        .bind("goal old")
        .bind("{\"goal\":\"g\",\"tasks\":[]}")
        .bind(0_i64)
        .bind(&emb)
        .bind("old-model")
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        // Constructing cache with "new-model" should invalidate the old embedding.
        let _cache = PlanCache::new(pool.clone(), PlanCacheConfig::default(), "new-model")
            .await
            .unwrap();

        let model: Option<String> =
            sqlx::query_scalar("SELECT embedding_model FROM plan_cache WHERE id = 'id-old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(model.is_none(), "stale embedding_model should be NULL");

        let emb_col: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT embedding FROM plan_cache WHERE id = 'id-old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(emb_col.is_none(), "stale embedding should be NULL");
    }

    #[tokio::test]
    async fn disabled_cache_not_used_in_plan_with_cache() {
        use zeph_llm::mock::MockProvider;

        let pool = test_pool().await;
        let config = PlanCacheConfig::default(); // enabled = false
        let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

        let graph_json = r#"{"tasks": [
            {"task_id": "t1", "title": "Task", "description": "do it", "depends_on": []}
        ]}"#
        .to_string();

        let provider = MockProvider::with_responses(vec![graph_json.clone()]);
        use crate::planner::LlmPlanner;
        use zeph_config::OrchestrationConfig;
        let planner = LlmPlanner::new(
            MockProvider::with_responses(vec![graph_json]),
            &OrchestrationConfig::default(),
        );

        let (graph, _) = plan_with_cache(
            &planner,
            Some(&cache),
            &provider,
            Some(&[1.0_f32, 0.0]),
            "test-model",
            "do something",
            &[],
            20,
        )
        .await
        .unwrap();

        assert_eq!(graph.tasks.len(), 1);
    }

    #[tokio::test]
    async fn plan_with_cache_with_none_embedding_skips_cache() {
        use crate::planner::LlmPlanner;
        use zeph_config::OrchestrationConfig;
        use zeph_llm::mock::MockProvider;

        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        config.enabled = true;
        config.similarity_threshold = 0.5;
        let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

        // Pre-populate cache with a similar goal.
        let graph = make_graph("deploy service", &[("Build", "build it", &[])]);
        cache
            .cache_plan(&graph, &[1.0_f32, 0.0], "test-model")
            .await
            .unwrap();

        let graph_json = r#"{"tasks": [
            {"task_id": "fallback-task-0", "title": "Fallback", "description": "planner fallback", "depends_on": []}
        ]}"#
        .to_string();

        let provider = MockProvider::with_responses(vec![graph_json.clone()]);
        let planner = LlmPlanner::new(
            MockProvider::with_responses(vec![graph_json]),
            &OrchestrationConfig::default(),
        );

        // embedding = None → must skip cache and call planner.
        let (result_graph, _) = plan_with_cache(
            &planner,
            Some(&cache),
            &provider,
            None, // no embedding provided
            "test-model",
            "deploy service",
            &[],
            20,
        )
        .await
        .unwrap();

        assert_eq!(result_graph.tasks[0].title, "Fallback");
    }

    #[tokio::test]
    async fn adapt_plan_error_fallback_to_full_decomposition() {
        use crate::planner::LlmPlanner;
        use zeph_config::OrchestrationConfig;
        use zeph_llm::mock::MockProvider;

        let pool = test_pool().await;
        let mut config = PlanCacheConfig::default();
        config.enabled = true;
        config.similarity_threshold = 0.5;
        let cache = PlanCache::new(pool, config, "test-model").await.unwrap();

        // Pre-populate cache with matching embedding.
        let graph = make_graph("deploy service", &[("Build", "build it", &[])]);
        cache
            .cache_plan(&graph, &[1.0_f32, 0.0], "test-model")
            .await
            .unwrap();

        // Provider for adapt_plan returns invalid JSON — adaptation fails.
        let bad_provider = MockProvider::with_responses(vec!["not valid json".to_string()]);

        // Planner (fallback path) returns a valid response.
        let fallback_json = r#"{"tasks": [
            {"task_id": "fallback-0", "title": "Fallback Task", "description": "via planner", "depends_on": []}
        ]}"#
        .to_string();
        let planner = LlmPlanner::new(
            MockProvider::with_responses(vec![fallback_json]),
            &OrchestrationConfig::default(),
        );

        let (result_graph, _) = plan_with_cache(
            &planner,
            Some(&cache),
            &bad_provider, // adapt_plan will fail with this provider
            Some(&[1.0_f32, 0.0]),
            "test-model",
            "deploy service",
            &[],
            20,
        )
        .await
        .unwrap();

        // Must return planner fallback result, not error.
        assert_eq!(result_graph.tasks[0].title, "Fallback Task");
    }
}
