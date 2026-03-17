# Spec: Tool Execution

## Sources

### External
- **OWASP AI Agent Security Cheat Sheet** (2026) — shell sandbox and tool policy design: https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html
- **Policy Compiler for Secure Agentic Systems** (Feb 2026) — PolicyEnforcer, PermissionPolicy: https://arxiv.org/html/2602.16708v2

### Internal
| File | Contents |
|---|---|
| `crates/zeph-tools/src/executor.rs` | `ToolExecutor` trait, `ErasedToolExecutor`, `ToolOutput` |
| `crates/zeph-tools/src/composite.rs` | `CompositeExecutor`, chain ordering |
| `crates/zeph-tools/src/filter/mod.rs` | `FilterPipeline`, `CommandMatcher`, `OutputFilterRegistry` |
| `crates/zeph-tools/src/filter/security.rs` | `SecurityPatterns`, 17 regex patterns |
| `crates/zeph-tools/src/filter/declarative.rs` | Per-filter TOML config |

---

`crates/zeph-tools/` — tool registry, executors, audit, filtering.

## ToolExecutor Trait

```rust
trait ToolExecutor: Send + Sync {
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError>;
    async fn execute_tool_call_confirmed(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError>;
    fn tool_definitions(&self) -> Vec<ToolDef>;
    fn set_skill_env(&self, env: Option<HashMap<String, String>>);
    fn set_effective_trust(&self, level: TrustLevel);
    fn is_tool_retryable(&self, tool_id: &str) -> bool;
}
```

- `Option<ToolOutput>` return: `None` = this executor doesn't own the tool
- `execute_tool_call` = pre-approved path; `execute_tool_call_confirmed` = requires user approval
- Held in Agent as `Arc<dyn ErasedToolExecutor>` (type-erased for object safety)

## CompositeExecutor

Chains multiple executors; first `Some(...)` response wins:

```
CompositeExecutor [
    SkillExecutor,     // SKILL.md inline tools
    ShellExecutor,     // shell commands
    FileExecutor,      // file read/write/list
    WebScrapeExecutor, // URL fetch + markdown conversion
    McpExecutor,       // MCP server tools (if mcp feature)
    NativeExecutor,    // memory_search, memory_save, load_skill, scheduler
]
```

## Shell Executor Security

- **Blocklist check runs unconditionally** before `PermissionPolicy` evaluation — cannot be bypassed
- Blocked patterns: process substitution `$(...)`, here-strings `<<<`, dangerous builtins
- `TrustLevel`: `Untrusted` / `Provisional` / `Trusted` — affects which commands are auto-approved
- Working directory is sandboxed to project root (configurable)

## Tool Audit

Every tool call is logged to `audit.jsonl`:

```json
{ "ts": "...", "tool": "shell", "call": {...}, "result": "...", "trust": "Trusted", "approved_by": "auto" }
```

- Audit log is append-only — never truncated mid-session
- Sensitive values in tool calls are redacted before logging

## Output Filtering

`FilterPipeline` — composable multi-stage filter applied to tool output before injecting into context:

- `CommandMatcher` variants: `Exact`, `Prefix`, `Regex`, `Custom`
- `FilterResult` carries `FilterConfidence`: `Full / Partial / Fallback`
- `SecurityPatterns`: 17 LazyLock regex across 6 categories (secrets, paths, tokens, etc.)
- Applied at `CompositeExecutor` level — output is filtered before stored in `ToolOutput`
- `FilterMetrics`: in-memory counters per filter, periodic debug logging

## Native Tools

Always available (no feature flag):

| Tool | Function |
|---|---|
| `memory_search` | Semantic recall from conversation history |
| `memory_save` | Explicit save to long-term memory |
| `load_skill` | Fetch full SKILL.md body on demand |
| `scheduler` | Register periodic or deferred tasks (natural language) |

## Key Invariants

- Blocklist check is unconditional — PermissionPolicy cannot bypass it
- `execute_tool_call` and `execute_tool_call_confirmed` are separate codepaths — never collapse them
- Composite chain order is deterministic and must not change without explicit config
- `ToolError::kind()` must be checked by callers: `Transient` → retry, `Permanent` → abort turn
- Audit log is written before result is returned to agent
