---
aliases:
  - Tool Execution
  - ToolExecutor
  - CompositeExecutor
tags:
  - sdd
  - spec
  - tools
  - execution
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec#5. Tool Execution Contract]]"
  - "[[016-output-filtering/spec]]"
  - "[[010-security/spec]]"
---

# Spec: Tool Execution

> [!info]
> ToolExecutor trait, CompositeExecutor, TAFC, schema filter, result cache,
> dependency graph, transactional ShellExecutor, utility-guided dispatch gate.

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

## Structured Shell Output Envelope


`ShellExecutor` wraps shell execution results in `ShellOutputEnvelope`:

```rust
pub struct ShellOutputEnvelope {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub truncated: bool,
}
```

`ToolOutput.raw_response` carries this envelope as a serialized JSON string. `AuditEntry` gains `exit_code: Option<i32>` and `truncated: bool` fields populated from the envelope.

### Key Invariants

- `exit_code` in `AuditEntry` is `None` only for non-shell tools — never `None` for shell calls
- `truncated = true` is set when stdout/stderr is cut to fit the output cap — the LLM must see the flag
- The envelope structure is stable — callers must not parse `raw_response` as plain text for shell results

---

## Per-Path File Read Sandbox


`FileExecutor` evaluates `[tools.file]` deny/allow lists on every read operation. Evaluation order: deny first, then allow. Paths are canonicalized before matching to prevent symlink traversal attacks.

### Config

```toml
[tools.file]
deny_read = ["**/.env", "**/secrets/**", "**/.git/config"]   # deny patterns (globs)
allow_read = []    # if non-empty, only matching paths are allowed (after deny check)
```

### Evaluation Order

1. Canonicalize the requested path (`std::fs::canonicalize`) — symlink traversal prevention
2. If path matches any `deny_read` pattern → reject with `ToolError::PolicyBlocked`
3. If `allow_read` is non-empty AND path does not match any `allow_read` pattern → reject
4. Otherwise → permit

### Key Invariants

- Deny check runs before allow check — a path matched by both deny and allow is always denied
- Canonicalized paths are used for matching — relative references and symlinks cannot escape the sandbox
- Reject produces `ToolError::PolicyBlocked`, not a generic I/O error — callers must not retry
- NEVER skip canonicalization for user-supplied paths
- `deny_read = []` with `allow_read = []` is the unrestricted default (backward-compatible)

---

## `extract_paths` Relative Path Detection


`extract_paths()` now detects relative path tokens (e.g., `src/main.rs`, `./foo/bar`) in addition to absolute paths. Detection uses the following heuristics:

- Token starts with `./` or `../`
- Token contains `/` and does not start with `http://` or `https://`
- Token matches a known source-tree pattern (e.g., `*.rs`, `*.toml` with path separator)

### Key Invariants

- Both absolute (`/usr/...`) and relative (`src/...`) paths must be detected
- URL-shaped tokens (`http://...`) must not be treated as paths even if they contain `/`
- Extracted paths feed the `claim_source` field in adversarial policy gate audit entries

## Tool Audit

Every tool call is logged to `audit.jsonl`:

```json
{ "ts": "...", "tool": "shell", "call": {...}, "result": "...", "trust": "Trusted", "approved_by": "auto", "exit_code": 0, "truncated": false }
```

- Audit log is append-only — never truncated mid-session
- Sensitive values in tool calls are redacted before logging
- `exit_code: Option<i32>` — present for shell tool calls; `null` for non-shell tools
- `truncated: bool` — set to `true` when `ShellOutputEnvelope.truncated` is true
- `claim_source` from `AdversarialPolicyGateExecutor` is propagated into `AuditEntry` — identifies the content source of adversarially-evaluated calls

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
| `compress_context` | Compress conversation via LLM, append to Knowledge block (feature-gated, see below) |

---

## compress_context Native Tool


### Overview

`compress_context` is a native tool available whenever the `context-compression`
feature is enabled. When called (by the agent or autonomously), it:

1. Compresses the current conversation history via a dedicated LLM call
2. Appends the compressed summary to the `Knowledge` block in the system prompt
3. Removes the original messages from the in-memory context window
4. Records the compression event in SQLite with a `CompactionStrategy::Autonomous` marker

Unlike `compact_context` (triggered at 90% context pressure), `compress_context`
can be invoked at any time. It is visible to the agent as a first-class tool call.

### CompressionStrategy

```rust
pub enum CompressionStrategy {
    Triggered,    // automatic hard-threshold compaction
    Manual,       // /compact command
    Autonomous,   // agent-initiated compress_context tool call
}
```

The strategy is stored in the `compaction_method` column on the `summaries` table.

### Tool Availability

`compress_context` is always registered in the tool catalog when the
`context-compression` feature is enabled — it does not require `[agent.tool_filter]`
to be disabled.

### Config

```toml
[agent]
compress_provider = ""  # provider for compression LLM call; empty = primary provider
```

