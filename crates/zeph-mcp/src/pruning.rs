// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic MCP tool pruning for context optimization (#2204).
//!
//! The `prune_tools` free function filters a list of MCP tools to only those relevant
//! to the current task, using an LLM call with a fast/cheap model. This reduces context
//! usage and improves tool selection accuracy when MCP servers expose many tools.
//!
//! `zeph-mcp` does not depend on `zeph-config` (circular dependency: zeph-config ->
//! zeph-mcp). Callers in `zeph-core` convert `ToolPruningConfig` into `PruningParams`
//! before calling `prune_tools`.

use std::fmt::Write as _;

use zeph_llm::LlmError;
use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::tool::McpTool;

// ── Per-message pruning cache (#2298) ────────────────────────────────────────

/// Cached outcome stored by [`PruningCache`].
///
/// [`Ok`] holds the previously-computed pruned tool list; [`Failed`] is a
/// sentinel written when the LLM call failed, so subsequent lookups with the
/// same key return the all-tools fallback without retrying the LLM.
#[derive(Debug, Clone)]
enum CachedResult {
    Ok(Vec<McpTool>),
    /// LLM call failed; caller should use the full tool list.
    Failed,
}

/// Per-message cache for MCP tool pruning results.
///
/// Stores at most one entry keyed on `(message_content_hash, tool_list_hash)`.
/// A cache miss triggers an LLM call; a hit returns the stored result
/// immediately.  Negative entries (`Failed`) prevent retry storms when the
/// pruning LLM is transiently unavailable.
///
/// # Cache contract
///
/// `PruningCache` returns previously-computed pruning results keyed on
/// `(message_content_hash, tool_list_hash)`.
///
/// `tool_list_hash` includes: `server_id`, `name`, `description`, and
/// `input_schema` for every tool.  Any change to tool metadata (not just the
/// name set) produces a different hash and causes a cache miss.
///
/// `PruningCache::reset()` is additionally called on:
/// - New user message (top of `process_user_message_inner`)
/// - `tools/list_changed` notification (in `check_tool_refresh`)
/// - Manual `/mcp add` or `/mcp remove` commands
///
/// `PruningParams` is **not** part of the cache key.  Callers must not change
/// `PruningParams` within a single user turn; this invariant holds because
/// params are derived from `ToolPruningConfig`, which is stable within a turn
/// (config changes trigger a full agent rebuild, not a mid-turn param swap).
///
/// Designed for single-owner use (`&mut` on `Agent`). Not thread-safe.
#[derive(Debug, Default, Clone)]
pub struct PruningCache {
    key: Option<(u64, u64)>,
    result: Option<CachedResult>,
}

/// Outcome of a [`PruningCache::lookup`] call.
enum CacheLookup<'a> {
    /// Positive hit: pruned tool slice from a previous successful call.
    Hit(&'a [McpTool]),
    /// Negative hit: LLM previously failed; caller should use the full tool list.
    NegativeHit,
    /// No entry for this key.
    Miss,
}

impl PruningCache {
    /// Create a new, empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the cached entry.
    ///
    /// Must be called at the start of each user turn and whenever the MCP tool
    /// list changes (via notification, `/mcp add`, or `/mcp remove`).
    pub fn reset(&mut self) {
        self.key = None;
        self.result = None;
    }

    fn lookup(&self, msg_hash: u64, tool_hash: u64) -> CacheLookup<'_> {
        match (&self.key, &self.result) {
            (Some(k), Some(CachedResult::Ok(tools))) if *k == (msg_hash, tool_hash) => {
                CacheLookup::Hit(tools)
            }
            (Some(k), Some(CachedResult::Failed)) if *k == (msg_hash, tool_hash) => {
                CacheLookup::NegativeHit
            }
            _ => CacheLookup::Miss,
        }
    }

    fn insert_ok(&mut self, msg_hash: u64, tool_hash: u64, tools: Vec<McpTool>) {
        self.key = Some((msg_hash, tool_hash));
        self.result = Some(CachedResult::Ok(tools));
    }

    fn insert_failed(&mut self, msg_hash: u64, tool_hash: u64) {
        self.key = Some((msg_hash, tool_hash));
        self.result = Some(CachedResult::Failed);
    }
}

