// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use zeph_common::ToolName;

use crate::executor::{
    ClaimSource, ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params,
};
use crate::file::expand_tilde;
use crate::registry::{InvocationHint, ToolDef};

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

use zeph_common::treesitter::{
    GO_SYM_Q, JS_SYM_Q, PYTHON_SYM_Q, RUST_SYM_Q, TS_SYM_Q, compile_query,
};

struct LangInfo {
    grammar: tree_sitter::Language,
    symbol_query: Option<&'static Query>,
}

fn lang_info_for_path(path: &Path) -> Option<LangInfo> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "rs" => {
            static Q: LazyLock<Option<Query>> = LazyLock::new(|| {
                let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
                compile_query(&lang, RUST_SYM_Q, "rust")
            });
            Some(LangInfo {
                grammar: tree_sitter_rust::LANGUAGE.into(),
                symbol_query: Q.as_ref(),
            })
        }
        "py" | "pyi" => {
            static Q: LazyLock<Option<Query>> = LazyLock::new(|| {
                let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
                compile_query(&lang, PYTHON_SYM_Q, "python")
            });
            Some(LangInfo {
                grammar: tree_sitter_python::LANGUAGE.into(),
                symbol_query: Q.as_ref(),
            })
        }
        "js" | "jsx" | "mjs" | "cjs" => {
            static Q: LazyLock<Option<Query>> = LazyLock::new(|| {
                let lang: tree_sitter::Language = tree_sitter_javascript::LANGUAGE.into();
                compile_query(&lang, JS_SYM_Q, "javascript")
            });
            Some(LangInfo {
                grammar: tree_sitter_javascript::LANGUAGE.into(),
                symbol_query: Q.as_ref(),
            })
        }
        "ts" | "tsx" | "mts" | "cts" => {
            static Q: LazyLock<Option<Query>> = LazyLock::new(|| {
                let lang: tree_sitter::Language =
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
                compile_query(&lang, TS_SYM_Q, "typescript")
            });
            Some(LangInfo {
                grammar: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                symbol_query: Q.as_ref(),
            })
        }
        "go" => {
            static Q: LazyLock<Option<Query>> = LazyLock::new(|| {
                let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
                compile_query(&lang, GO_SYM_Q, "go")
            });
            Some(LangInfo {
                grammar: tree_sitter_go::LANGUAGE.into(),
                symbol_query: Q.as_ref(),
            })
        }
        "sh" | "bash" | "zsh" => Some(LangInfo {
            grammar: tree_sitter_bash::LANGUAGE.into(),
            symbol_query: None,
        }),
        "toml" => Some(LangInfo {
            grammar: tree_sitter_toml_ng::LANGUAGE.into(),
            symbol_query: None,
        }),
        "json" | "jsonc" => Some(LangInfo {
            grammar: tree_sitter_json::LANGUAGE.into(),
            symbol_query: None,
        }),
        "md" | "markdown" => Some(LangInfo {
            grammar: tree_sitter_md::LANGUAGE.into(),
            symbol_query: None,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SearchCodeSource {
    Semantic,
    Structural,
    LspSymbol,
    LspReferences,
    GrepFallback,
}

impl SearchCodeSource {
    fn label(self) -> &'static str {
        match self {
            Self::Semantic => "vector search",
            Self::Structural => "tree-sitter",
            Self::LspSymbol => "LSP symbol search",
            Self::LspReferences => "LSP references",
            Self::GrepFallback => "grep fallback",
        }
    }

    #[must_use]
    pub fn default_score(self) -> f32 {
        match self {
            Self::Structural => 0.98,
            Self::LspSymbol => 0.95,
            Self::LspReferences => 0.90,
            Self::Semantic => 0.75,
            Self::GrepFallback => 0.45,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchCodeHit {
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub snippet: String,
    pub source: SearchCodeSource,
    pub score: f32,
    pub symbol_name: Option<String>,
}

pub trait SemanticSearchBackend: Send + Sync {
    fn search<'a>(
        &'a self,
        query: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, ToolError>> + Send + 'a>>;
}

pub trait LspSearchBackend: Send + Sync {
    fn workspace_symbol<'a>(
        &'a self,
        symbol: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, ToolError>> + Send + 'a>>;

    fn references<'a>(
        &'a self,
        symbol: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, ToolError>> + Send + 'a>>;
}

#[derive(Deserialize, JsonSchema)]
struct SearchCodeParams {
    /// Natural-language query for semantic search.
    #[serde(default)]
    query: Option<String>,
    /// Exact or partial symbol name.
    #[serde(default)]
    symbol: Option<String>,
    /// Optional glob restricting files, for example `crates/zeph-tools/**`.
    #[serde(default)]
    file_pattern: Option<String>,
    /// Also return reference locations when `symbol` is provided.
    #[serde(default)]
    include_references: bool,
    /// Cap on returned locations.
    #[serde(default = "default_max_results")]
    max_results: usize,
}

const fn default_max_results() -> usize {
    10
}

/// Maximum directory entries a single structural/grep walk may visit before it
/// is truncated. Bounds `allowed_paths` resolving to a huge tree (e.g. `~`).
///
/// Large monorepos can exceed a few thousand entries in normal operation, so this is
/// generous; `MAX_WALK_DURATION` is the real backstop against the original
/// pathological-scope hang (e.g. `allowed_paths` resolving to `~`), since wall-clock
/// time bounds the walk regardless of how many entries a given filesystem can visit
/// per second.
const MAX_WALK_ENTRIES: usize = 50_000;

/// Maximum wall-clock time a single structural/grep walk may run before it is
/// truncated, independent of entry count (guards against slow filesystems).
const MAX_WALK_DURATION: Duration = Duration::from_secs(2);

/// Safety net shared by the structural and grep-fallback directory walks.
///
/// `allowed_paths` is user/config controlled and may resolve to an
/// unexpectedly broad root (e.g. `~`). Without a bound, a recursive walk over
/// such a root can run indefinitely. `tick` caps both the number of entries
/// visited and elapsed wall-clock time; once either limit is hit, the walk
/// stops and reports partial results as truncated instead of hanging.
struct WalkBudget {
    started: Instant,
    visited: usize,
    truncated: bool,
}

impl WalkBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            visited: 0,
            truncated: false,
        }
    }

    /// Records one visited directory entry; returns `true` once the walk
    /// should stop because the budget is exhausted.
    fn tick(&mut self) -> bool {
        if self.truncated {
            return true;
        }
        self.visited += 1;
        if self.visited > MAX_WALK_ENTRIES || self.started.elapsed() > MAX_WALK_DURATION {
            self.truncated = true;
        }
        self.truncated
    }
}

/// Bundles the two pieces of walk-wide mutable state threaded through the
/// structural/grep directory recursion: the [`WalkBudget`] and the symlink-cycle
/// guard (canonical paths of directories on the *active* recursion path). Grouped
/// into one struct so the walk functions stay under clippy's argument-count limit.
struct WalkState<'a> {
    budget: &'a mut WalkBudget,
    ancestors: &'a mut Vec<PathBuf>,
}

