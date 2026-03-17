# LSP Code Intelligence

Zeph can use Language Server Protocol (LSP) servers — rust-analyzer, pyright, gopls, and others — for
compiler-level code understanding. The integration is provided by **mcpls**, an MCP-to-LSP bridge
that exposes 16 LSP capabilities as standard MCP tools.

No changes to Zeph itself are required. Enabling LSP intelligence is purely a configuration step.

## What You Get

- **Type information**: ask "what type is this variable?" and get the compiler's answer, not a guess.
- **Definition navigation**: jump to the source of any function, type, or trait.
- **Reference analysis**: find every usage of a symbol before renaming or deleting it.
- **Diagnostics**: get compiler errors and warnings for any file on demand.
- **Call hierarchy**: trace data flow up and down the call graph.
- **Symbol search**: find any symbol across the entire workspace by name.
- **Code actions**: apply quick fixes and refactorings suggested by the language server.
- **Safe rename**: rename a symbol across all files in one step.

## Prerequisites

- Zeph with MCP support (always-on since v0.13)
- `mcpls` binary:

  ```bash
  cargo install mcpls
  ```

- At least one language server for your project:

  | Language | Language Server | Install |
  |----------|----------------|---------|
  | Rust | rust-analyzer | `rustup component add rust-analyzer` |
  | Python | pyright | `pip install pyright` or `npm install -g pyright` |
  | TypeScript | typescript-language-server | `npm install -g typescript-language-server` |
  | Go | gopls | `go install golang.org/x/tools/gopls@latest` |

## Quick Start

Run `zeph --init` and answer **Yes** when asked:

```
== MCP: LSP Code Intelligence ==

mcpls detected.
Enable LSP code intelligence via mcpls? (Y/n)
```