/// Compute a `u64` hash of a string using blake3 (first 8 bytes, little-endian).
///
/// # Panics
///
/// Never panics in practice: blake3 always produces at least 8 bytes of output.
#[must_use]
pub fn content_hash(s: &str) -> u64 {
    let hash = blake3::hash(s.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("blake3 >= 8 bytes"))
}

/// Compute a `u64` hash of the full tool list metadata using blake3.
///
/// Hashes `server_id`, `name`, `description`, and `input_schema` for every
/// tool, sorted by qualified name (`server_id` then `name`) for deterministic
/// ordering regardless of list order.
///
/// **`BTreeMap` assumption**: `serde_json::to_vec` produces deterministic output
/// because `serde_json::Map` defaults to `BTreeMap`-backed storage (sorted
/// keys).  If the `preserve_order` feature of `serde_json` is ever enabled
/// (switching `Map` to `IndexMap`), key order becomes insertion-order and
/// hashing becomes non-deterministic.  Should `preserve_order` be needed,
/// sort `Map` keys before serialising here.
///
/// # Panics
///
/// Never panics in practice: blake3 always produces at least 8 bytes of output.
#[must_use]
pub fn tool_list_hash(tools: &[McpTool]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    let mut sorted: Vec<&McpTool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.server_id.cmp(&b.server_id).then(a.name.cmp(&b.name)));
    for tool in sorted {
        hasher.update(tool.server_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(tool.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(tool.description.as_bytes());
        hasher.update(b"\0");
        match serde_json::to_vec(&tool.input_schema) {
            Ok(schema_bytes) => {
                hasher.update(&schema_bytes);
            }
            Err(_) => {
                hasher.update(b"\x00");
            }
        }
        // Tool separator — prevents adjacent-field collisions.
        hasher.update(b"\x01");
    }
    let hash = hasher.finalize();
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("blake3 >= 8 bytes"))
}

/// Cache-aware wrapper around [`prune_tools`].
///
/// On a **positive cache hit**: returns the previously-computed pruned list
/// without an LLM call.
///
/// On a **negative cache hit** (LLM previously failed for this key): returns
/// `Ok(all_tools.to_vec())` without retrying the LLM, avoiding retry storms
/// when the pruning LLM is transiently unavailable.
///
/// On a **cache miss**: calls [`prune_tools`], stores the result (success or
/// failure), and returns.  On LLM failure the negative sentinel is cached and
/// `Err(PruningError)` is returned so the caller can log and fall back.
///
/// # Errors
///
/// Propagates `PruningError` from [`prune_tools`] on the first (uncached) LLM
/// failure.  Subsequent calls with the same key return `Ok(all_tools.to_vec())`
/// from the negative cache entry.
pub async fn prune_tools_cached<P: LlmProvider>(
    cache: &mut PruningCache,
    all_tools: &[McpTool],
    task_context: &str,
    params: &PruningParams,
    provider: &P,
) -> Result<Vec<McpTool>, PruningError> {
    let msg_hash = content_hash(task_context);
    let tl_hash = tool_list_hash(all_tools);

    match cache.lookup(msg_hash, tl_hash) {
        CacheLookup::Hit(cached) => return Ok(cached.to_vec()),
        CacheLookup::NegativeHit => {
            // Negative cache hit: LLM previously failed for this key.
            // Return all tools as fallback without retrying to avoid retry storms.
            tracing::warn!("pruning cache: negative hit, returning all tools without LLM call");
            return Ok(all_tools.to_vec());
        }
        CacheLookup::Miss => {}
    }

    match prune_tools(all_tools, task_context, params, provider).await {
        Ok(result) => {
            cache.insert_ok(msg_hash, tl_hash, result.clone());
            Ok(result)
        }
        Err(e) => {
            cache.insert_failed(msg_hash, tl_hash);
            Err(e)
        }
    }
}