/// Canonicalizes `current` and checks it against the active recursion stack in
/// `ancestors` — the canonical paths of directories on the *active* recursion path
/// (not every directory ever visited). A symlinked directory is only a cycle when
/// following it would revisit one of those ancestors; legitimate symlinks that point
/// elsewhere (a vendored dependency, a symlinked source file) are not cycles.
///
/// Returns `true` when `current` would revisit an ancestor — the caller must stop
/// without recursing further. Otherwise pushes `current`'s canonical path onto
/// `ancestors` and returns `false`; the caller **must** pop it (`ancestors.pop()`)
/// once the recursive body for `current` returns.
///
/// Shared by [`collect_structural_hits`] and [`collect_grep_hits`] so the two walk
/// modes cannot silently diverge (#5588).
fn enters_symlink_cycle(ancestors: &mut Vec<PathBuf>, current: &Path) -> bool {
    let canonical = current
        .canonicalize()
        .unwrap_or_else(|_| current.to_path_buf());
    if ancestors.contains(&canonical) {
        return true;
    }
    ancestors.push(canonical);
    false
}

pub struct SearchCodeExecutor {
    allowed_paths: Vec<PathBuf>,
    semantic_backend: Option<std::sync::Arc<dyn SemanticSearchBackend>>,
    lsp_backend: Option<std::sync::Arc<dyn LspSearchBackend>>,
}

