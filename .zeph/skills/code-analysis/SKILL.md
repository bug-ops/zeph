---
name: code-analysis
description: >
  LSP-based code analysis via mcpls MCP server. Use for compiler-accurate type
  inspection, go-to-definition, find-all-references, diagnostics, call hierarchy,
  workspace symbol search, code actions, rename refactoring, and document formatting.
  Provides real compiler errors — not guesses. Keywords: LSP, language server,
  type, definition, references, diagnostics, hover, symbol, call hierarchy,
  rename, refactor, code actions, rust-analyzer, pyright, gopls, clangd.
license: MIT
compatibility: Requires mcpls MCP server (https://github.com/bug-ops/mcpls) configured as an MCP server. Works with any LSP 3.17+ compliant language server (rust-analyzer, pyright, typescript-language-server, gopls, clangd, zls).
metadata:
  author: zeph
  version: "1.0"
---
# Code Analysis with LSP

Use LSP tools for accurate, compiler-verified code understanding. These tools require the `mcpls`
MCP server to be configured.

## Positions

Positions are **1-based**: line 1, column 1 is the first character. If you read a file and see
line numbers in the output, use those directly — no conversion needed (mcpls translates to 0-based
LSP positions internally).

File paths must be absolute. Relative paths will not resolve correctly.

## Tool Reference

### Code Intelligence

| Tool | Purpose | When to use |
|---|---|---|
| `get_hover` | Type signature, inferred type, and documentation at a position | "What type is X?", "What does this function do?" |
| `get_definition` | Navigate to where a symbol is defined | Read a function/type implementation before reasoning about it |
| `get_references` | Find all usages of a symbol across the workspace | Before renaming, deleting, or changing a symbol's signature |
| `get_completions` | Context-aware suggestions respecting types and scope | Exploring unknown APIs, discovering available methods |
| `get_document_symbols` | Structured outline of a file (functions, types, constants, fields) | Understanding file structure without reading every line |
| `workspace_symbol_search` | Search for a symbol by name across the entire workspace | Know a name but not which file defines it |

### Diagnostics and Correctness

| Tool | Purpose | When to use |
|---|---|---|
| `get_diagnostics` | Real compiler errors and warnings for a file | After editing code — always call to verify correctness |
| `get_cached_diagnostics` | Previously cached diagnostics (no fresh check) | Quick check when file has not changed recently |
| `get_code_actions` | Quick fixes, refactorings, source actions at a position | Fix diagnostics automatically, add missing imports |

### Refactoring

| Tool | Purpose | When to use |
|---|---|---|
| `rename_symbol` | Workspace-wide rename with full reference tracking | Always prefer over manual find-and-replace |
| `format_document` | Apply language-specific formatting rules | After editing, before committing |

### Call Hierarchy

| Tool | Purpose | When to use |
|---|---|---|
| `prepare_call_hierarchy` | Get callable items at a position | First step before incoming/outgoing calls |
| `get_incoming_calls` | Find all callers of a function | "Who calls this?" — impact analysis |
| `get_outgoing_calls` | Find all callees of a function | "What does this call?" — dependency analysis |

### Server Monitoring

| Tool | Purpose | When to use |
|---|---|---|
| `get_server_logs` | Internal log messages from the language server | Debug "no results" issues, server startup failures |
| `get_server_messages` | User-facing messages from the language server | Check for server notifications, progress, errors |

## Workflow Patterns

### Diagnostic-Driven Editing

After editing a file:

1. Save the file to disk (diagnostics reflect the file on disk, not in-memory).
2. Call `get_diagnostics` on the changed file.
3. For each error, call `get_code_actions` to find available fixes.
4. Apply fixes or edit manually.
5. Repeat until `get_diagnostics` returns an empty list.

### Impact Analysis Before Refactoring

1. Call `get_references` on the symbol you intend to change.
2. Review all usage sites to understand the blast radius.
3. Make the change (or use `rename_symbol` for renames).
4. Call `get_diagnostics` on all affected files.

### Type Exploration

1. Call `get_hover` on an unknown symbol to see its type and documentation.
2. Call `get_definition` to read the implementation.
3. Call `get_references` to understand how other code uses it.

### Call Graph Analysis

1. Call `prepare_call_hierarchy` on a function — this returns a call hierarchy item.
2. Pass the item to `get_incoming_calls` to see what calls it (consumers).
3. Pass the item to `get_outgoing_calls` to see what it calls (dependencies).
4. Repeat recursively to trace deeper call chains.

### Workspace Navigation

1. Call `workspace_symbol_search` with a partial name to find symbols across the project.
2. Call `get_definition` on the result to jump to the source.
3. Call `get_document_symbols` on the target file to understand its full structure.

## Configuration

mcpls auto-detects language servers based on project markers:

| Language | Server | Markers |
|---|---|---|
| Rust | rust-analyzer | `Cargo.toml`, `rust-toolchain.toml` |
| Python | pyright | `pyproject.toml`, `setup.py`, `requirements.txt` |
| TypeScript | typescript-language-server | `package.json`, `tsconfig.json` |
| Go | gopls | `go.mod`, `go.sum` |
| C/C++ | clangd | `CMakeLists.txt`, `compile_commands.json`, `Makefile` |
| Zig | zls | `build.zig`, `build.zig.zon` |

Custom servers can be added in `~/.config/mcpls/mcpls.toml`:

```toml
[[lsp_servers]]
language_id = "rust"
command = "rust-analyzer"
args = []
file_patterns = ["*.rs"]
timeout_seconds = 30

[lsp_servers.initialization_options]
check.command = "clippy"
```

Environment variables:

| Variable | Description | Default |
|---|---|---|
| `MCPLS_CONFIG` | Path to config file | `~/.config/mcpls/mcpls.toml` |
| `MCPLS_LOG` | Log level (trace/debug/info/warn/error) | `info` |
| `MCPLS_LOG_JSON` | Output logs as JSON | `false` |

## Troubleshooting

### No results from get_hover or get_definition

1. The file may not be indexed yet. mcpls opens files lazily — the first access to a file
   triggers indexing, which may take a few seconds for large projects.
2. Call `get_server_logs` to check if the language server is running and has finished indexing.
3. Verify the language server is installed and in `$PATH` (e.g., `which rust-analyzer`).
4. Check that the project has the expected marker files (e.g., `Cargo.toml` for Rust).

### get_diagnostics returns stale results

- Diagnostics reflect the file **on disk**. If you edited the file but did not save, the
  diagnostics will be for the old content. Always save before calling `get_diagnostics`.
- After a save, the language server needs time to re-analyze. Wait briefly and retry.
- Use `get_cached_diagnostics` only when you know the file has not changed — it returns
  push-based diagnostics from the server's last notification, which may be outdated.

### Language server not starting

1. Check `get_server_logs` for startup errors.
2. Verify the server binary is installed: `which rust-analyzer`, `which pyright`, etc.
3. Check the config file (`~/.config/mcpls/mcpls.toml`) for misconfigured `command` or `args`.
4. Ensure the project root contains the expected marker files for auto-detection.
5. For non-standard setups, add an explicit `[[lsp_servers]]` entry in the config.

### Slow responses

- Initial indexing can take 10-30 seconds for large Rust workspaces. Subsequent calls are fast.
- Set `timeout_seconds` higher in the config for large projects.
- Check `get_server_logs` for memory or CPU warnings from the language server.
- For rust-analyzer, ensure `rust-analyzer.cargo.buildScripts.enable` is not causing excessive
  build script evaluation.

### rename_symbol fails or misses references

- `rename_symbol` only works on symbols the language server can resolve. If a symbol is in a
  macro expansion or generated code, the rename may be partial.
- Always call `get_references` first to verify the server can see all usage sites.
- After rename, call `get_diagnostics` on affected files to catch any breakage.

## Edge Cases

- **Macro-generated code**: `get_hover` and `get_definition` may not resolve symbols inside
  procedural macro expansions. Use `get_references` on the macro invocation site instead.
- **Conditional compilation**: Symbols behind `#[cfg(...)]` may not be visible depending on the
  active feature set. Configure the language server's feature flags accordingly.
- **Multi-root workspaces**: mcpls supports multiple roots via the `roots` config option. Each
  root gets its own language server instance.
- **Large files**: `get_document_symbols` on very large files (10k+ lines) may be slow. Prefer
  targeted `get_hover` or `workspace_symbol_search` instead.
- **Cross-crate navigation**: `get_definition` may jump into dependency source code
  (e.g., `~/.cargo/registry/`). This is expected behavior — the language server resolves through
  the full dependency graph.