When `compress_provider` is set, it references a `[[llm.providers]]` entry by name.
An empty string falls back to the agent's primary provider.

### Key Invariants

- `compress_context` appends to the Knowledge block, never replaces it
- The original messages are removed from the in-memory context after successful compression
- On compression failure, original messages are preserved — never lose content
- `CompactionStrategy::Autonomous` must be distinct from `Triggered` and `Manual` in queries and TUI display
- `compress_context` is non-cacheable (side effects on context window) — must be in the non-cacheable set
- NEVER call `compress_context` recursively from within a compress_context execution

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

- TAFC augmentation applies only when the provider returns `supports_tool_use() = true` — providers without native tool support (e.g., Candle) are not augmented
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

- Providers that return `supports_tool_use() = false` (e.g., Candle) receive the full unfiltered set — filtering has no effect when there is no native tool path
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

---

## Tool Error Taxonomy

`crates/zeph-tools/src/error.rs`. Issue #2203.

### Overview

11-category error taxonomy for tool invocation failures, based on arXiv:2601.16280. Replaces opaque `[error] ...` strings with structured `[tool_error]` blocks that include category, message, suggestion, and retryability signal.

### ToolErrorCategory

| Category | Retryable | Triggers self-reflection |
|---|---|---|
| `ToolNotFound` | No | No |
| `InvalidParameters` | Yes (reformat) | Yes |
| `TypeMismatch` | Yes (reformat) | Yes |
| `PolicyBlocked` | No | No |
| `ConfirmationRequired` | No | No |
| `PermanentFailure` | No | No |
| `Cancelled` | No | No |
| `RateLimited` | Yes | No |
| `ServerError` | Yes | No |
| `NetworkError` | Yes | No |
| `Timeout` | Yes | No |

Self-reflection (`is_quality_failure()`) is only triggered for `InvalidParameters` and `TypeMismatch` — infrastructure errors (Network, Server, Rate) never trigger self-reflection.

### ToolErrorFeedback

`format_for_llm()` produces:
```
[tool_error]
category: InvalidParameters
message: ...
suggestion: ...
retryable: true
```

### ToolError::Shell Variant

`ToolError::Shell { exit_code, category, message }` — used by `ShellExecutor` for classified exit-code failures:
- Exit 126 → `PolicyBlocked`
- Exit 127 → `PermanentFailure`
- Stderr "permission denied" / "no such file or directory" (case-insensitive) → `PermanentFailure`

### Config

```toml
[tools.retry]
max_attempts = 2
base_ms = 500
max_ms = 5000
budget_secs = 30
parameter_reformat_provider = ""  # provider name for parameter reformat path
```

`--migrate-config` auto-migrates `[agent].max_tool_retries` → `[tools.retry].max_attempts` and `[agent].max_retry_duration_secs` → `[tools.retry].budget_secs`.

### Key Invariants

- `is_quality_failure()` must return `false` for all infrastructure error categories (Network, Server, Rate, Timeout) — self-reflection on infrastructure errors wastes tokens and context
- `ToolError::Shell` must classify exit codes before passing to the ToolErrorFeedback pipeline
- `AuditEntry.error_category` must be set for every failed tool call
- NEVER trigger self-reflection on `PolicyBlocked` — this is a policy decision, not a quality failure

---

## Transactional ShellExecutor


Opt-in snapshot+rollback for shell commands. Before executing a write command, `ShellExecutor` captures a file-level snapshot; on configured exit codes the snapshot is restored.

### Config

```toml
[tools.shell]
transactional = false
transaction_scope = ["**"]          # glob-filtered paths to snapshot
auto_rollback = false
auto_rollback_exit_codes = [1, 2]
snapshot_required = false           # fail-closed if snapshot fails
max_snapshot_bytes = 0              # 0 = unlimited; returns SnapshotFailed when exceeded
```

### Write Detection

Write commands are detected via `WRITE_INDICATORS` heuristic (keywords like `rm`, `mv`, `cp`, `dd`, `truncate`, `tee`) plus redirection target extraction (`>`, `>>`). False negatives are acceptable; false positives only cause unnecessary snapshots.

### New Variants

- `ToolError::SnapshotFailed` — snapshot could not be captured; used when `snapshot_required = true`
- `AuditResult::Rollback` — emitted when a rollback is performed
- `ToolEvent::Rollback` — broadcast to TUI/channels on rollback

### Key Invariants

- Snapshot storage uses `tempfile::TempDir` — automatically cleaned on success or process exit
- Rollback MUST restore originals atomically (rename); partial restore is a hard error
- User-requested commands bypass the gate unconditionally — opt-in only
- `max_snapshot_bytes = 0` means unlimited; any other value is a hard cap

---

## Utility-Guided Tool Dispatch Gate


`UtilityScorer` assigns a score to each candidate tool call before execution. Calls below the configured threshold are skipped. Disabled by default.

### Scoring Components

