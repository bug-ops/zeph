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

---

## TAFC: Think-Augmented Function Calling

`crates/zeph-tools/src/config.rs` (`TafcConfig`), `crates/zeph-core/src/agent/tool_execution/mod.rs`.

### Overview

TAFC injects a hidden `_tafc_think` parameter into complex tool schemas before sending to the LLM. The model fills in this field with its reasoning about how to call the tool, improving parameter accuracy for tools with high schema complexity. The think field is stripped from the tool call before execution — it is only visible to the LLM, never executed.

### Schema Augmentation

`tool_def_to_definition_with_tafc(def, tafc)`:
1. Compute `schema_complexity(def)` — proprietary heuristic returning a score in `[0.0, 1.0]`
2. If `complexity >= tafc.complexity_threshold` (default 0.6): inject `_tafc_think` as a top-level string property into the tool's JSON Schema parameters

`_tafc_think` field description instructs the model to reason step-by-step about the tool parameters before filling them in.

### Execution Path

After LLM response, TAFC fields are stripped via `strip_tafc_fields()` before constructing `ToolCall`:
- All keys starting with `_tafc_think` are removed from the params map
- If the remaining map contains only TAFC fields (no real params), the tool call is skipped entirely

TAFC field content is intentionally dropped — never written to audit log or stored in memory.

### Config

```toml
[tools.tafc]
enabled = false             # opt-in
complexity_threshold = 0.6  # [0.0, 1.0]; NaN/Inf resets to 0.6
```

### Key Invariants

- TAFC augmentation applies only to the native `tool_use` path — prompt-based providers (Ollama, Candle) are not augmented
- `_tafc_think` fields must be stripped before `ToolCall` construction — never execute them
- If all params are TAFC fields, the tool call is silently skipped (model produced only reasoning)
- `complexity_threshold` is validated and clamped to `[0.0, 1.0]` — NaN/Inf is reset to 0.6
- NEVER log `_tafc_think` content to audit log or debug dumps
- NEVER pass `_tafc_think` fields to executor — executor must never see them

---

## Tool Schema Filtering

`crates/zeph-tools/src/schema_filter.rs`. Issue #2020.

### Overview

Dynamic tool schema filtering reduces the number of tool definitions sent to the LLM per turn. Only the most relevant tools are selected based on embedding cosine similarity between the user query and pre-computed tool description embeddings. Reduces context waste in deployments with many tools (especially MCP servers).

### Filtering Pipeline

`filter(query_embedding, all_tools)` → `ToolFilterResult`:
1. Always-on tools (from `always_on` config list) → `InclusionReason::AlwaysOn`
2. Tools mentioned by name in the user query → `InclusionReason::NameMentioned`
3. Tools with descriptions shorter than `min_description_words` (MCP tools) → `InclusionReason::ShortDescription`
4. Tools with no cached embedding → `InclusionReason::NoEmbedding`
5. Remaining: score by cosine similarity, keep top-K → `InclusionReason::SimilarityRank`
6. Dependency gate applied after filter (see Tool Dependency Graph below)

`ToolFilterResult.included` is a `HashSet<String>` of tool IDs that passed. `excluded` lists filtered-out tools. `dependency_exclusions` lists tools blocked by unmet hard dependencies.

### Inclusion Reasons

| Reason | Bypass similarity filter? |
|---|---|
| `AlwaysOn` | Yes |
| `NameMentioned` | Yes |
| `ShortDescription` | Yes |
| `NoEmbedding` | Yes |
| `SimilarityRank` | No |
| `DependencyMet` | Gate-only |
| `PreferenceBoost` | Boost only |

### Known Limitations

- Prompt-based providers (Ollama non-native, Candle) receive the full unfiltered set — filtering has no effect on the prompt path
- Each turn with a different top-K selection invalidates the Claude `cache_control` breakpoint on `tools`, increasing `cache_creation_input_tokens`
- Expected token savings: 15–25% in practice (4 always-on + top-K + name-mentioned + NoEmbedding tools)

### Config

```toml
[agent.tool_filter]
enabled = false        # opt-in; default off
top_k = 10             # max similarity-ranked tools
min_description_words = 3  # tools with fewer words always pass
always_on = ["memory_search", "memory_save", "load_skill", "scheduler"]
```

### TUI / Status