impl std::fmt::Debug for SearchCodeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchCodeExecutor")
            .field("allowed_paths", &self.allowed_paths)
            .field("has_semantic_backend", &self.semantic_backend.is_some())
            .field("has_lsp_backend", &self.lsp_backend.is_some())
            .finish()
    }
}

impl SearchCodeExecutor {
    #[must_use]
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        let paths = if allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            allowed_paths.into_iter().map(expand_tilde).collect()
        };
        Self {
            allowed_paths: paths
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
            semantic_backend: None,
            lsp_backend: None,
        }
    }

    #[must_use]
    pub fn with_semantic_backend(
        mut self,
        backend: std::sync::Arc<dyn SemanticSearchBackend>,
    ) -> Self {
        self.semantic_backend = Some(backend);
        self
    }

    #[must_use]
    pub fn with_lsp_backend(mut self, backend: std::sync::Arc<dyn LspSearchBackend>) -> Self {
        self.lsp_backend = Some(backend);
        self
    }

    async fn handle_search_code(
        &self,
        params: &SearchCodeParams,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let query = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let symbol = params
            .symbol
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if query.is_none() && symbol.is_none() {
            return Err(ToolError::InvalidParams {
                message: "at least one of `query` or `symbol` must be provided".into(),
            });
        }

        let max_results = params.max_results.clamp(1, 50);
        let mut hits = Vec::new();
        let mut walk_truncated = false;

        if let Some(query) = query
            && let Some(backend) = &self.semantic_backend
        {
            hits.extend(
                backend
                    .search(query, params.file_pattern.as_deref(), max_results)
                    .await?,
            );
        }

        if let Some(symbol) = symbol {
            let paths = self.allowed_paths.clone();
            let sym = symbol.to_owned();
            let pat = params.file_pattern.clone();
            let (structural_hits, structural_truncated) = tokio::task::spawn_blocking(move || {
                collect_all_structural_hits(&paths, &sym, pat.as_deref(), max_results)
            })
            .await
            .map_err(|e| ToolError::Execution(e.into()))??;
            hits.extend(structural_hits);
            walk_truncated |= structural_truncated;

            if let Some(backend) = &self.lsp_backend {
                if let Ok(lsp_hits) = backend
                    .workspace_symbol(symbol, params.file_pattern.as_deref(), max_results)
                    .await
                {
                    hits.extend(lsp_hits);
                }
                if params.include_references
                    && let Ok(lsp_refs) = backend
                        .references(symbol, params.file_pattern.as_deref(), max_results)
                        .await
                {
                    hits.extend(lsp_refs);
                }
            }
        }

        if hits.is_empty() {
            let fallback_term = symbol.or(query).unwrap_or_default();
            let (grep_hits, grep_truncated) =
                self.grep_fallback(fallback_term, params.file_pattern.as_deref(), max_results)?;
            hits.extend(grep_hits);
            walk_truncated |= grep_truncated;
        }

        let merged = dedupe_hits(hits, max_results);
        let root = self
            .allowed_paths
            .first()
            .map_or(Path::new("."), PathBuf::as_path);
        Ok(Some(build_search_code_output(
            &merged,
            root,
            walk_truncated,
        )))
    }

    /// Returns the collected hits alongside whether the walk was cut short by
    /// [`WalkBudget`].
    fn grep_fallback(
        &self,
        pattern: &str,
        file_pattern: Option<&str>,
        max_results: usize,
    ) -> Result<(Vec<SearchCodeHit>, bool), ToolError> {
        let matcher = file_pattern
            .map(glob::Pattern::new)
            .transpose()
            .map_err(|e| ToolError::InvalidParams {
                message: format!("invalid file_pattern: {e}"),
            })?;
        let escaped = regex::escape(pattern);
        let regex = regex::RegexBuilder::new(&escaped)
            .case_insensitive(true)
            .build()
            .map_err(|e| ToolError::InvalidParams {
                message: e.to_string(),
            })?;
        let mut hits = Vec::new();
        let mut budget = WalkBudget::new();
        for root in &self.allowed_paths {
            let mut ancestors = Vec::new();
            let mut state = WalkState {
                budget: &mut budget,
                ancestors: &mut ancestors,
            };
            collect_grep_hits(
                root,
                root,
                matcher.as_ref(),
                &regex,
                &mut hits,
                max_results,
                &mut state,
            )?;
            if hits.len() >= max_results || budget.truncated {
                break;
            }
        }
        Ok((hits, budget.truncated))
    }
}

