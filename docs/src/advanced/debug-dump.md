# Debug Dump

Debug dump writes every LLM request, response, and raw tool output to numbered files on disk. Use it when you need to inspect exactly what context is sent to the model, what comes back, and what tool results look like before any truncation or summarization.

## Enabling

Three ways to activate debug dump:

**CLI flag (one session):**

```bash
zeph --debug-dump                     # use output_dir from config (default: .local/debug)
zeph --debug-dump /tmp/my-debug       # write to a custom path
```

**Config file (persistent):**

```toml
[debug]
enabled = true
output_dir = ".local/debug"           # relative to cwd, or absolute path
```

**Slash command (mid-session):**

```
/debug-dump                           # enable using configured output_dir
/debug-dump /tmp/my-debug             # enable with a custom path
```

The slash command is useful when you notice unexpected output and want to capture subsequent turns without restarting. Dump files accumulate from that point forward.

## File Layout

Each session creates a timestamped subdirectory under the output directory:

```
.local/debug/
└── 1748992800/          ← Unix timestamp at session start
    ├── 0000-request.json
    ├── 0000-response.txt
    ├── 0001-tool-shell.txt
    ├── 0002-request.json
    ├── 0002-response.txt
    └── …
```

Files are numbered sequentially with a shared counter. Request/response pairs share the same ID prefix so they can be correlated. Tool output files use `{id:04}-tool-{name}.txt` where `name` is the tool name with non-alphanumeric characters replaced by `_`.

| File pattern | Contents |
|---|---|
| `{id}-request.json` | JSON array of messages sent to the LLM (full context) |
| `{id}-response.txt` | Raw text returned by the LLM |
| `{id}-tool-{name}.txt` | Raw tool output before summarization or truncation |

## What Gets Captured

- **LLM requests** — the full `messages` array including all system blocks, tool results, and history. Useful for identifying what "garbage" is accumulating in context.
- **LLM responses** — the complete raw text returned by the model, including thinking blocks if extended thinking is enabled.
- **Tool output** — the unprocessed output string before `maybe_summarize_tool_output` runs. This lets you compare what the tool actually returned vs. what the model saw.

Both the streaming and non-streaming LLM code paths are instrumented. Tool output is captured for every tool execution regardless of whether summarization is configured.

## Configuration

```toml
[debug]
enabled = false             # Enable at startup (default: false)
output_dir = ".local/debug" # Base directory for dump files (default: ".local/debug")
```

The `--debug-dump` CLI flag overrides both fields: if `PATH` is provided it overrides `output_dir`; if omitted, `output_dir` is used. If neither the flag nor `enabled = true` is set, no files are written.

> **Note:** Debug dump does not affect the agent loop, context, or LLM calls — it is purely additive. There is no performance overhead beyond the file writes themselves.

## Security

Dump files contain the full conversation context including any secrets, tokens, or sensitive data present in messages and tool output. Do not store dump directories in version-controlled or publicly accessible locations.

Add `.local/` to `.gitignore` (this is the default) to keep dumps out of your repository.

## See Also

- [CLI Reference — `--debug-dump`](../reference/cli.md#global-options)
- [Configuration Reference — `[debug]`](../reference/configuration.md)
- [Context Engineering](context.md) — understanding how context is assembled