`/status` shows `Filter: top_k={k}, always_on={n}, embeddings={m}` when enabled. Silent when disabled.

### Key Invariants

- Filtering is applied after TAFC augmentation and before TAFC strip
- Dependency gates (see below) are applied AFTER schema filtering — `apply()` is a separate composable step
- Always-on and name-mentioned tools always bypass hard dependency gates
- Filtering must not remove tools that the LLM already referenced in the current turn
- NEVER filter when no query embedding is available — return full tool set

---

## Tool Result Cache

`crates/zeph-tools/src/cache.rs`. `ToolResultCache`.

### Overview

Session-scoped in-memory cache for tool results. Avoids redundant executions of identical tool calls within a session. Keys are `(tool_name, args_hash)` pairs. Cache is not `Send + Sync` — accessed only from the agent's single-threaded tool loop.

### Non-cacheable Tools

Tools with side effects are permanently excluded:

| Tool | Reason |
|---|---|
| `bash` | Shell side effects, mutable state |
| `memory_save` | Writes to memory store |
| `memory_search` | Results may change after `memory_save` |
| `scheduler` | Creates/modifies scheduled tasks |
| `write` | Writes files |
| `mcp_*` (prefix) | Third-party, unknown side effects |

### Behavior

- `get(key)`: returns cached `ToolOutput` or `None`; expired entries are lazily evicted on access
- `put(key, output)`: inserts entry; no-op when disabled
- `ttl = None`: entries never expire (useful for batch sessions)
- `ttl = Some(d)`: lazy eviction on `get()`; expired entry removed from map
- `clear()`: removes all entries and resets hit/miss counters — called on `/clear`

### Config

```toml
[tools.result_cache]
enabled = true
ttl_secs = 300  # 0 = never expire
```

### Key Invariants

- Cache is session-scoped only — never persisted across sessions
- Non-cacheable tools are defined in a `LazyLock<HashSet>` — the set must be updated when new write-path tools are added
- `clear()` resets counters — always call on `/clear` to avoid stale hits in new sessions
- NEVER cache MCP tools — they are third-party and may have unknown side effects
- NEVER share `ToolResultCache` across async tasks — it is intentionally not `Send + Sync`

---

## Tool Dependency Graph

`crates/zeph-tools/src/schema_filter.rs` (`ToolDependencyGraph`). Issue #2024.

### Overview

Sequential tool availability control: some tools should only appear in the LLM's schema after prerequisite tools have been called. `requires` enforces hard gates (tool hidden until all prerequisites completed). `prefers` adds a soft similarity boost when prerequisites are met.

### `ToolDependency` Config

```toml
[tools.dependencies.rules.read_file]
requires = []         # hard gate — hidden until all listed tools completed

[tools.dependencies.rules.write_file]
requires = ["read_file"]   # hidden until read_file completed successfully
prefers = ["list_files"]   # gets +0.15 similarity boost per satisfied prereq
```

### Cycle Detection

`ToolDependencyGraph::new()` runs DFS-based cycle detection. All tools in any detected cycle have their `requires` entries cleared and are made unconditionally available. A `WARN` log is emitted listing cycle participants.

### Deadlock Fallback

If `apply()` would block ALL non-always-on tools (all have unmet hard gates), the dependency gates are disabled for that turn with a `WARN` log. This prevents the agent from having no callable tools.

### Completed Tool Tracking

`completed_tool_ids` is a session-scoped set tracking which tools completed successfully. Cleared on `/clear`. Used by `apply()` to evaluate `requires` and `prefers`.

### Preference Boost

Per satisfied `prefers` dependency: `+boost_per_dep` added to similarity score (default 0.15). Capped at `max_total_boost` regardless of how many `prefers` deps are met (default 0.20).

### Config

```toml
[tools.dependencies]
enabled = false
boost_per_dep = 0.15
max_total_boost = 0.20
rules = {}
```

### Key Invariants

- Dependency gates apply AFTER schema filtering — `apply()` is a separate post-filter step
- Always-on and name-mentioned tools always bypass hard gates
- `requires` cycles are broken at construction time — never at filter time
- Deadlock fallback is mandatory — never leave the agent with zero callable tools
- `completed_tool_ids` must be cleared on `/clear` — stale completed set causes gate bypass
- NEVER evaluate `requires` against tools from previous sessions (only current session's `completed_tool_ids`)