Alternatively, add the configuration manually (see [Configuration](#configuration) below).

## Verify the Setup

Start Zeph and ask a question that triggers LSP:

```
You: What type does the `build_config` function return in src/init.rs?
```

The agent will call `get_hover` and return the compiler's type signature. If you see a meaningful
type instead of an error, mcpls is working.

## Configuration

The wizard generates the following block in `config.toml`:

```toml
[[mcp.servers]]
id = "mcpls"
command = "mcpls"
args = ["--workspace-root", "."]
# LSP servers need warmup time. The default MCP timeout is 30s; 60s is recommended for mcpls.
timeout = 60
```

For a workspace with multiple roots (e.g. a monorepo):

```toml
[[mcp.servers]]
id = "mcpls"
command = "mcpls"
args = [
    "--workspace-root", "./backend",
    "--workspace-root", "./frontend",
]
timeout = 60
```

### Advanced: mcpls.toml

For multi-language projects or to pin specific language servers, create `mcpls.toml` in your
workspace root. mcpls auto-detects language servers from project files (`Cargo.toml`,
`pyproject.toml`, `tsconfig.json`, `go.mod`) when no `mcpls.toml` is present.

**Rust project:**

```toml
[servers.rust-analyzer]
command = "rust-analyzer"
languages = ["rust"]
```

**Python project:**

```toml
[servers.pyright]
command = "pyright-langserver"
args = ["--stdio"]
languages = ["python"]
```

**TypeScript project:**

```toml
[servers.typescript]
command = "typescript-language-server"
args = ["--stdio"]
languages = ["typescript", "javascript"]
```

**Go project:**

```toml
[servers.gopls]
command = "gopls"
languages = ["go"]
```

**Multi-language project:**

```toml
[servers.rust-analyzer]
command = "rust-analyzer"
languages = ["rust"]

[servers.pyright]
command = "pyright-langserver"
args = ["--stdio"]
languages = ["python"]
```

## Available Tools

mcpls exposes the following MCP tools. Zeph selects the appropriate tool based on context.

### Core (P0 — use these daily)

| Tool | Description |
|------|-------------|
| `get_hover` | Type signature, documentation, and inferred type for a symbol at a position |
| `get_definition` | Location where a symbol is defined |
| `get_references` | All usages of a symbol across the workspace |
| `get_diagnostics` | Compiler errors and warnings for a file |

### Navigation (P1)

| Tool | Description |
|------|-------------|
| `get_document_symbols` | All symbols defined in a file (functions, types, constants) |
| `workspace_symbol_search` | Search for symbols by name across the entire workspace |
| `prepare_call_hierarchy` | Prepare a symbol for call hierarchy queries |
| `incoming_calls` | Functions that call the given symbol |
| `outgoing_calls` | Functions called by the given symbol |
| `get_code_actions` | Quick fixes and refactorings available at a position |

### Editing (P2)

| Tool | Description |
|------|-------------|
| `rename_symbol` | Rename a symbol across all files |
| `format_document` | Format a file according to language rules |
| `get_completions` | Completion candidates at a position |

### Diagnostics & Debug

| Tool | Description |
|------|-------------|
| `get_cached_diagnostics` | Previously cached diagnostics (faster, may be stale) |
| `server_logs` | Raw log output from the language server |
| `server_messages` | Raw LSP messages exchanged with the language server |

## Usage Patterns

### Diagnostic-Driven Workflow

After editing a file, verify correctness:

1. Edit the file with the `shell` tool.
2. Call `get_diagnostics` on the changed file.
3. For each error, call `get_code_actions` to see available fixes.
4. Apply fixes or edit manually.
5. Repeat until `get_diagnostics` returns no errors.

### Impact Analysis Before Refactoring

1. Call `get_references` on the symbol to change.
2. Review all usage sites.
3. Make changes.
4. Call `get_diagnostics` on all affected files.

### Type Exploration

1. Call `get_hover` on an unknown symbol to see its type and docs.
2. Call `get_definition` to read the implementation.
3. Call `get_references` to understand usage patterns.

### Call Graph Analysis

1. Call `prepare_call_hierarchy` on a function.
2. Call `incoming_calls` to see what calls it (data consumers).
3. Call `outgoing_calls` to see what it calls (dependencies).

## Troubleshooting

**"Server not starting" or no results:**

Check the language server logs:

```
Ask: Show me the mcpls server logs.
```

The agent will call `server_logs` and display the raw output. Common causes:
- Language server not installed or not in PATH.
- Wrong working directory — confirm `--workspace-root` matches your project root.

**"Stale diagnostics after editing a file":**

mcpls does not forward `textDocument/didChange` notifications to the LSP server. Diagnostics
reflect the state of the file on disk. After editing, save the file before calling
`get_diagnostics`.

**"Timeout errors":**

The default `timeout = 60` should be enough for most language servers. If rust-analyzer or another
slow server times out on first use (it performs initial indexing), increase the timeout:

```toml
[[mcp.servers]]
id = "mcpls"
command = "mcpls"
args = ["--workspace-root", "."]
timeout = 120
```

**"No results for hover or definition":**

mcpls opens files lazily. The first access to a file may be slower. If results are consistently
empty, verify that the language server is installed and that `mcpls.toml` (if present) has the
correct `languages` mapping for your file type.

## LSP Context Injection

> [!NOTE]
> Requires the `lsp-context` feature flag (included in `--features full`).

Zeph can automatically inject LSP-derived data into the agent's context without the LLM needing to
make explicit tool calls. Three hooks are provided:

- **Diagnostics on save** — after every `write_file` tool call, Zeph fetches diagnostics from the
  LSP server and injects errors directly into the next LLM turn. The agent sees compiler errors
  immediately and can fix them without manual intervention.
- **Hover on read** *(opt-in)* — after `read_file`, Zeph pre-fetches hover information for key
  symbol definitions in the file and injects it as annotations. Disabled by default.
- **References on rename** — before `rename_symbol`, Zeph fetches all reference locations and
  presents them to the LLM for review.

### Enabling

```bash
# CLI flag — enable for this session
zeph --lsp-context

# Config file — enable permanently
```

```toml
[agent.lsp]
enabled = true
```

The wizard (`zeph --init`) prompts for this setting after the mcpls step. It is skipped
automatically when mcpls is not configured.

### Configuration

```toml
[agent.lsp]
enabled = true
mcp_server_id = "mcpls"   # MCP server that provides LSP tools (default: "mcpls")
token_budget = 2000        # Max tokens to spend on injected LSP context per turn

[agent.lsp.diagnostics]
enabled = true             # Inject diagnostics after write_file (default: true when [agent.lsp] is enabled)
max_per_file = 20          # Max diagnostics per file
max_files = 5              # Max files per injection batch
min_severity = "error"     # Minimum severity: "error", "warning", "info", or "hint"

[agent.lsp.hover]
enabled = false            # Pre-fetch hover info on read_file (default: false — opt-in)
max_symbols = 10           # Max symbols to fetch hover for per file

[agent.lsp.references]
enabled = true             # Inject reference list before rename_symbol (default: true)
max_refs = 50              # Max references to show per symbol
```

### How Injection Works

LSP notes are injected into the message history (not the system prompt) as a `[lsp ...]` prefixed
user message, following the same pattern used by semantic recall, graph facts, and code context:

```
[lsp diagnostics]
src/main.rs:42:5 error[E0308]: mismatched types — expected `u32`, found `String`
src/main.rs:55:1 error[E0599]: no method named `foo` found for struct `Bar`
```

Notes exceeding `token_budget` are dropped with a truncation marker. The budget resets each turn.

### Graceful Degradation

LSP context injection is fully optional. When the configured MCP server is unavailable:

- Hooks silently skip — the agent continues working normally
- No error is logged or shown to the user
- Individual tool call failures are logged at `debug` level only

This means the agent works correctly whether or not mcpls is installed or running.

### TUI: `/lsp` Command

In TUI mode, type `/lsp` to show LSP context injection status:

- Whether hooks are active and the configured MCP server is connected
- Count of diagnostics, hover entries, and references injected this session
- Token budget usage for the current turn

### Requirements

The `lsp-context` feature requires the `mcp` feature (always-on since v0.13) and a configured
mcpls MCP server. See the [Configuration](#configuration) section above for mcpls setup.

## ACP LSP Extension

> Requires the `acp` feature flag (included in `--features full`).

When Zeph runs as an ACP server (connected to an IDE like Zed, Helix, or VS Code), the IDE can
expose its own LSP capabilities directly to the agent. This is the third and most integrated path
to LSP intelligence: instead of running a separate mcpls process, the agent sends LSP requests
back to the IDE through the ACP connection.

### How It Works

During the ACP `initialize` handshake, the IDE can advertise LSP support by including
`"lsp": true` in its `meta` capabilities. When Zeph sees this flag, it creates an `AcpLspProvider`
that sends `ext_method` requests back to the IDE for LSP operations.

The agent can also fall back to an `McpLspProvider` (mcpls) when the IDE does not advertise LSP
support but mcpls is configured as an MCP server. Priority order:

1. **ACP provider** (IDE-proxied) — used when the IDE advertises `meta["lsp"]`
2. **MCP provider** (mcpls) — used when mcpls is configured under `[[mcp.servers]]`

### Supported Methods

The ACP LSP extension exposes seven methods via `ext_method`:

| Method | Description |
|--------|-------------|
| `lsp/hover` | Type signature and documentation at a position |
| `lsp/definition` | Jump-to-definition locations |
| `lsp/references` | All usages of a symbol across the workspace |
| `lsp/diagnostics` | Compiler errors and warnings for a file |
| `lsp/documentSymbols` | All symbols defined in a file |
| `lsp/workspaceSymbol` | Search symbols by name across the workspace |
| `lsp/codeActions` | Quick fixes and refactorings at a position or range |

### Push Notifications

The IDE can also push data to the agent via `ext_notification`:

| Notification | Description |
|--------------|-------------|
| `lsp/publishDiagnostics` | Push diagnostics for a file (cached in a bounded LRU cache) |
| `lsp/didSave` | Notify the agent that a file was saved; triggers automatic diagnostics fetch when `auto_diagnostics_on_save` is enabled |

Pushed diagnostics are stored in a bounded `DiagnosticsCache` with LRU eviction. The cache size
is controlled by `max_diagnostic_files` (default: 5).

### Configuration

```toml
[acp.lsp]
enabled = true                     # Enable LSP extension when IDE supports it (default: true)
auto_diagnostics_on_save = true    # Fetch diagnostics on lsp/didSave notification (default: true)
max_diagnostics_per_file = 20      # Max diagnostics accepted per file (default: 20)
max_diagnostic_files = 5           # Max files in DiagnosticsCache, LRU eviction (default: 5)
max_references = 100               # Max reference locations returned (default: 100)
max_workspace_symbols = 50         # Max workspace symbol search results (default: 50)
request_timeout_secs = 10          # Timeout for LSP ext_method calls in seconds (default: 10)
```

See [Configuration Reference](../reference/configuration.md) for the full `[acp.lsp]` section.

### Capability Negotiation

The LSP extension is negotiated per-session. The flow is:

1. IDE sends `initialize` with `meta: { "lsp": true }` in client capabilities.
2. Zeph responds with the list of supported LSP methods in its server capabilities.
3. The IDE can now receive `ext_method` calls for the advertised LSP methods.
4. The IDE can send `ext_notification` for `lsp/publishDiagnostics` and `lsp/didSave`.

If the IDE does not include `"lsp": true`, the ACP LSP provider is marked as unavailable and
Zeph falls back to the MCP provider (mcpls) if configured.

### Coordinates

All positions use **1-based** line and character coordinates (ACP/MCP convention). The IDE is
responsible for converting between 1-based (ACP) and 0-based (LSP) coordinates.

## Limitations

- **No live file sync**: mcpls does not support `textDocument/didChange`. Edits are invisible to
  the LSP server until the file is saved and mcpls reopens it. Always save before querying.
- **No file watcher**: `workspace/didChangeWatchedFiles` is not implemented. Adding new files
  requires restarting mcpls.
- **Pull-based diagnostics**: diagnostics are fetched on demand, not pushed proactively. Use
  `get_cached_diagnostics` for fast repeated checks. When `lsp-context` injection is enabled,
  diagnostics are fetched automatically after `write_file` with a short delay for LSP re-analysis.
  When using the ACP LSP extension with `auto_diagnostics_on_save`, diagnostics are fetched
  automatically on `lsp/didSave` notifications from the IDE.
- **Stale diagnostics on first fetch**: After a file write, there is a 200ms delay before
  fetching to allow the language server to begin re-analysis. Diagnostics may still reflect the
  previous file state if the server is slow.
- **Untrusted code**: LSP server output (diagnostics, hover text, `server_logs`) may contain
  content from the source files being analyzed. If analyzing untrusted code (e.g., cloned
  repositories), adversarial content in comments or string literals could appear in the LLM
  context. Zeph's content sanitizer automatically wraps this output for isolation.
- **ACP LSP is `!Send`**: The `AcpLspProvider` holds `Rc<RefCell<...>>` state and must run inside
  a `tokio::task::LocalSet`. HTTP transport sessions requiring `Send` are not yet supported.