impl ToolExecutor for SearchCodeExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "tools.search_code.execute", skip_all)
    )]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "search_code" {
            return Ok(None);
        }
        let params: SearchCodeParams = deserialize_params(&call.params)?;
        self.handle_search_code(&params).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "search_code".into(),
            description: "Search the codebase using semantic, structural, and LSP sources. Use only to search source code files — not for user-provided facts, preferences, or statements made in conversation.\n\nParameters: query (string, optional) - natural language description to find semantically similar code; symbol (string, optional) - exact or partial symbol name for definition search; file_pattern (string, optional) - glob restricting files; include_references (boolean, optional) - also return symbol references when LSP is available; max_results (integer, optional) - cap results 1-50, default 10\nReturns: ranked code locations with file path, line range, snippet, source label, and score\nErrors: InvalidParams when both query and symbol are empty\nExample: {\"query\": \"where is retry backoff calculated\", \"symbol\": \"retry_backoff_ms\", \"include_references\": true}".into(),
            schema: schemars::schema_for!(SearchCodeParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]
    }
}

/// Traverse `allowed_paths` collecting structural symbol hits synchronously.
///
/// Extracted as a free function so callers can run it inside
/// `tokio::task::spawn_blocking` without borrowing `self`.
///
/// Returns the collected hits alongside whether the walk was cut short by
/// [`WalkBudget`] (see its docs for why the bound exists).
fn collect_all_structural_hits(
    allowed_paths: &[PathBuf],
    symbol: &str,
    file_pattern: Option<&str>,
    max_results: usize,
) -> Result<(Vec<SearchCodeHit>, bool), ToolError> {
    let matcher = file_pattern
        .map(glob::Pattern::new)
        .transpose()
        .map_err(|e| ToolError::InvalidParams {
            message: format!("invalid file_pattern: {e}"),
        })?;
    let mut hits = Vec::new();
    let symbol_lower = symbol.to_lowercase();
    let mut budget = WalkBudget::new();
    for root in allowed_paths {
        let mut ancestors = Vec::new();
        let mut state = WalkState {
            budget: &mut budget,
            ancestors: &mut ancestors,
        };
        collect_structural_hits(
            root,
            root,
            matcher.as_ref(),
            &symbol_lower,
            &mut hits,
            &mut state,
        )?;
        if hits.len() >= max_results || budget.truncated {
            break;
        }
    }
    Ok((hits, budget.truncated))
}

fn dedupe_hits(mut hits: Vec<SearchCodeHit>, max_results: usize) -> Vec<SearchCodeHit> {
    let mut merged: HashMap<(String, usize, usize), SearchCodeHit> = HashMap::new();
    for hit in hits.drain(..) {
        let key = (hit.file_path.clone(), hit.line_start, hit.line_end);
        merged
            .entry(key)
            .and_modify(|existing| {
                if hit.score > existing.score {
                    existing.score = hit.score;
                    existing.snippet.clone_from(&hit.snippet);
                    existing.symbol_name = hit.symbol_name.clone().or(existing.symbol_name.clone());
                }
                if existing.source != hit.source {
                    existing.source = if existing.score >= hit.score {
                        existing.source
                    } else {
                        hit.source
                    };
                }
            })
            .or_insert(hit);
    }

    let mut merged = merged.into_values().collect::<Vec<_>>();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.line_start.cmp(&b.line_start))
    });
    merged.truncate(max_results);
    merged
}

