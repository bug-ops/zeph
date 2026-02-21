# Sub-Agent Orchestration

Zeph supports delegating tasks to specialized sub-agents that run in isolated contexts with controlled permissions. Sub-agents are defined as markdown files with TOML frontmatter and communicate with the main agent via the A2A protocol over in-process channels.

## Concepts

A sub-agent is a lightweight, disposable agent instance that:

- Has its own system prompt, message history, and LLM model
- Receives a filtered subset of tools and skills from the main agent
- Starts with zero permissions (zero-trust model)
- Communicates with the main agent via A2A protocol messages
- Cannot spawn other sub-agents (no nesting)
- Is automatically cleaned up on completion, cancellation, timeout, or crash

The main agent remains the single user-facing interface. Sub-agents are invisible to the user except through status updates and approval prompts.

## Architecture

```
User
  |
  v
+---------------------------+
|  Main Agent               |
|  - user channel           |
|  - SubAgentManager        |
|  - VaultProvider          |
|  - SkillRegistry          |
+-----|---------------------+
      |
      | A2A (in-process mpsc channels)
      |
  +---+---+---+
  |   |   |   |
  v   v   v   v
 SA1 SA2 SA3 SA4   (sub-agent instances)
```

Each sub-agent has:
- Own message history and system prompt
- Filtered tool access via `FilteredToolExecutor`
- Filtered skill access via glob patterns
- No vault access unless explicitly granted with TTL

## Definition Format

Sub-agent definitions are markdown files with `+++`-delimited TOML frontmatter, stored in:

- `.zeph/agents/` (project scope, higher priority)
- `~/.config/zeph/agents/` (user scope)

When both directories contain a definition with the same `name`, the project-scope file takes precedence.

### Example

```markdown
+++
name = "code-reviewer"
description = "Reviews code changes for correctness and style"
model = "claude-sonnet-4-20250514"

[tools]
allow = ["shell", "web_scrape"]

[permissions]
secrets = ["github-token"]
max_turns = 10
background = false
timeout_secs = 300
ttl_secs = 120

[skills]
include = ["git-*", "rust-*"]
exclude = ["deploy-*"]
+++

You are a code reviewer. Analyze the provided code for:
- Correctness bugs
- Performance issues
- Idiomatic Rust style

Report findings as a structured list with severity (critical/warning/info).
```

### Frontmatter Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | required | Unique identifier for the sub-agent |
| `description` | string | required | Human-readable description |
| `model` | string | inherited | LLM model override (uses main agent model if omitted) |
| `tools.allow` | string[] | - | Allowlist of tool IDs (mutually exclusive with `deny`) |
| `tools.deny` | string[] | - | Denylist of tool IDs (mutually exclusive with `allow`) |
| `permissions.secrets` | string[] | `[]` | Vault keys this agent MAY request (not granted by default) |
| `permissions.max_turns` | u32 | `20` | Maximum LLM turns before forced stop |
| `permissions.background` | bool | `false` | Run in background (non-blocking) |
| `permissions.timeout_secs` | u64 | `600` | Hard kill deadline in seconds |
| `permissions.ttl_secs` | u64 | `300` | TTL for granted permissions and secrets |
| `skills.include` | string[] | `[]` (all) | Glob patterns for skill names to include |
| `skills.exclude` | string[] | `[]` | Glob patterns for skill names to exclude |

If neither `tools.allow` nor `tools.deny` is specified, the sub-agent inherits all tools from the main agent.

The body after the closing `+++` becomes the sub-agent's system prompt.

## SubAgentManager API

`SubAgentManager` lives in `zeph-core` and manages the full sub-agent lifecycle:

```rust
let mut manager = SubAgentManager::new(/* max_concurrent */ 4);

// Load definitions from project and user directories
manager.load_definitions(&[
    project_dir.join(".zeph/agents"),
    dirs::config_dir().unwrap().join("zeph/agents"),
])?;

// Spawn a sub-agent by definition name
let task_id = manager.spawn("code-reviewer", "Review src/main.rs")?;

// Check status
let statuses = manager.statuses();

// Cancel if needed
manager.cancel(&task_id)?;

// Collect result (removes from active set)
let result = manager.collect(&task_id).await?;
```

Key operations:

| Method | Description |
|--------|-------------|
| `load_definitions(&[PathBuf])` | Load `.md` definitions from directories (first-wins deduplication) |
| `spawn(name, prompt)` | Spawn a sub-agent, returns task ID |
| `cancel(task_id)` | Cancel and revoke all grants |
| `collect(task_id)` | Await result and remove from active set |
| `statuses()` | Snapshot of all active sub-agent states |
| `approve_secret(task_id, key, ttl)` | Grant a vault secret after user approval |

Concurrency is enforced: `spawn` returns an error if the active agent count reaches `max_concurrent`.

