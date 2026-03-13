# Code Intelligence

Zeph provides out-of-the-box code intelligence for any project you work in — without plugins, language servers, or manual configuration. It combines three complementary layers into a unified `search_code` tool that the agent calls automatically when it needs to understand your codebase.

## The Problem with Context Windows

When an agent needs to understand a large codebase, it faces a fundamental constraint: it cannot read every file. A grep-based approach works for small projects or large context windows, but becomes expensive at scale — each grep cycle consumes tokens, and an 8K-context local model might exhaust its budget after 3–4 searches.

Zeph's code intelligence pre-indexes your project and retrieves the most relevant code for each query, so the agent spends its context budget on reasoning rather than searching.

## Three Layers, One Tool

The `search_code` tool unifies three search strategies:

### Structural Search (tree-sitter)

Tree-sitter parses your source files into an AST and extracts named symbols — functions, structs, classes, impl blocks — with accurate visibility annotations and line numbers. Structural search is fast, offline, and works for all supported languages without any external services.

Use structural search when you need exact definitions: "where is `AuthMiddleware` defined?"

### Semantic Search (Qdrant)

When your question is conceptual rather than syntactic — "how does the authentication flow work?" — semantic search finds relevant code by meaning, not keyword. Each source chunk is embedded into a vector and stored in Qdrant. At query time, the question is embedded and the closest chunks are retrieved.

Semantic search requires a running Qdrant instance and an active code index. Enable it once and Zeph keeps the index up to date as you edit files.

### LSP Integration

For precise cross-reference questions — "what calls this function?", "go to definition" — Zeph delegates to the language server via the `mcpls` MCP tool. LSP answers are authoritative because they come from the same compiler-backed analysis used by IDEs.

LSP integration requires `mcpls` to be configured under `[[mcp.servers]]`.

## How the Agent Uses It

The agent calls `search_code` with a natural-language query. Zeph runs all available layers in parallel, deduplicates results, and returns a ranked list with file paths, line numbers, and relevance scores:

```
> find where API keys are validated

[structural] src/vault/mod.rs:34  pub fn validate_key
[semantic]   src/vault/mod.rs:34–67  (score: 0.94)
[semantic]   src/auth/middleware.rs:12–45  (score: 0.81)
[lsp]        3 references to `validate_key`
```

The agent uses these results to read specific files rather than scanning the entire codebase.

## Repo Map

Alongside per-query retrieval, Zeph maintains a compact structural map of the project — a list of every public symbol with its file and line number. The repo map is injected into the system prompt and cached (default: 5 minutes). It gives the model a bird's-eye view of the codebase without consuming significant context.

The repo map is generated via tree-sitter queries and works for all providers, including Claude and OpenAI. It does not require Qdrant.

Example:

```text
<repo_map>
  src/agent.rs :: pub struct Agent (line 12), pub fn new (line 45), pub fn run (line 78)
  src/config.rs :: pub struct Config (line 5), pub fn load (line 30)
  src/vault/mod.rs :: pub fn validate_key (line 34), pub fn get_secret (line 68)
  ... and 14 more files
</repo_map>
```

## Setup

### Structural search and repo map (always available)

No setup required. Tree-sitter grammars are compiled into every Zeph build. The repo map is enabled by default with a 1024-token budget.

```toml
[index]
repo_map_budget = 1024    # tokens; set to 0 to disable
repo_map_ttl_secs = 300   # cache TTL
```

### Semantic search (requires Qdrant)

1. Start Qdrant:

   ```bash
   docker compose up -d qdrant
   ```

2. Enable indexing:

   ```toml
   [index]
   enabled = true
   auto_index = true    # re-index on startup and on file changes
   ```

3. On first run, Zeph indexes the project automatically. Subsequent runs only re-embed changed files.

### LSP integration (requires mcpls)

Configure `mcpls` as an MCP server in your config or via `zeph init`:

```toml
[[mcp.servers]]
name = "mcpls"
command = "mcpls"
args = ["--config", ".zeph/mcpls.toml"]
```

Run `zeph init` to have the wizard generate the correct mcpls config for your project.

## Supported Languages

| Language | Structural | Semantic | LSP |
|----------|-----------|----------|-----|
| Rust | yes | yes | yes (rust-analyzer) |
| Python | yes | yes | yes (pylsp, pyright) |
| JavaScript | yes | yes | yes (typescript-language-server) |
| TypeScript | yes | yes | yes (typescript-language-server) |
| Go | yes | yes | yes (gopls) |
| Bash, TOML, JSON, Markdown | yes (file-level) | yes | no |

## Related

- [Code Indexing](../advanced/code-indexing.md) — full configuration reference, chunking algorithm, retrieval tuning
- [LSP Context Injection](lsp-context-injection.md) — automatic diagnostic and hover injection on file read/write
- [Tools](tools.md#code-search) — how `search_code` fits into the tool catalog
- [Feature Flags](../reference/feature-flags.md#zeph-index-language-features) — tree-sitter grammar sub-features
