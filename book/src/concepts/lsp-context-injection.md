# LSP Context Injection

> **Feature flag:** `lsp-context` (included in `--features full`)

LSP Context Injection automatically adds compiler-derived information to the agent's context after
certain tool calls — without the LLM needing to issue explicit tool requests.

## What It Does

Three hooks fire automatically during a conversation:

| Hook | Trigger | What gets injected |
|------|---------|-------------------|
| **Diagnostics** | After `write_file` | Compiler errors and warnings for the saved file |
| **Hover** *(opt-in)* | After `read_file` | Type signatures for key symbols in the file |
| **References** | Before `rename_symbol` | All call sites of the symbol being renamed |

The injected data appears as a `[lsp ...]` prefixed message in the conversation history — the same
pattern used by semantic recall and graph facts. A per-turn `token_budget` cap prevents runaway
context growth.

## Why It Matters

Without this feature, the agent has to explicitly call `get_diagnostics`, `get_hover`, or
`get_references` after every file operation. With LSP Context Injection enabled, the feedback loop
is automatic:

1. Agent writes a file.
2. Zeph fetches diagnostics from the language server.
3. Errors appear as the next turn's context — the agent fixes them immediately.

No extra round-trips. No "check for errors" prompt needed.

## Prerequisites

- mcpls configured as an MCP server (see [LSP Code Intelligence](../guides/lsp.md))
- `lsp-context` feature enabled (already included in the `full` feature set)

## Enabling

```bash
# For a single session
zeph --lsp-context

# Or set permanently in config.toml
```

```toml
[agent.lsp]
enabled = true
```

The interactive wizard (`zeph --init`) prompts for this setting after the mcpls step.

## Graceful Degradation

When mcpls is unavailable, all hooks silently skip. The agent continues working normally — no errors
are shown, no functionality is lost. Individual failures are logged at `debug` level only.

## Configuration and Details

Full configuration reference, token budget tuning, and TUI status command:
[LSP Context Injection → guides/lsp.md](../guides/lsp.md#lsp-context-injection)

For IDE-proxied LSP via ACP (Zed, Helix, VS Code):
[ACP LSP Extension → guides/lsp.md](../guides/lsp.md#acp-lsp-extension)