## Zero-Trust Security Model

Every sub-agent starts with zero permissions. Access is never implicit.

### Core Rules

1. **No default trust** -- the definition declares what a sub-agent MAY request, not what it has
2. **Explicit user approval** -- every secret requires user consent at runtime
3. **TTL on all grants** -- secrets and runtime permissions expire after `ttl_secs`
4. **Automatic revocation** -- all grants revoked on completion, cancellation, or crash
5. **No persistence** -- secrets exist only in memory, never written to disk or message history
6. **Audit trail** -- every grant, revoke, and expiry event logged via `tracing`
7. **Sweep before access** -- `PermissionGrants::sweep_expired()` called before every tool execution and secret read

### Grant Lifecycle

```
Request -> User Approval -> Grant (with TTL) -> Active -> Expired/Revoked
                |                                            |
              Denied                                 Cleared from memory
```

### PermissionGrants

`PermissionGrants` tracks active grants and enforces TTL:

- `add(kind, ttl)` -- add a new time-bounded grant
- `is_active(kind)` -- check if a grant is still valid (auto-sweeps expired)
- `sweep_expired()` -- remove all expired grants
- `revoke_all()` -- immediately revoke all grants
- `Drop` impl revokes remaining grants as defense-in-depth

Grant kinds:
- `GrantKind::Secret(key)` -- vault secret access (redacted in logs and `Display`)
- `GrantKind::Tool(name)` -- runtime tool access beyond static policy

### Secret Delivery Protocol

1. Sub-agent sends `InputRequired` status with `SecretRequest { secret_key, reason }`
2. Main agent validates: is the key in the definition's allowed `secrets` list?
3. If not in allowed list, auto-deny (no user prompt)
4. If allowed, prompt user: agent name, key name, TTL
5. User approves: vault read, in-memory grant with TTL via `approve_secret()`
6. Sub-agent accesses secret via `PermissionGrants` (not via message channel)
7. On TTL expiry or sub-agent end: grant swept, secret cleared from memory

Secrets are never serialized into A2A messages, message history, or logs.

## Tool Filtering

`FilteredToolExecutor` wraps the main agent's tool executor and enforces the sub-agent's `ToolPolicy`:

- **AllowList** -- only listed tools are permitted
- **DenyList** -- all tools except listed ones are permitted
- **InheritAll** -- all parent tools are available

Enforcement happens at the executor level (not prompt level). Rejected tool calls return a `ToolError::Blocked`. Tool definitions exposed to the sub-agent's LLM are also filtered, so the model only sees tools it can actually use.

## Skill Filtering

Skills are filtered from the main agent's `SkillRegistry` using glob patterns:

- `include = ["git-*", "rust-*"]` -- only skills matching these patterns
- `exclude = ["deploy-*"]` -- remove skills matching these patterns
- Empty `include` means all skills pass (unless excluded)
- Exclude patterns always take precedence over include

Supported glob syntax: `*` wildcard (e.g., `git-*`, `*-review`). Double-star `**` is not supported.

Filtered skills are injected into the sub-agent's system prompt at spawn time.

## A2A Channel Communication

Sub-agents communicate with the main agent via in-process bidirectional channels (`tokio::sync::mpsc`) that carry A2A protocol messages:

```
Main Agent                          Sub-Agent
    |                                   |
    |--- SendMessage(task prompt) ----->|
    |                                   |
    |<-- StatusUpdate(working) ---------|
    |<-- StatusUpdate(working, msg) ----|  (progress)
    |                                   |
    |<-- StatusUpdate(input-required) --|  (needs permission)
    |--- SendMessage(approval) -------->|
    |                                   |
    |<-- ArtifactUpdate(result) --------|
    |<-- StatusUpdate(completed) -------|
```

Message types (`A2aMessage` enum):
- `SendMessage` -- carries an A2A `Message`
- `StatusUpdate` -- carries a `TaskStatusUpdateEvent`
- `ArtifactUpdate` -- carries a `TaskArtifactUpdateEvent`
- `Cancel` -- signals cancellation

The channel reuses A2A types from `zeph-a2a` directly. This means upgrading to HTTP-based A2A (remote sub-agents) in the future requires zero protocol changes.

## Error Handling

Sub-agent operations return `SubAgentError`:

| Variant | When |
|---------|------|
| `Parse` | Definition file has invalid frontmatter or TOML |
| `Invalid` | Validation failure (empty name, mutual exclusion, unauthorized secret) |
| `NotFound` | Unknown definition name or task ID |
| `Spawn` | Concurrency limit reached or task panic |
| `Cancelled` | Sub-agent was cancelled |

## Background Execution

Sub-agents can run in the background without blocking the main agent loop. This is the default execution model: `spawn()` returns immediately with a task ID, and the sub-agent runs on a separate Tokio task.

