# ShadowSentinel: AI Safety Probing

ShadowSentinel is a safety capability governance system that performs pre-execution LLM-based probes on high-risk tool categories before they run. It maintains a persistent audit trail of all safety events across sessions.

**Phase 2** adds the `SafetyProbe` trait and `ShadowProbeExecutor`, enabling real-time safety classification with confidence scoring and bounded latency.

## How It Works

Before executing a tool, ShadowSentinel asks the LLM: "Is this tool call safe to execute?" For high-risk tool categories (shell commands, file writes, exfil-capable MCP tools), the system:

1. Extracts tool metadata (name, arguments, category)
2. Sends a structured probe request to the configured LLM (`probe_provider`, or fallback to main provider)
3. Receives a confidence score (0.0–1.0) and reasoning
4. Logs the event to the persistent `safety_shadow_events` table
5. Allows execution if confidence > threshold (default: 0.5), or fails open if the probe times out

The probe is **transparent** — it runs before policy gates and doesn't block concurrent tool execution.

## Configuration

Enable ShadowSentinel in your `config.toml`:

```toml
[security.shadow_sentinel]
enabled = true                  # Enable safety probing (default: false)
probe_provider = "fast"         # Optional: provider name for probing (empty = main provider)
confidence_threshold = 0.5      # Minimum confidence for approval (0.0–1.0, default: 0.5)
max_probes_per_turn = 10        # Rate limit: max probes per agent turn (default: 10)
probe_timeout_ms = 5000         # Max time for one probe (default: 5000 ms)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | false | Enable ShadowSentinel (default: false) |
| `probe_provider` | string | "" | Provider name for probes; empty uses main provider |
| `confidence_threshold` | float | 0.5 | Approval threshold (0.0–1.0); higher = stricter |
| `max_probes_per_turn` | int | 10 | Rate limit per agent turn to prevent probe spam |
| `probe_timeout_ms` | int | 5000 | Max milliseconds per probe before timeout |

### Choosing a Probe Provider

Probes need to be fast and inexpensive — longer latency delays tool execution. Use a fast, cheap model:

```toml
[[llm.providers]]
name = "fast"
type = "openai"
model = "gpt-4o-mini"

[security.shadow_sentinel]
probe_provider = "fast"         # Use the cheap model for quick safety checks
```

When `probe_provider` is empty, ShadowSentinel falls back to the agent's main provider.

## Probed Tool Categories

ShadowSentinel probes are issued before executing these high-risk tool types:

| Category | Examples | Risk Profile |
|----------|----------|--------------|
| **Shell** | `shell`, `bash`, `sh` | Arbitrary OS command execution |
| **File Write** | `write_file`, `edit_file` | Persistent state changes |
| **Exfil-capable MCP** | `brave_search`, `web_scrape` | Network access, data exfiltration |

Tools in other categories (code execution, math, read-only file access) skip probing.

## Safety Events Table

Every probe is logged to the `safety_shadow_events` SQLite table (created by migration 085) for audit and analysis:

```
CREATE TABLE safety_shadow_events (
  id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,           -- Agent session ID
  timestamp TEXT NOT NULL,            -- Event timestamp (ISO 8601)
  tool_name TEXT NOT NULL,            -- Tool being probed (e.g., "shell")
  tool_args_preview TEXT,             -- First 512 chars of tool args
  probe_request_tokens INTEGER,       -- LLM input tokens
  probe_response_tokens INTEGER,      -- LLM output tokens
  confidence REAL,                    -- Safety confidence (0.0-1.0)
  reasoning TEXT,                     -- LLM's explanation
  result TEXT,                        -- 'approved', 'rejected', 'timeout'
  execution_allowed BOOLEAN           -- Whether execution proceeded
);
```

This allows you to:
- Audit all safety decisions across all sessions
- Analyze probe latency and cost
- Review LLM reasoning for rejected calls
- Detect patterns in tool usage and safety concerns

### Querying Safety Events

```bash
# Count safety events by tool
sqlite3 ~/.zeph/zeph.db "SELECT tool_name, COUNT(*) FROM safety_shadow_events GROUP BY tool_name;"

# Find rejected probes
sqlite3 ~/.zeph/zeph.db "SELECT tool_name, confidence, reasoning FROM safety_shadow_events WHERE result = 'rejected';"

# Recent safety events
sqlite3 ~/.zeph/zeph.db "SELECT timestamp, tool_name, confidence, result FROM safety_shadow_events ORDER BY timestamp DESC LIMIT 20;"
```

## Probe Behavior

### Success (Confident Approval)

If the probe LLM returns confidence >= `confidence_threshold`, the tool executes immediately. The event is logged with `result = 'approved'` and `execution_allowed = true`.

### Timeout

If the probe takes longer than `probe_timeout_ms`, ShadowSentinel fails open: the tool executes and the event is logged with `result = 'timeout'` and `execution_allowed = true`. This prevents slow probes from blocking operations.

### Rate Limiting

At most `max_probes_per_turn` probes are issued per agent turn. If the limit is reached, subsequent tools skip probing for that turn. This prevents probe spam when many tools are called in a single step.

### Rejection (Low Confidence)

When confidence < `confidence_threshold`, the tool **does not execute**. The event is logged with `result = 'rejected'` and `execution_allowed = false`. The agent receives a `ToolError::SafetyCheckFailed` result with the probe reasoning.

The agent can acknowledge the safety concern and retry, or choose a different approach.

## Multi-Provider Safety (Optional)

For extra safety, probe with a different provider than the main inference engine:

```toml
[[llm.providers]]
name = "main"
type = "openai"
model = "gpt-4-turbo"

[[llm.providers]]
name = "safety-check"
type = "anthropic"
model = "claude-opus-4"

[security.shadow_sentinel]
probe_provider = "safety-check"     # Use Anthropic for safety, OpenAI for main inference
```

This creates an independent safety review layer using a different model/provider, reducing the chance of both falling into the same blind spots.

## Disabling Probes for Specific Tools

There is no per-tool override for probing. If you trust certain tools completely and want to skip probing:

1. **Recommendation:** Keep probing enabled at the category level. The cost is low and the safety benefit is high.
2. **Alternative:** Disable ShadowSentinel entirely and rely on policy gates and permission checks.

## Cost Considerations

Each probe:
- Costs ~100 tokens prompt + ~50 tokens response (varies by tool complexity)
- At $0.0001 per 1K tokens (typical cheap models), costs ~0.015¢ per probe
- With `max_probes_per_turn = 10`, max cost per turn is ~0.15¢

For most workloads, probe overhead is negligible compared to main LLM inference.

## See Also

- [Skill Trust & Security](../../advanced/skill-trust.md) — Policy enforcement and permission models
- [File Read Sandbox](./file-sandbox.md) — Sandboxed file access restrictions
- [MCP Security](./mcp.md) — MCP server vetting and privilege isolation
