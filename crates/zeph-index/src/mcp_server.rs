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
        description: "Return a BFS subgraph of containment relationships (e.g. impl → methods) \
            up to `depth` hops from a starting symbol. Default depth=2, max=3. \
            Note: this reflects static AST containment (struct/impl → fields/methods), \
            not runtime call relationships — cross-function calls are not traced."
            .into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal Rust source file to a temp dir and return the dir + server.
    fn setup_with_rust_file() -> (tempfile::TempDir, IndexMcpServer) {
        let dir = tempfile::TempDir::new().unwrap();
        let src = dir.path().join("lib.rs");
        std::fs::write(
            &src,
            r"pub fn hello() {}
pub fn world() {}
pub struct Foo { pub x: i32 }
impl Foo {
    pub fn bar(&self) {}
}
",
        )
        .unwrap();
        let server = IndexMcpServer::new(dir.path());
        (dir, server)
    }

    #[test]
    fn tool_definitions_returns_four_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = IndexMcpServer::new(dir.path());
        let defs = server.tool_definitions();
        assert_eq!(defs.len(), 4);
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(ids.contains(&"symbol_definition"));
        assert!(ids.contains(&"find_text_references"));
        assert!(ids.contains(&"call_graph"));
        assert!(ids.contains(&"module_summary"));
    }

    #[test]
    fn is_tool_retryable_all_tools() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = IndexMcpServer::new(dir.path());
        assert!(server.is_tool_retryable("symbol_definition"));
        assert!(server.is_tool_retryable("find_text_references"));
        assert!(server.is_tool_retryable("call_graph"));
        assert!(server.is_tool_retryable("module_summary"));
        assert!(!server.is_tool_retryable("shell"));
    }

    #[test]
    fn symbol_definition_finds_known_symbol() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = SymbolDefinitionParams {
            name: "hello".to_string(),
        };
        let result = run_symbol_definition(&index, &params);
        assert!(!result.is_null(), "should find 'hello' symbol");
        // Result should contain file and line fields.
        assert!(result.get("file").is_some() || result.is_array());
    }

    #[test]
    fn symbol_definition_returns_null_for_unknown() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = SymbolDefinitionParams {
            name: "nonexistent_xyz".to_string(),
        };
        let result = run_symbol_definition(&index, &params);
        assert!(result.is_null());
    }

    #[test]
    fn find_text_references_finds_occurrences() {
        let (dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = FindTextReferencesParams {
            name: "hello".to_string(),
            max_results: 10,
        };
        let result = run_find_text_references(dir.path(), &index, &params);
        let arr = result.as_array().unwrap();
        assert!(
            !arr.is_empty(),
            "should find at least one reference to 'hello'"
        );
    }

    #[test]
    fn find_text_references_empty_for_unknown() {
        let (dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = FindTextReferencesParams {
            name: "zzz_not_present_zzz".to_string(),
            max_results: 10,
        };
        let result = run_find_text_references(dir.path(), &index, &params);
        assert!(result.as_array().unwrap().is_empty());
    }

    #[test]
    fn call_graph_returns_nodes_and_edges() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = CallGraphParams {
            fn_name: "Foo".to_string(),
            depth: 2,
        };
        let result = run_call_graph(&index, params);
        assert!(result.get("nodes").is_some());
        assert!(result.get("edges").is_some());
        assert_eq!(result["truncated"], serde_json::Value::Bool(false));
        let nodes = result["nodes"].as_array().unwrap();
        // Root node must always be present.
        assert!(nodes.iter().any(|n| n.as_str() == Some("Foo")));
    }

    #[test]
    fn module_summary_returns_symbols() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = ModuleSummaryParams {
            path: "lib.rs".to_string(),
        };
        let result = run_module_summary(&index, &params);
        assert!(
            !result.is_null(),
            "module_summary for lib.rs should not be null"
        );
        let entities = result["entities"].as_array().unwrap();
        assert!(!entities.is_empty());
        // At least one of hello/world/Foo should be listed.
        let names: Vec<&str> = entities.iter().filter_map(|e| e["name"].as_str()).collect();
        assert!(
            names.contains(&"hello") || names.contains(&"world") || names.contains(&"Foo"),
            "expected at least one known symbol, got: {names:?}"
        );
    }

    #[test]
    fn module_summary_returns_null_for_unknown_path() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = ModuleSummaryParams {
            path: "does_not_exist.rs".to_string(),
        };
        let result = run_module_summary(&index, &params);
        assert!(result.is_null());
    }

    #[test]
    fn call_graph_depth_zero_returns_only_root() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = CallGraphParams {
            fn_name: "Foo".to_string(),
            depth: 0,
        };
        let result = run_call_graph(&index, params);
        let nodes = result["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1, "depth=0 must return only the root node");
        assert_eq!(nodes[0].as_str(), Some("Foo"));
        let edges = result["edges"].as_array().unwrap();
        assert!(edges.is_empty(), "depth=0 must return no edges");
    }

    #[test]
    fn call_graph_unknown_root_returns_single_node_no_edges() {
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = CallGraphParams {
            fn_name: "nonexistent_fn_xyz".to_string(),
            depth: 2,
        };
        let result = run_call_graph(&index, params);
        let nodes = result["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].as_str(), Some("nonexistent_fn_xyz"));
        let edges = result["edges"].as_array().unwrap();
        assert!(edges.is_empty());
    }

    #[test]
    fn call_graph_depth_clamped_to_three() {
        // Depth > 3 must be clamped. The BFS must terminate and return truncated=false.
        let (_dir, server) = setup_with_rust_file();
        let index = server.index.blocking_read();
        let params = CallGraphParams {
            fn_name: "Foo".to_string(),
            depth: 99,
        };
        let result = run_call_graph(&index, params);
        assert_eq!(result["truncated"], serde_json::Value::Bool(false));
    }

    #[test]
    fn find_text_references_max_results_respected() {
        let dir = tempfile::TempDir::new().unwrap();
        // Write a file with many occurrences of "target".
        let content = "fn target() {}\n".repeat(50);
        std::fs::write(dir.path().join("many.rs"), &content).unwrap();
        let server = IndexMcpServer::new(dir.path());
        let index = server.index.blocking_read();
        let params = FindTextReferencesParams {
            name: "target".to_string(),
            max_results: 5,
        };
        let result = run_find_text_references(dir.path(), &index, &params);
        let arr = result.as_array().unwrap();
        assert!(
            arr.len() <= 5,
            "must not exceed max_results, got {}",
            arr.len()
        );
    }

    fn make_call(tool_id: &str, params: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: match params {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            },
            caller_id: None,
        }
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_tool_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = IndexMcpServer::new(dir.path());
        let call = make_call("not_a_real_tool", serde_json::json!({}));
        let result = server.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none(), "unknown tool_id must return None");
    }

    #[tokio::test]
    async fn execute_tool_call_symbol_definition_known() {
        let (_dir, server) = setup_with_rust_file();
        let call = make_call("symbol_definition", serde_json::json!({ "name": "hello" }));
        let result = server.execute_tool_call(&call).await.unwrap();
        assert!(
            result.is_some(),
            "symbol_definition should return Some for a known symbol"
        );
        let output = result.unwrap();
        assert_eq!(output.tool_name, "symbol_definition");
    }

    #[tokio::test]
    async fn execute_tool_call_module_summary_known() {
        let (_dir, server) = setup_with_rust_file();
        let call = make_call("module_summary", serde_json::json!({ "path": "lib.rs" }));
        let result = server.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        let output = result.unwrap();
        assert_eq!(output.tool_name, "module_summary");
    }

    #[tokio::test]
    async fn server_on_empty_directory_builds_empty_index() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = IndexMcpServer::new(dir.path());
        let index = server.index.read().await;
        assert!(index.definitions.is_empty());
        assert!(index.modules.is_empty());
        assert!(index.call_edges.is_empty());
    }
}