| Component | Description |
|-----------|-------------|
| Estimated gain | Expected information value of the call |
| Token cost | Estimated tokens consumed |
| Redundancy | Similarity to recent tool outputs |
| Exploration bonus | Bonus for tools not recently called |

### Config

```toml
[tools.utility]
enabled = false
threshold = 0.0   # calls below this score are skipped
```

### Key Invariants

- User-requested tool calls (explicit in turn) bypass the gate unconditionally
- Scoring errors are fail-closed — uncertain scores do not allow execution
- NEVER skip `memory_save` or other side-effect tools on pure score alone

---

## Adversarial Policy Gate


LLM-based pre-execution validation of tool calls against plain-language operator policies.

### Executor Chain Order

```
PolicyGateExecutor → AdversarialPolicyGateExecutor → TrustGateExecutor → ...
```

### Config

```toml
[tools.adversarial_policy]
enabled = false
fail_open = false      # true = allow on LLM error; false = deny on error
policies = []          # plain-language policy strings
exempt_tools = ["memory_save", "memory_search", "read_overflow", "load_skill", "schedule_deferred"]
```

### Key Invariants

- `exempt_tools` defaults prevent false denials for internal agent operations
- Prompt injection hardening: tool call parameters are code-fence quoted before LLM call
- Response parsing is strict: only `ALLOW` / `DENY` tokens are accepted
- `fail_open = false` is the secure default — unknown LLM response → deny
- Audit log records `adversarial_policy_decision` field for every evaluated call
- `claim_source` is propagated from `AdversarialPolicyGateExecutor` into `AuditEntry` — identifies the content origin of each evaluated call; relative path tokens (e.g. `src/main.rs`) are detected by `extract_paths()`
- `/status` shows gate state (provider, policy count, `fail_open`) when `enabled = true`
- NEVER retry `PermanentFailure` or `ToolNotFound` — infinite retry loops are a liveness hazard

---

## Tool Invocation Phase Taxonomy


Tool calls are categorized into phases based on when they occur in the agent's reasoning cycle. Phase is used by the adversarial policy gate and audit system to apply different trust policies to calls made in different contexts.

### Phases

| Phase | Description |
|-------|-------------|
| `Planner` | Tool called during plan construction (before first LLM inference of turn) |
| `Executor` | Tool called as a result of LLM tool_use response |
| `Verifier` | Tool called during post-execution verification |
| `Autonomous` | Tool called by agent-initiated compress_context or similar internal ops |

### Key Invariants

- Phase is determined at call site — never inferred from tool name
- Adversarial policy may apply different policies per phase — phase must be included in gate audit entry
- `Autonomous` phase calls bypass the user-facing confirmation path unconditionally

---

## Reasoning Model Hallucination Detection


For reasoning models (e.g., o3, claude-sonnet thinking blocks), a heuristic detects when tool call parameters appear to have been hallucinated (not grounded in context).

### Detection Heuristics

1. **Path plausibility**: file path parameters are checked against the known file system state via `extract_paths()`. Paths that do not exist and were not mentioned in context trigger a `HallucinationSuspect` warning.
2. **Entity reference grounding**: named entities in parameters are checked for presence in the current turn's context window. Entities that appear only in the tool call but nowhere in context are flagged.

### Config

```toml
[agent]
hallucination_detection = false
compress_provider = ""   # provider for compress_context tool; empty = primary
```

### Key Invariants

- Hallucination detection is heuristic — NEVER hard-block a tool call on hallucination suspicion alone; always warn and continue unless adversarial policy explicitly blocks
- `reasoning_model_detection` determines whether the model is treated as a reasoning model — this is config-driven, not inferred from model name alone
- `compress_provider` must be wired at agent bootstrap — NEVER default to empty string at execution time

---

## Tool Call Quota and OAP Authorization

> **Status**: Implemented. Source: `crates/zeph-tools/src/config.rs`.
> Full invariants documented in `008-mcp/spec.md` (MCP identity propagation section).

### `max_tool_calls_per_session`

```toml
[tools]
max_tool_calls_per_session = 100   # Option<u32>; None = unlimited (default)
```

Counts first attempt only — retries are free. When exhausted, executor returns `quota_blocked` error.

### `[tools.authorization]` — OAP Authorization

```toml
[tools.authorization]
enabled = false   # default

[[tools.authorization.rules]]
action = "allow" | "deny"
tools = ["tool_name", ...]
```

Rules appended after `[tools.policy]` rules. `[tools.policy]` has precedence (first-match-wins). Disabled by default — no behavioral change for existing configs.

### `caller_id` on `ToolCall`

`ToolCall::caller_id: Option<String>` — set by orchestrator when a sub-agent dispatches a call. Recorded in audit log. Primary agent leaves `None`.

### Key Invariants

- See `008-mcp/spec.md` for the complete invariant list
- NEVER let quota exhaustion silently drop a tool call — always return `quota_blocked`
- OAP rules are merged at startup — runtime changes require restart
