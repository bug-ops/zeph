---
name: code-analysis
description: Use LSP tools (hover, definitions, references, diagnostics) for compiler-level code understanding.
compatibility: Requires mcpls MCP server
---
# Code Analysis with LSP

Use LSP tools for accurate, compiler-verified code understanding. These tools require the `mcpls`
MCP server to be configured.

Positions are **1-based**: line 1, column 1 is the first character. If you read a file and see
line numbers in the output, use those directly — no conversion needed (mcpls does not use
0-based LSP positions in its tool interface).

## When to Use Each Tool

### Understanding Code

- **`get_hover`** — Get the type signature, inferred type, and documentation for a symbol at a
  specific file position. Use when asked "what type is X?" or "what does this function do?".
- **`get_definition`** — Navigate to where a symbol is defined. Use when you need to read the
  implementation of a function, type, or trait before reasoning about it.
- **`get_references`** — Find all usages of a symbol across the workspace. Always call this before
  renaming or deleting a symbol to understand the full impact.

### Navigating Structure

- **`get_document_symbols`** — List all symbols defined in a file (functions, types, constants,
  fields). Use to understand a file's structure without reading every line.
- **`workspace_symbol_search`** — Search for a symbol by name across the entire workspace. Use
  when you know a name but not which file defines it.
- **`prepare_call_hierarchy`** → **`incoming_calls`** / **`outgoing_calls`** — Trace call chains.
  Use for data flow analysis or to understand the impact of changing a function's signature.

### Checking Correctness

- **`get_diagnostics`** — Get compiler errors and warnings for a file. Always call this after
  editing code to verify correctness. Results reflect the file on disk — save before calling.
- **`get_cached_diagnostics`** — Return previously cached diagnostics without triggering a fresh
  check. Faster, but may be stale if the file changed recently.

### Modifying Code

- **`get_code_actions`** — Get quick fixes and refactorings available at a position (e.g., "add
  missing import", "convert to async"). Use to automatically fix diagnostics.
- **`rename_symbol`** — Rename a symbol across all files in the workspace. Always prefer this
  over manual find-and-replace.
- **`format_document`** — Auto-format a file according to the language's formatting rules.

### Diagnostics and Debug

- **`server_logs`** — Raw log output from the language server. Use to debug why LSP tools return
  no results.
- **`server_messages`** — Raw LSP protocol messages. Use for deep debugging of server behavior.

## Workflow Patterns

### Diagnostic-Driven Workflow

After editing a file:

1. Call `get_diagnostics` on the changed file.
2. For each error, call `get_code_actions` to find available fixes.
3. Apply fixes or edit manually.
4. Repeat until `get_diagnostics` returns an empty list.

### Impact Analysis Before Refactoring

1. Call `get_references` on the symbol you intend to change.
2. Review all usage sites to understand the blast radius.
3. Make the change.
4. Call `get_diagnostics` on all affected files.

### Type Exploration

1. Call `get_hover` on an unknown symbol to see its type and documentation.
2. Call `get_definition` to read the implementation.
3. Call `get_references` to understand how other code uses it.

### Call Graph Analysis

1. Call `prepare_call_hierarchy` on a function.
2. Call `incoming_calls` to see what calls it (consumers).
3. Call `outgoing_calls` to see what it calls (dependencies).

## Tips

- Files are opened lazily by mcpls. The first access to a file may be slightly slower.
- After editing a file externally, diagnostics may be stale. Save the file before querying.
- Use `server_logs` to diagnose "no results" issues — the language server may not have indexed
  the file yet or may not be running.
- `get_completions` is available but rarely needed — it is most useful for exploring unknown APIs.