/// Errors that can occur during tool pruning.
#[derive(Debug, thiserror::Error)]
pub enum PruningError {
    /// LLM call failed.
    #[error("pruning LLM call failed: {0}")]
    LlmError(#[from] LlmError),
    /// Could not extract a valid JSON array from the LLM response.
    #[error("failed to parse pruning response as JSON array of tool names")]
    ParseError,
}

/// Parameters for the `prune_tools` function.
///
/// Mirrors `zeph_config::ToolPruningConfig` but lives in `zeph-mcp` to avoid a
/// circular crate dependency (`zeph-config` → `zeph-mcp`). Callers in `zeph-core`
/// convert from `ToolPruningConfig`.
#[derive(Debug, Clone)]
pub struct PruningParams {
    /// Maximum number of MCP tools to include after pruning.
    pub max_tools: usize,
    /// Minimum number of MCP tools below which pruning is skipped.
    pub min_tools_to_prune: usize,
    /// Tool names that are never pruned (always included).
    ///
    /// Matches on bare tool `name` (not qualified `server_id:name`).  When two
    /// MCP servers expose a tool with the same name, both instances are pinned.
    /// This is intentional: the config is user-facing and users specify tool
    /// names, not server-qualified identifiers.
    pub always_include: Vec<String>,
}

impl Default for PruningParams {
    fn default() -> Self {
        Self {
            max_tools: 15,
            min_tools_to_prune: 10,
            always_include: Vec::new(),
        }
    }
}

/// Prune MCP tools to those relevant to the current task.
///
/// Returns a filtered subset of `all_tools` based on the LLM's assessment of relevance
/// to `task_context`. Tools listed in `params.always_include` bypass the LLM filter.
///
/// # Behavior
///
/// - If `all_tools.len() < params.min_tools_to_prune`, returns `Ok(all_tools.to_vec())`.
/// - On LLM failure or parse failure, returns `Err(PruningError)` — the caller should
///   fall back to the full tool list and log at `WARN` level.
/// - Result is capped at `params.max_tools` total tools. `max_tools == 0` means no cap.
///
/// # Errors
///
/// Returns `PruningError::LlmError` if the provider call fails.
/// Returns `PruningError::ParseError` if the response cannot be parsed as a JSON array.
pub async fn prune_tools<P: LlmProvider>(
    all_tools: &[McpTool],
    task_context: &str,
    params: &PruningParams,
    provider: &P,
) -> Result<Vec<McpTool>, PruningError> {
    if all_tools.len() < params.min_tools_to_prune {
        return Ok(all_tools.to_vec());
    }

    // Partition: always-include tools bypass the LLM filter.
    let (pinned, candidates): (Vec<_>, Vec<_>) = all_tools
        .iter()
        .partition(|t| params.always_include.iter().any(|a| a == &t.name));

    // Build the pruning prompt.
    // Sanitize tool names and descriptions before interpolation to prevent prompt injection
    // from attacker-controlled MCP servers.
    let tool_list = candidates.iter().fold(String::new(), |mut acc, t| {
        let name = sanitize_tool_name(&t.name);
        let desc = sanitize_tool_description(&t.description);
        let _ = writeln!(acc, "- {name}: {desc}");
        acc
    });

    let prompt = format!(
        "Return a JSON array of tool names that are relevant to the task below.\n\
         Return ONLY the JSON array, no explanation, no markdown.\n\n\
         Task: {task_context}\n\n\
         Available tools:\n{tool_list}"
    );

    let messages = vec![Message::from_legacy(Role::User, prompt)];
    let response = provider.chat(&messages).await?;

    // Parse: strip markdown fences, find first `[` to last `]`.
    let relevant_names = parse_name_array(&response)?;

    // always_include tools are added unconditionally and bypass the max_tools cap;
    // max_tools applies only to LLM-selected candidates.
    let mut result: Vec<McpTool> = pinned.into_iter().cloned().collect();
    let mut candidates_added: usize = 0;
    for tool in &candidates {
        // max_tools == 0 means no cap on LLM-selected candidates.
        if params.max_tools > 0 && candidates_added >= params.max_tools {
            break;
        }
        if relevant_names.iter().any(|n| n == &tool.name) {
            result.push((*tool).clone());
            candidates_added += 1;
        }
    }

    Ok(result)
}

/// Sanitize a tool name before interpolating into an LLM prompt.
///
/// Strips control characters and caps at 64 characters.
fn sanitize_tool_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Sanitize a tool description before interpolating into an LLM prompt.
///
/// Strips control characters and caps at 200 characters.
fn sanitize_tool_description(desc: &str) -> String {
    desc.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Extract tool names from an LLM response expected to contain a JSON array of strings.
///
/// Handles markdown code fences (` ```json ... ``` `) and leading/trailing whitespace.
fn parse_name_array(response: &str) -> Result<Vec<String>, PruningError> {
    // Strip markdown code fence lines.
    let stripped = response
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");

    // Find the first `[` and last `]` to isolate the JSON array.
    let start = stripped.find('[').ok_or(PruningError::ParseError)?;
    let end = stripped.rfind(']').ok_or(PruningError::ParseError)?;
    if end <= start {
        return Err(PruningError::ParseError);
    }

    let json_fragment = &stripped[start..=end];
    let names: Vec<String> =
        serde_json::from_str(json_fragment).map_err(|_| PruningError::ParseError)?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use zeph_llm::mock::MockProvider;

    use super::*;

    fn make_tool(name: &str, description: &str) -> McpTool {
        McpTool {
            server_id: "test".into(),
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::Value::Null,
            security_meta: Default::default(),
        }
    }

    fn make_tool_with_server(server_id: &str, name: &str, description: &str) -> McpTool {
        McpTool {
            server_id: server_id.into(),
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::Value::Null,
            security_meta: Default::default(),
        }
    }

    /// Build params with low `min_tools_to_prune` so tests aren't skipped early.
    fn params_with_max(max_tools: usize) -> PruningParams {
        PruningParams {
            max_tools,
            min_tools_to_prune: 1,
            always_include: Vec::new(),
        }
    }

    #[test]
    fn parse_plain_array() {
        let names = parse_name_array(r#"["bash", "read", "write"]"#).unwrap();
        assert_eq!(names, vec!["bash", "read", "write"]);
    }

    #[test]
    fn parse_array_with_markdown_fences() {
        let input = "```json\n[\"bash\", \"read\"]\n```";
        let names = parse_name_array(input).unwrap();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn parse_array_with_preamble() {
        let input = "Here are the relevant tools:\n[\"bash\", \"read\"]";
        let names = parse_name_array(input).unwrap();
        assert_eq!(names, vec!["bash", "read"]);
    }

    #[test]
    fn parse_empty_array() {
        let names = parse_name_array("[]").unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn parse_invalid_returns_error() {
        assert!(parse_name_array("not json").is_err());
        assert!(parse_name_array("").is_err());
        assert!(parse_name_array("{\"key\": \"val\"}").is_err());
    }

    // Replaced below_min_detected tautology (#2300): call prune_tools with a failing
    // mock to verify the early-return path fires before the LLM is ever contacted.
    #[tokio::test]
    async fn below_min_detected_early_return() {
        let tools: Vec<McpTool> = (0..5).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        // MockProvider::failing() would panic on any LLM call — if prune_tools invokes it,
        // the test will error rather than pass.
        let provider = MockProvider::failing();
        let params = PruningParams {
            max_tools: 0,
            min_tools_to_prune: 10, // 5 tools < 10 → early return before LLM
            always_include: Vec::new(),
        };

        let result = prune_tools(&tools, "task", &params, &provider)
            .await
            .unwrap();
        assert_eq!(result.len(), 5, "all tools returned when below threshold");
    }

    #[tokio::test]
    async fn always_include_pinned() {
        let tools = vec![
            make_tool("pinned", "always here"),
            make_tool("candidate_a", "desc a"),
            make_tool("candidate_b", "desc b"),
        ];
        // LLM returns only candidate_a; pinned must still appear.
        let provider = MockProvider::with_responses(vec![r#"["candidate_a"]"#.into()]);
        let params = PruningParams {
            max_tools: 0,
            min_tools_to_prune: 1,
            always_include: vec!["pinned".into()],
        };

        let result = prune_tools(&tools, "task", &params, &provider)
            .await
            .unwrap();
        assert!(
            result.iter().any(|t| t.name == "pinned"),
            "pinned must survive pruning"
        );
        assert!(result.iter().any(|t| t.name == "candidate_a"));
    }

    /// S4: `always_include` pins tools by bare name across multiple servers.
    #[tokio::test]
    async fn always_include_matches_bare_name_across_servers() {
        let tools = vec![
            make_tool_with_server("server_a", "search", "search on A"),
            make_tool_with_server("server_b", "search", "search on B"),
            make_tool_with_server("server_a", "other", "other tool"),
        ];
        // LLM returns only "other"; both "search" instances should still be pinned.
        let provider = MockProvider::with_responses(vec![r#"["other"]"#.into()]);
        let params = PruningParams {
            max_tools: 0,
            min_tools_to_prune: 1,
            always_include: vec!["search".into()],
        };

        let result = prune_tools(&tools, "task", &params, &provider)
            .await
            .unwrap();
        assert_eq!(result.len(), 3, "both search tools + other must be present");
        let search_count = result.iter().filter(|t| t.name == "search").count();
        assert_eq!(
            search_count, 2,
            "both server_a:search and server_b:search must be pinned"
        );
        assert!(result.iter().any(|t| t.name == "other"));
    }

    #[tokio::test]
    async fn max_tools_cap_respected() {
        let tools: Vec<McpTool> = (0..5).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        // LLM returns all 5 as relevant; max_tools=2 must cap candidates.
        let names_json = r#"["t0","t1","t2","t3","t4"]"#;
        let provider = MockProvider::with_responses(vec![names_json.into()]);

        let result = prune_tools(&tools, "task", &params_with_max(2), &provider)
            .await
            .unwrap();
        assert_eq!(
            result.len(),
            2,
            "max_tools=2 must cap LLM-selected candidates"
        );
    }

    #[tokio::test]
    async fn llm_failure_propagates() {
        let tools: Vec<McpTool> = (0..3).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let provider = MockProvider::failing();
        let result = prune_tools(&tools, "task", &params_with_max(0), &provider).await;
        assert!(matches!(result, Err(PruningError::LlmError(_))));
    }

    #[tokio::test]
    async fn parse_error_propagates() {
        let tools: Vec<McpTool> = (0..3).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let provider = MockProvider::with_responses(vec!["not valid json at all".into()]);
        let result = prune_tools(&tools, "task", &params_with_max(0), &provider).await;
        assert!(matches!(result, Err(PruningError::ParseError)));
    }

    #[tokio::test]
    async fn max_tools_zero_means_no_cap() {
        let tools: Vec<McpTool> = (0..5)
            .map(|i| make_tool(&format!("tool{i}"), "desc"))
            .collect();
        let names_json = r#"["tool0","tool1","tool2","tool3","tool4"]"#;
        let provider = MockProvider::with_responses(vec![names_json.into()]);
        let params = params_with_max(0);

        let result = prune_tools(&tools, "any task", &params, &provider)
            .await
            .unwrap();
        assert_eq!(result.len(), 5, "max_tools=0 must not cap the result");
    }

    #[test]
    fn description_sanitization_strips_control_chars_and_caps() {
        // Newline and tab are control characters.
        let desc = "line1\nline2\tinject";
        let sanitized = sanitize_tool_description(desc);
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\t'));

        // Cap at 200 characters.
        let long_desc = "x".repeat(300);
        assert_eq!(sanitize_tool_description(&long_desc).len(), 200);

        // Name capped at 64 characters.
        let long_name = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long_name).len(), 64);
    }

    #[tokio::test]
    async fn always_include_bypasses_max_tools_cap() {
        // max_tools=1 — only 1 candidate from LLM allowed; but always_include adds unconditionally.
        let tools = vec![
            make_tool("pinned", "always here"),
            make_tool("candidate_a", "desc a"),
            make_tool("candidate_b", "desc b"),
        ];
        let provider =
            MockProvider::with_responses(vec![r#"["candidate_a","candidate_b"]"#.into()]);
        let params = PruningParams {
            max_tools: 1,
            min_tools_to_prune: 1,
            always_include: vec!["pinned".into()],
        };

        let result = prune_tools(&tools, "task", &params, &provider)
            .await
            .unwrap();

        // "pinned" is always present regardless of max_tools.
        assert!(
            result.iter().any(|t| t.name == "pinned"),
            "pinned tool must bypass cap"
        );
        // Only 1 candidate slot remains after pinned bypasses cap; total = 1 (pinned) + 1 (candidate).
        assert_eq!(result.len(), 2);
    }

    // ── PruningCache tests (#2298, #2300) ────────────────────────────────────

    #[tokio::test]
    async fn cache_positive_hit() {
        // Two tools to exceed min_tools_to_prune=1; MockProvider has exactly one response.
        // The second call must succeed from cache without consuming the (empty) response queue.
        let tools: Vec<McpTool> = (0..2).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let provider = MockProvider::with_responses(vec![r#"["t0","t1"]"#.into()]);
        let params = params_with_max(0);
        let mut cache = PruningCache::new();

        let r1 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider)
            .await
            .unwrap();
        let r2 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider)
            .await
            .unwrap();

        assert_eq!(r1.len(), 2);
        assert_eq!(r1.len(), r2.len(), "cache hit must return same result");
    }

    #[tokio::test]
    async fn cache_miss_on_message_change() {
        let tools: Vec<McpTool> = (0..2).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let provider =
            MockProvider::with_responses(vec![r#"["t0","t1"]"#.into(), r#"["t0"]"#.into()]);
        let params = params_with_max(0);
        let mut cache = PruningCache::new();

        let r1 = prune_tools_cached(&mut cache, &tools, "query_a", &params, &provider)
            .await
            .unwrap();
        let r2 = prune_tools_cached(&mut cache, &tools, "query_b", &params, &provider)
            .await
            .unwrap();

        assert_eq!(r1.len(), 2, "first call returns both tools");
        assert_eq!(
            r2.len(),
            1,
            "different message triggers cache miss and LLM call"
        );
    }

    #[tokio::test]
    async fn cache_miss_on_tool_list_change() {
        let tools1: Vec<McpTool> = (0..2).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let mut tools2 = tools1.clone();
        tools2.push(make_tool("t2", "new tool"));

        let provider = MockProvider::with_responses(vec![
            r#"["t0","t1"]"#.into(),
            r#"["t0","t1","t2"]"#.into(),
        ]);
        let params = params_with_max(0);
        let mut cache = PruningCache::new();

        let r1 = prune_tools_cached(&mut cache, &tools1, "query", &params, &provider)
            .await
            .unwrap();
        let r2 = prune_tools_cached(&mut cache, &tools2, "query", &params, &provider)
            .await
            .unwrap();

        assert_eq!(r1.len(), 2);
        assert_eq!(r2.len(), 3, "new tool triggers cache miss");
    }

    #[tokio::test]
    async fn cache_negative_hit_skips_llm() {
        let tools: Vec<McpTool> = (0..2).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let provider = MockProvider::failing();
        let params = params_with_max(0);
        let mut cache = PruningCache::new();

        // First call: LLM fails → error is returned and negative entry is cached.
        let r1 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider).await;
        assert!(r1.is_err(), "first call must propagate LLM error");

        // Second call: negative cache hit → returns all tools without calling LLM.
        // MockProvider::failing() would panic on a second LLM call, proving cache is used.
        let r2 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider)
            .await
            .unwrap();
        assert_eq!(r2.len(), 2, "negative cache hit must return all tools");
    }

    #[tokio::test]
    async fn cache_negative_hit_clears_on_reset() {
        let tools: Vec<McpTool> = (0..2).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        // Fail on the first LLM call; succeed on the second (after cache.reset()).
        let provider = MockProvider::with_responses(vec![r#"["t0","t1"]"#.into()])
            .with_errors(vec![zeph_llm::LlmError::Other("simulated failure".into())]);
        let params = params_with_max(0);
        let mut cache = PruningCache::new();

        // First call: LLM fails → negative entry cached.
        let r1 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider).await;
        assert!(r1.is_err());

        // Reset clears the negative entry.
        cache.reset();

        // After reset the LLM is retried; the queued success response is now returned.
        let r2 = prune_tools_cached(&mut cache, &tools, "query", &params, &provider)
            .await
            .unwrap();
        assert_eq!(r2.len(), 2, "after reset the LLM must be retried");
    }
}