/// Assembles the final `search_code` [`ToolOutput`] from merged hits.
///
/// `walk_truncated` reports whether any directory walk (structural or grep
/// fallback) was cut short by [`WalkBudget`] — surfaced both as a note in the
/// summary text and as a `truncated` field in `raw_response` so callers can
/// detect incomplete results programmatically.
fn build_search_code_output(
    hits: &[SearchCodeHit],
    root: &Path,
    walk_truncated: bool,
) -> ToolOutput {
    let mut summary = format_hits(hits, root);
    if walk_truncated {
        summary.push_str(
            "\n\n[search truncated: directory walk exceeded the safety budget; \
             results may be incomplete — narrow `allowed_paths` or `file_pattern`]",
        );
    }
    let locations = hits
        .iter()
        .map(|hit| hit.file_path.clone())
        .collect::<Vec<_>>();
    let raw_response = serde_json::json!({
        "results": hits.iter().map(|hit| {
            serde_json::json!({
                "file_path": hit.file_path,
                "line_start": hit.line_start,
                "line_end": hit.line_end,
                "snippet": hit.snippet,
                "source": hit.source.label(),
                "score": hit.score,
                "symbol_name": hit.symbol_name,
            })
        }).collect::<Vec<_>>(),
        "truncated": walk_truncated,
    });

    ToolOutput {
        tool_name: ToolName::new("search_code"),
        summary,
        blocks_executed: 1,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: Some(locations),
        raw_response: Some(raw_response),
        claim_source: Some(ClaimSource::CodeSearch),
    }
}

fn format_hits(hits: &[SearchCodeHit], root: &Path) -> String {
    if hits.is_empty() {
        return "No code matches found.".into();
    }

    hits.iter()
        .enumerate()
        .map(|(idx, hit)| {
            let display_path = Path::new(&hit.file_path)
                .strip_prefix(root)
                .map_or_else(|_| hit.file_path.clone(), |p| p.display().to_string());
            format!(
                "[{}] {}:{}-{}\n    {}\n    source: {}\n    score: {:.2}",
                idx + 1,
                display_path,
                hit.line_start,
                hit.line_end,
                hit.snippet.replace('\n', " "),
                hit.source.label(),
                hit.score,
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Walks `current`, guarding against symlink cycles via [`enters_symlink_cycle`], still
/// bounded by [`WalkBudget`] via `state.budget`.
fn collect_structural_hits(
    root: &Path,
    current: &Path,
    matcher: Option<&glob::Pattern>,
    symbol_lower: &str,
    hits: &mut Vec<SearchCodeHit>,
    state: &mut WalkState<'_>,
) -> Result<(), ToolError> {
    if should_skip_path(current) || state.budget.truncated {
        return Ok(());
    }

    if enters_symlink_cycle(state.ancestors, current) {
        return Ok(());
    }
    let result = collect_structural_hits_inner(root, current, matcher, symbol_lower, hits, state);
    state.ancestors.pop();
    result
}

fn collect_structural_hits_inner(
    root: &Path,
    current: &Path,
    matcher: Option<&glob::Pattern>,
    symbol_lower: &str,
    hits: &mut Vec<SearchCodeHit>,
    state: &mut WalkState<'_>,
) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(current).map_err(ToolError::Execution)?;
    for entry in entries {
        if state.budget.tick() {
            return Ok(());
        }
        let entry = entry.map_err(ToolError::Execution)?;
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = if meta.file_type().is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target_meta) => target_meta.is_dir(),
                Err(_) => continue, // broken symlink target
            }
        } else {
            meta.is_dir()
        };
        if is_dir {
            collect_structural_hits(root, &path, matcher, symbol_lower, hits, state)?;
            continue;
        }
        if !matches_pattern(root, &path, matcher) {
            continue;
        }
        let Some(info) = lang_info_for_path(&path) else {
            continue;
        };
        let grammar = info.grammar;
        let Some(query) = info.symbol_query.as_ref() else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut parser = Parser::new();
        if parser.set_language(&grammar).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        let mut cursor = QueryCursor::new();
        let capture_names = query.capture_names();
        let def_idx = capture_names.iter().position(|name| *name == "def");
        let name_idx = capture_names.iter().position(|name| *name == "name");
        let (Some(def_idx), Some(name_idx)) = (def_idx, name_idx) else {
            continue;
        };

        let mut query_matches = cursor.matches(query, tree.root_node(), source.as_bytes());
        while let Some(match_) = query_matches.next() {
            let mut def_node = None;
            let mut name = None;
            for capture in match_.captures {
                if capture.index as usize == def_idx {
                    def_node = Some(capture.node);
                }
                if capture.index as usize == name_idx {
                    name = Some(source[capture.node.byte_range()].to_string());
                }
            }
            let Some(name) = name else {
                continue;
            };
            if !name.to_lowercase().contains(symbol_lower) {
                continue;
            }
            let Some(def_node) = def_node else {
                continue;
            };
            hits.push(SearchCodeHit {
                file_path: canonical_string(&path),
                line_start: def_node.start_position().row + 1,
                line_end: def_node.end_position().row + 1,
                snippet: extract_snippet(&source, def_node.start_position().row + 1),
                source: SearchCodeSource::Structural,
                score: SearchCodeSource::Structural.default_score(),
                symbol_name: Some(name),
            });
        }
    }
    Ok(())
}

