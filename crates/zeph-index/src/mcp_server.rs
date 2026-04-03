// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-process MCP server exposing AST-based code navigation tools.
//!
//! Implements [`ToolExecutor`] so it can be composed into the tool executor pipeline
//! alongside external MCP servers without requiring JSON-RPC transport overhead.
//!
//! Cross-crate reference limitation: tree-sitter parses files independently and cannot
//! resolve cross-crate use/import paths. `find_text_references` is a textual search —
//! it may include false positives from comments, strings, and unrelated symbols with
//! the same name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;
use zeph_tools::{
    ClaimSource, ToolCall, ToolError, ToolOutput,
    executor::{ToolExecutor, deserialize_params},
    registry::{InvocationHint, ToolDef},
    truncate_tool_output,
};

use crate::languages::detect_language;
use crate::repo_map::{SymbolInfo, SymbolKind, Visibility, extract_symbols};

/// In-memory symbol index built from tree-sitter parse results.
#[derive(Default)]
struct SymbolIndex {
    /// `canonical_name` -> `Vec<SymbolDef>` (multiple definitions possible across files)
    definitions: HashMap<String, Vec<SymbolDef>>,
    /// `file_path` -> `Vec<SymbolInfo>`
    modules: HashMap<PathBuf, Vec<SymbolInfo>>,
    /// `fn_name` -> `Vec<fn_name>` (direct call targets, heuristic from child symbols)
    call_edges: HashMap<String, Vec<String>>,
}

#[derive(Clone)]
struct SymbolDef {
    file: PathBuf,
    line: usize,
    kind: SymbolKind,
    visibility: Visibility,
}

/// In-process MCP server exposing AST-based code navigation tools.
pub struct IndexMcpServer {
    project_root: PathBuf,
    index: Arc<RwLock<SymbolIndex>>,
}

impl IndexMcpServer {
    /// Create a new `IndexMcpServer` and build the initial symbol index.
    ///
    /// Index building is synchronous and happens inline. For large repos this may
    /// take a few hundred milliseconds — call from a background task if needed.
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let root = project_root.into();
        let index = build_index(&root);
        Self {
            project_root: root,
            index: Arc::new(RwLock::new(index)),
        }
    }

    /// Rebuild the symbol index from the project root.
    ///
    /// Call this when watcher events indicate file changes.
    pub async fn refresh(&self) {
        let index = build_index(&self.project_root);
        *self.index.write().await = index;
    }
}

fn build_index(root: &Path) -> SymbolIndex {
    let mut idx = SymbolIndex::default();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(lang) = detect_language(path) else {
            continue;
        };
        let Some(grammar) = lang.grammar() else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let symbols = extract_symbols(&source, &grammar, lang);
        if symbols.is_empty() {
            continue;
        }

        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();

        for sym in &symbols {
            let def = SymbolDef {
                file: rel.clone(),
                line: sym.line,
                kind: sym.kind,
                visibility: sym.visibility,
            };
            idx.definitions
                .entry(sym.name.clone())
                .or_default()
                .push(def);

            // Record call edges from impl/class children.
            if !sym.children.is_empty() {
                let parent = sym.name.clone();
                for child in &sym.children {
                    idx.call_edges
                        .entry(parent.clone())
                        .or_default()
                        .push(child.name.clone());
                    // Also index child definitions.
                    let child_def = SymbolDef {
                        file: rel.clone(),
                        line: child.line,
                        kind: child.kind,
                        visibility: child.visibility,
                    };
                    idx.definitions
                        .entry(child.name.clone())
                        .or_default()
                        .push(child_def);
                }
            }
        }

        idx.modules.insert(rel, symbols);
    }

    idx
}