### Non-Blocking Spawn

```rust
// Spawn returns immediately — the sub-agent runs in the background
let task_id = manager.spawn("code-reviewer", "Review src/main.rs", provider, executor)?;
// Main agent continues processing user input
```

The `spawn()` call creates a `JoinHandle` via `tokio::spawn` and inserts the handle into the active agents map. The main agent loop is never blocked.

### Polling for Completed Results

The agent loop calls `poll_subagents()` on every iteration to check for finished sub-agents:

```rust
let completed = agent.poll_subagents().await;
for (task_id, result) in completed {
    // Notify user about completed background work
}
```

`poll_subagents()` is non-blocking: it inspects each active sub-agent's `watch::Receiver<SubAgentStatus>` for terminal states (`Completed`, `Failed`, `Canceled`) and collects results from finished agents. Collected agents are removed from the active set.

### Graceful Shutdown

On main agent shutdown, `shutdown_all()` cancels every active sub-agent via their `CancellationToken` and revokes all permission grants:

```rust
manager.shutdown_all();
```

This iterates over all active agents and calls `cancel()` on each. The `CancellationToken` is checked inside the agent loop, so the sub-agent stops at the next iteration boundary.

## TUI Sub-Agent Panel

When the `tui` feature is enabled, the TUI dashboard displays a dedicated sub-agent panel in the side bar.

### Layout

The TUI uses a 4-panel side bar layout (each panel takes 25% of the side bar height):

| Panel | Content |
|-------|---------|
| Skills | Loaded skills list |
| Memory | Memory/context stats |
| Resources | System resource usage |
| **Sub-Agents** | Active sub-agent list |

The side bar is visible when the terminal width is 80 columns or wider and can be toggled on/off. On narrow terminals (< 80 columns), the side bar is hidden automatically.

### Widget

The sub-agent widget (`crates/zeph-tui/src/widgets/subagents.rs`) renders a bordered list with:

- **Agent name** with a `[bg]` marker for background agents
- **State** in uppercase, color-coded:
  - Yellow: `WORKING` / `SUBMITTED`
  - Green: `COMPLETED`
  - Red: `FAILED`
  - Cyan: `INPUT_REQUIRED`
  - Dark gray: other/unknown
- **Turn counter**: `turns_used/max_turns`
- **Elapsed time** in seconds

Example rendering:

```
┌ Sub-Agents (2) ──────────┐
│  code-reviewer  WORKING  │
│    3/20  42s             │
│  test-writer [bg]  COMPLETED │
│    10/20  100s           │
└──────────────────────────┘
```

The widget reads from `MetricsSnapshot.sub_agents` (a `Vec<SubAgentMetrics>` published via the metrics watch channel) and renders nothing when the list is empty.

## CLI `/agent` Commands

The `/agent` family of commands provides interactive control over sub-agents from the CLI and TUI chat input.

### Command Reference

| Command | Description |
|---------|-------------|
| `/agent list` | Show all loaded sub-agent definitions |
| `/agent spawn <name> <prompt>` | Spawn a sub-agent in the background |
| `/agent bg <name> <prompt>` | Alias for `spawn` |
| `/agent status` | Show all active sub-agents with state, turns, and elapsed time |
| `/agent cancel <id>` | Cancel a running sub-agent (accepts ID prefix) |
| `/agent approve <id>` | Approve a pending permission request (not yet implemented) |
| `/agent deny <id>` | Deny a pending permission request (not yet implemented) |

### Usage Examples

**List available sub-agent definitions:**

```
> /agent list
Available sub-agents:
  code-reviewer — Reviews code changes for correctness and style
  test-writer — Generates unit tests for Rust modules
```

**Spawn a sub-agent:**

```
> /agent spawn code-reviewer Review the authentication module
Sub-agent 'code-reviewer' started (id: a1b2c3d4)
```

**Check status of active sub-agents:**

```
> /agent status
Active sub-agents:
  [a1b2c3d4] working  turns=3  elapsed=42s  Analyzing auth flow...
```

**Cancel a sub-agent by ID prefix:**

```
> /agent cancel a1b2
Cancelled sub-agent a1b2c3d4-...
```

The `cancel` command accepts a prefix of the task UUID. If the prefix matches multiple agents, the command reports ambiguity and requires a longer prefix.

**Ambiguous cancel:**

```
> /agent cancel a1
Ambiguous id prefix 'a1': matches 2 agents
```

## Safety Guarantees

- `SubAgentHandle::Drop` cancels the task and revokes all grants
- `PermissionGrants::Drop` revokes remaining grants with a warning log
- Concurrency limit prevents resource exhaustion
- `timeout_secs` provides a hard kill deadline
- `max_turns` prevents runaway LLM loops
- Secret key names are redacted in `Display` and log output