/// Symlink cycle guard mirrors [`collect_structural_hits`] via [`enters_symlink_cycle`].
fn collect_grep_hits(
    root: &Path,
    current: &Path,
    matcher: Option<&glob::Pattern>,
    regex: &regex::Regex,
    hits: &mut Vec<SearchCodeHit>,
    max_results: usize,
    state: &mut WalkState<'_>,
) -> Result<(), ToolError> {
    if hits.len() >= max_results || should_skip_path(current) || state.budget.truncated {
        return Ok(());
    }

    if enters_symlink_cycle(state.ancestors, current) {
        return Ok(());
    }
    let result = collect_grep_hits_inner(root, current, matcher, regex, hits, max_results, state);
    state.ancestors.pop();
    result
}

fn collect_grep_hits_inner(
    root: &Path,
    current: &Path,
    matcher: Option<&glob::Pattern>,
    regex: &regex::Regex,
    hits: &mut Vec<SearchCodeHit>,
    max_results: usize,
    state: &mut WalkState<'_>,
) -> Result<(), ToolError> {
    let entries = std::fs::read_dir(current).map_err(ToolError::Execution)?;
    for entry in entries {
        if state.budget.tick() {
            return Ok(());
        }
        let entry = entry.map_err(ToolError::Execution)?;
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = if meta.file_type().is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target_meta) => target_meta.is_dir(),
                Err(_) => continue, // broken symlink target
            }
        } else {
            meta.is_dir()
        };
        if is_dir {
            collect_grep_hits(root, &path, matcher, regex, hits, max_results, state)?;
            continue;
        }
        if !matches_pattern(root, &path, matcher) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (idx, line) in source.lines().enumerate() {
            if regex.is_match(line) {
                hits.push(SearchCodeHit {
                    file_path: canonical_string(&path),
                    line_start: idx + 1,
                    line_end: idx + 1,
                    snippet: line.trim().to_string(),
                    source: SearchCodeSource::GrepFallback,
                    score: SearchCodeSource::GrepFallback.default_score(),
                    symbol_name: None,
                });
                if hits.len() >= max_results {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn matches_pattern(root: &Path, path: &Path, matcher: Option<&glob::Pattern>) -> bool {
    let Some(matcher) = matcher else {
        return true;
    };
    let relative = path.strip_prefix(root).unwrap_or(path);
    matcher.matches_path(relative)
}

fn should_skip_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules" | ".zeph"))
}

fn canonical_string(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn extract_snippet(source: &str, line_number: usize) -> String {
    source
        .lines()
        .nth(line_number.saturating_sub(1))
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    struct EmptySemantic;

    impl SemanticSearchBackend for EmptySemantic {
        fn search<'a>(
            &'a self,
            _query: &'a str,
            _file_pattern: Option<&'a str>,
            _max_results: usize,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(async move { Ok(vec![]) })
        }
    }

    #[tokio::test]
    async fn search_code_requires_query_or_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let err = exec.execute_tool_call(&call).await.unwrap_err();
        assert_matches!(err, ToolError::InvalidParams { .. });
    }

    #[tokio::test]
    async fn search_code_finds_structural_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "pub fn retry_backoff_ms() -> u64 { 0 }\n").unwrap();
        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "retry_backoff_ms" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(out.summary.contains("retry_backoff_ms"));
        assert!(out.summary.contains("tree-sitter"));
        assert_eq!(out.tool_name, "search_code");
    }

    #[tokio::test]
    async fn search_code_uses_grep_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mod.rs");
        std::fs::write(&file, "let retry_backoff_ms = 5;\n").unwrap();
        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "query": "retry_backoff_ms" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = exec.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(out.summary.contains("grep fallback"));
    }

    #[test]
    fn walk_budget_truncates_after_entry_limit() {
        let mut budget = WalkBudget::new();
        for _ in 0..MAX_WALK_ENTRIES {
            assert!(!budget.tick(), "budget must not trip before the limit");
        }
        assert!(
            budget.tick(),
            "budget must trip once entries exceed MAX_WALK_ENTRIES"
        );
        assert!(budget.truncated);
    }

    /// Regression test for the hang reported against a broad `allowed_paths`
    /// scope (e.g. `~`): a directory containing far more entries than
    /// `MAX_WALK_ENTRIES` must not be walked exhaustively — the search
    /// returns promptly with a truncation indicator instead of hanging.
    #[tokio::test]
    async fn search_code_bounds_wide_directory_walk() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_WALK_ENTRIES + 200) {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "nonexistent_symbol_xyz" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(
            Duration::from_secs(MAX_WALK_DURATION.as_secs() + 10),
            exec.execute_tool_call(&call),
        )
        .await
        .expect("search_code must return within the walk budget, not hang")
        .unwrap()
        .unwrap();
        assert!(
            out.summary.contains("truncated"),
            "expected truncation note in summary, got: {}",
            out.summary
        );
        let raw = out.raw_response.unwrap();
        assert_eq!(raw["truncated"], serde_json::json!(true));
    }

    /// A directory cycle created via a symlink pointing back to an ancestor
    /// must not send the walk into an infinite loop, even though symlinks are
    /// now followed in general (#5577).
    #[cfg(unix)]
    #[tokio::test]
    async fn search_code_does_not_follow_symlink_loop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "pub fn retry_backoff_ms() -> u64 { 0 }\n").unwrap();
        let loop_link = dir.path().join("self_loop");
        std::os::unix::fs::symlink(dir.path(), &loop_link).unwrap();

        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "retry_backoff_ms" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(Duration::from_secs(10), exec.execute_tool_call(&call))
            .await
            .expect("search_code must not hang on a symlink loop")
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("retry_backoff_ms"));
    }

    /// Unit-level regression test for #5588: `enters_symlink_cycle` is the single
    /// shared implementation of the cycle-detection logic previously duplicated
    /// between `collect_structural_hits` and `collect_grep_hits`.
    #[test]
    fn enters_symlink_cycle_detects_repeated_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let mut ancestors = Vec::new();

        assert!(!enters_symlink_cycle(&mut ancestors, dir.path()));
        assert_eq!(ancestors.len(), 1);

        // Re-entering the same (canonicalized) path is a cycle.
        assert!(enters_symlink_cycle(&mut ancestors, dir.path()));
        // A cycle must not push a duplicate entry.
        assert_eq!(ancestors.len(), 1);
    }

    #[test]
    fn enters_symlink_cycle_allows_distinct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let mut ancestors = Vec::new();

        assert!(!enters_symlink_cycle(&mut ancestors, dir.path()));
        assert!(!enters_symlink_cycle(&mut ancestors, &child));
        assert_eq!(ancestors.len(), 2);
    }

    /// Regression test for #5577 (critic finding M1 / tester coverage gap 1): a
    /// symlink several directories deep pointing back at a non-root ancestor — not
    /// a direct self-loop — must still be caught. This only works because *every*
    /// directory entered (not just symlinked ones) is pushed onto the active
    /// recursion stack; a design that only tracked symlinked directories would miss
    /// this cycle, since `a` here is a plain directory, not a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn search_code_does_not_follow_indirect_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let c = a.join("b").join("c");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(a.join("lib.rs"), "pub fn retry_backoff_ms() -> u64 { 0 }\n").unwrap();
        let back_to_a = c.join("back_to_a");
        std::os::unix::fs::symlink(&a, &back_to_a).unwrap();

        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "retry_backoff_ms" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(Duration::from_secs(10), exec.execute_tool_call(&call))
            .await
            .expect("search_code must not hang on an indirect symlink cycle several levels deep")
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("retry_backoff_ms"));
    }

    /// Regression test for #5577 (tester coverage gap 2): `collect_grep_hits`'s
    /// cycle-detection logic is a hand-duplicated copy of
    /// `collect_structural_hits`'s (see [`WalkState`] docs) and had zero direct
    /// test coverage of its own loop-protection path — only the structural walk's
    /// was exercised by `search_code_does_not_follow_symlink_loop`. Uses `query`
    /// (no `symbol`, no semantic backend attached) so the search falls through to
    /// `grep_fallback`, exactly like `search_code_uses_grep_fallback`.
    #[cfg(unix)]
    #[tokio::test]
    async fn search_code_grep_fallback_does_not_follow_symlink_loop() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mod.rs");
        std::fs::write(&file, "let retry_backoff_ms = 5;\n").unwrap();
        let loop_link = dir.path().join("self_loop");
        std::os::unix::fs::symlink(dir.path(), &loop_link).unwrap();

        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "query": "retry_backoff_ms" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(Duration::from_secs(10), exec.execute_tool_call(&call))
            .await
            .expect("grep fallback must not hang on a symlink loop")
            .unwrap()
            .unwrap();
        assert!(out.summary.contains("grep fallback"));
    }

    /// Regression test for #5577: a symlinked directory that does *not* create a
    /// cycle (e.g. a vendored dependency symlinked into the tree) must still be
    /// walked and searched, not blanket-skipped just because it is a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn search_code_follows_non_looping_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = tempfile::tempdir().unwrap();
        std::fs::write(
            vendor.path().join("vendored.rs"),
            "pub fn vendored_symbol_xyz() -> u64 { 0 }\n",
        )
        .unwrap();
        let link = dir.path().join("vendor_link");
        std::os::unix::fs::symlink(vendor.path(), &link).unwrap();

        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "vendored_symbol_xyz" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(Duration::from_secs(10), exec.execute_tool_call(&call))
            .await
            .expect("search_code must not hang")
            .unwrap()
            .unwrap();
        assert!(
            out.summary.contains("vendored_symbol_xyz"),
            "expected symlinked directory to be walked, got: {}",
            out.summary
        );
    }

    /// Regression test for #5577: a symlinked source file (not a directory) must
    /// still be searched, not skipped just because it is a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn search_code_follows_symlinked_file() {
        let dir = tempfile::tempdir().unwrap();
        let real_file = tempfile::tempdir().unwrap();
        let target = real_file.path().join("real.rs");
        std::fs::write(&target, "pub fn symlinked_file_symbol_xyz() -> u64 { 0 }\n").unwrap();
        let link = dir.path().join("linked.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let call = ToolCall {
            tool_id: "search_code".into(),
            params: serde_json::json!({ "symbol": "symlinked_file_symbol_xyz" })
                .as_object()
                .unwrap()
                .clone(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let out = tokio::time::timeout(Duration::from_secs(10), exec.execute_tool_call(&call))
            .await
            .expect("search_code must not hang")
            .unwrap()
            .unwrap();
        assert!(
            out.summary.contains("symlinked_file_symbol_xyz"),
            "expected symlinked file to be searched, got: {}",
            out.summary
        );
    }

    #[test]
    fn tool_definitions_include_search_code() {
        let exec = SearchCodeExecutor::new(vec![])
            .with_semantic_backend(std::sync::Arc::new(EmptySemantic));
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), "search_code");
    }

    #[test]
    fn format_hits_strips_root_prefix() {
        let root = Path::new("/tmp/myproject");
        let hits = vec![SearchCodeHit {
            file_path: "/tmp/myproject/crates/foo/src/lib.rs".to_owned(),
            line_start: 10,
            line_end: 15,
            snippet: "pub fn example() {}".to_owned(),
            source: SearchCodeSource::GrepFallback,
            score: 0.45,
            symbol_name: None,
        }];
        let output = format_hits(&hits, root);
        assert!(
            output.contains("crates/foo/src/lib.rs"),
            "expected relative path in output, got: {output}"
        );
        assert!(
            !output.contains("/tmp/myproject"),
            "absolute path must not appear in output, got: {output}"
        );
    }

    /// `search_code` description must explicitly state it is not for user-provided facts
    /// so the model does not use it when recalling conversation context (#2475).
    #[tokio::test]
    async fn search_code_description_excludes_user_facts() {
        let dir = tempfile::tempdir().unwrap();
        let exec = SearchCodeExecutor::new(vec![dir.path().to_path_buf()]);
        let defs = exec.tool_definitions();
        let search_code = defs
            .iter()
            .find(|d| d.id.as_ref() == "search_code")
            .unwrap();
        assert!(
            search_code
                .description
                .contains("not for user-provided facts"),
            "search_code description must contain disambiguation phrase; got: {}",
            search_code.description
        );
    }
}