// ── Tool parameter schemas ─────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
struct SymbolDefinitionParams {
    /// Symbol name to look up.
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct FindTextReferencesParams {
    /// Symbol name to search for.
    name: String,
    /// Maximum number of results to return (default: 20).
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_max_results() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
struct CallGraphParams {
    /// Starting function/method name.
    fn_name: String,
    /// BFS depth (default: 2, max: 3).
    #[serde(default = "default_depth")]
    depth: u32,
}

fn default_depth() -> u32 {
    2
}

#[derive(Deserialize, JsonSchema)]
struct ModuleSummaryParams {
    /// Relative file path (e.g. `src/main.rs`).
    path: String,
}

// ── Tool implementations ───────────────────────────────────────────────────────

fn tool_symbol_definition() -> ToolDef {
    ToolDef {
        id: "symbol_definition".into(),
        description: "Look up a symbol by name. Returns file path, line number, kind, and visibility. Returns null if not found.".into(),
        schema: schemars::schema_for!(SymbolDefinitionParams),
        invocation: InvocationHint::ToolCall,
    }
}

fn tool_find_text_references() -> ToolDef {
    ToolDef {
        id: "find_text_references".into(),
        description: "Find all files where a symbol name appears (textual search, not semantic). May include false positives from comments and strings.".into(),
        schema: schemars::schema_for!(FindTextReferencesParams),
        invocation: InvocationHint::ToolCall,
    }
}

fn tool_call_graph() -> ToolDef {
    ToolDef {
        id: "call_graph".into(),
        description: "Return a BFS subgraph of method/function relationships up to `depth` hops from a starting symbol. Default depth=2, max=3.".into(),
        schema: schemars::schema_for!(CallGraphParams),
        invocation: InvocationHint::ToolCall,
    }
}

fn tool_module_summary() -> ToolDef {
    ToolDef {
        id: "module_summary".into(),
        description:
            "Return the list of top-level symbols defined in a file, given its relative path."
                .into(),
        schema: schemars::schema_for!(ModuleSummaryParams),
        invocation: InvocationHint::ToolCall,
    }
}

fn run_symbol_definition(
    index: &SymbolIndex,
    params: &SymbolDefinitionParams,
) -> serde_json::Value {
    match index.definitions.get(&params.name) {
        None => serde_json::Value::Null,
        Some(defs) => {
            let results: Vec<serde_json::Value> = defs
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "file": d.file.display().to_string(),
                        "line": d.line + 1,
                        "kind": format!("{:?}", d.kind).to_lowercase(),
                        "visibility": format!("{:?}", d.visibility).to_lowercase(),
                    })
                })
                .collect();
            if results.len() == 1 {
                results
                    .into_iter()
                    .next()
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Array(results)
            }
        }
    }
}

fn run_find_text_references(
    root: &Path,
    index: &SymbolIndex,
    params: &FindTextReferencesParams,
) -> serde_json::Value {
    let name = &params.name;
    let mut hits: Vec<serde_json::Value> = Vec::new();

    'outer: for rel_path in index.modules.keys() {
        let abs = root.join(rel_path);
        let Ok(source) = std::fs::read_to_string(&abs) else {
            continue;
        };
        for (line_idx, line) in source.lines().enumerate() {
            if line.contains(name.as_str()) {
                hits.push(serde_json::json!({
                    "file": rel_path.display().to_string(),
                    "line": line_idx + 1,
                    "context": line.trim(),
                }));
                if hits.len() >= params.max_results {
                    break 'outer;
                }
            }
        }
    }

    serde_json::Value::Array(hits)
}

fn run_call_graph(index: &SymbolIndex, params: CallGraphParams) -> serde_json::Value {
    let depth = params.depth.min(3);
    let mut nodes: Vec<String> = Vec::new();
    let mut edges: Vec<serde_json::Value> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<(String, u32)> = std::collections::VecDeque::new();

    queue.push_back((params.fn_name.clone(), 0));
    visited.insert(params.fn_name.clone());
    nodes.push(params.fn_name);

    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= depth {
            continue;
        }
        let Some(callees) = index.call_edges.get(&current) else {
            continue;
        };
        for callee in callees {
            edges.push(serde_json::json!({ "from": current, "to": callee }));
            if visited.insert(callee.clone()) {
                nodes.push(callee.clone());
                queue.push_back((callee.clone(), current_depth + 1));
            }
        }
    }

    serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "truncated": false,
    })
}

fn run_module_summary(index: &SymbolIndex, params: &ModuleSummaryParams) -> serde_json::Value {
    let path = PathBuf::from(&params.path);
    match index.modules.get(&path) {
        None => serde_json::Value::Null,
        Some(symbols) => {
            let entities: Vec<serde_json::Value> = symbols
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "kind": format!("{:?}", s.kind).to_lowercase(),
                        "line": s.line + 1,
                        "visibility": format!("{:?}", s.visibility).to_lowercase(),
                    })
                })
                .collect();
            serde_json::json!({ "entities": entities })
        }
    }
}

// ── ToolExecutor impl ──────────────────────────────────────────────────────────

impl ToolExecutor for IndexMcpServer {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            tool_symbol_definition(),
            tool_find_text_references(),
            tool_call_graph(),
            tool_module_summary(),
        ]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let index = self.index.read().await;
        let result = match call.tool_id.as_str() {
            "symbol_definition" => {
                let params: SymbolDefinitionParams = deserialize_params(&call.params)?;
                run_symbol_definition(&index, &params)
            }
            "find_text_references" => {
                let params: FindTextReferencesParams = deserialize_params(&call.params)?;
                run_find_text_references(&self.project_root, &index, &params)
            }
            "call_graph" => {
                let params: CallGraphParams = deserialize_params(&call.params)?;
                run_call_graph(&index, params)
            }
            "module_summary" => {
                let params: ModuleSummaryParams = deserialize_params(&call.params)?;
                run_module_summary(&index, &params)
            }
            _ => return Ok(None),
        };

        let summary = serde_json::to_string_pretty(&result).unwrap_or_default();
        Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary: truncate_tool_output(&summary),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: Some(result),
            claim_source: Some(ClaimSource::CodeSearch),
        }))
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        // All index tools are read-only — safe to retry.
        matches!(
            tool_id,
            "symbol_definition" | "find_text_references" | "call_graph" | "module_summary"
        )
    }
}
