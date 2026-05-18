---
aliases:
  - MAGE + SafeHarbor
  - Shadow Memory Guardrail
  - Multi-Turn Threat Detection
  - Hierarchical Memory Guardrail
tags:
  - sdd
  - spec
  - security
  - defense
  - agent-loop
  - contract
created: 2026-05-17
status: approved
related:
  - "[[010-security/spec]]"
  - "[[010-2-injection-defense]]"
  - "[[010-4-audit]]"
  - "[[010-6-vigil-intent-anchoring]]"
  - "[[050-security-capability-governance/spec]]"
  - "[[004-memory/spec]]"
  - "[[040-sanitizer/spec]]"
---

# Spec: Shadow Memory Guardrail (MAGE + SafeHarbor)

> [!info]
> Two complementary layers of multi-turn threat detection and runtime guardrail refinement.
>
> **MAGE** (arXiv:2605.03228): Shadow memory that accumulates cross-turn risk signals
> (tool call summaries, anomaly scores, goal trajectory deviations) to detect long-horizon
> attack patterns beyond single-turn injection defenses.
>
> **SafeHarbor** (arXiv:2605.05704): Hierarchical memory tree of guardrail rules
> that evolves via entropy-based node splitting/merging, injected into system prompt
> before each LLM call to refine guardrail boundaries per request context.

## Sources

### External

- **MAGE: Multi-Turn Agent Goal Hijacking Detection via Shadow Memory** (arXiv:2605.03228, 2026)
  — accumulation of tool risk scores, pattern frequency analysis, goal-state coherence scoring
- **SafeHarbor: Hierarchical Memory-Backed Guardrail Boundary Refinement** (arXiv:2605.05704, 2026)
  — entropy-based tree evolution, context-aware rule injection, per-request guardrail tuning
- **Threat Detection in Agentic Systems** (OWASP AI Agent Security, 2026)
  — multi-turn attack patterns, boundary erosion detection, defense-in-depth layering

### Internal

| File | Contents |
|---|---|
| `crates/zeph-core/src/vault/` | `VaultProvider`, credential resolution |
| `crates/zeph-sanitizer/src/` | `ContentSanitizer`, `GuardrailFilter` (static prompt baseline) |
| `crates/zeph-memory/src/` | `SemanticMemory`, SQLite backend, Qdrant vector store |
| `crates/zeph-agent-tools/src/` | Native tool loop, pre-action hooks, tool execution phases |
| `crates/zeph-core/src/agent/security.rs` | `SecurityState`, risk accumulation |
| `crates/zeph-config/src/security.rs` | `SecurityConfig` |

---

## 1. Overview

### Problem Statement

Existing per-action security defenses (VIGIL, ContentSanitizer, guardrail filters)
protect against single-turn injection and immediate boundary violations. However,
sophisticated adversaries can orchestrate multi-turn attack sequences that:

1. **Goal hijacking**: Gradually shift the agent's objectives through repeated
   tool output manipulations (e.g., convince agent to "pivot to debugging mode").
2. **Boundary erosion**: Slowly relax guardrail boundaries through incremental
   exceptions ("can we do X?" → "what about Y?" → "then surely Z?").
3. **Pattern obfuscation**: Hide malicious intent across 10+ turns with benign
   intermediate states that individually pass checks.

### Solution: Dual-Layer Detection

**Layer 1: MAGE Shadow Memory**
- Maintains session-scoped "shadow" record of tool execution risk signals
- Accumulates per-turn: tool call summaries (sanitized, never raw user content),
  risk scores (pre-action probe), goal trajectory coherence
- Pre-action probe fires before every tool execution, returning cumulative threat
  score from shadow memory analysis
- Triggers on: repeated tool patterns, risk score acceleration, goal-state divergence

**Layer 2: SafeHarbor Guardrail Tree**
- Maintains persistent hierarchical tree of guardrail rules (category → subcategory → decision rules)
- Rules auto-evolve via entropy-based splitting/merging based on classification decisions
- Before each LLM inference, prepend context-aware rules from tree to system prompt
- Reduces false positives on edge cases via memory-backed boundary tuning

### Goal

- Detect 10+ turn goal-hijacking scenarios before agent commits to irreversible action
- Reduce guardrail false positives on legitimate edge cases via memory-tuned rule injection
- Maintain backwards compatibility: both mechanisms optional, existing behavior unchanged when disabled
- All risk assessment and rule evolution run synchronously in agent loop (no background processes)

### Out of Scope — "Shadow Memory is Not a Forensic History"

This section is **normative** — developer must include in rustdoc and CI playbook:

> **Shadow memory is optimized for real-time multi-turn threat detection,
> NOT forensic post-incident analysis.** It stores only sanitized summaries,
> never raw user messages or complete tool outputs. For forensic audit,
> use the existing audit trail (`AuditEntry` persistence).
>
> **Explicit non-goals for v1:**
>
> - Precise goal-state reconstruction (uses trajectory summary, not complete history)
> - Cross-session threat correlation (shadow memory is session-scoped, evicted at session end)
> - User-facing transparency of shadow memory contents (internal only, never exposed via API/tools)
> - Real-time network exfiltration detection (covered by egress logging and SSRF guards)
>
> **What v1 does provide:**
>
> - Cumulative risk score combining multiple per-turn probes
> - Pattern-frequency detection: same tool called N times in M turns
> - Goal coherence check: LLM-rated turn intent vs. tool outcome alignment
> - SafeHarbor rule evolution: slow feedback loop on guardrail boundary precision
>
> **v2 scope** (filed as GitHub issue before PR merge):
>
> - Cross-session shadow aggregation (anonymized threat fingerprints)
> - Semantic goal-state reconstruction (embedding-based trajectory clustering)
> - Real-time LLM-based anomaly scoring (replaces simple frequency heuristics)

---

## 2. Functional Requirements

### MAGE Shadow Memory

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN an agent session starts AND `[sanitizer.shadow_memory].enabled = true` THE SYSTEM SHALL initialize an empty `ShadowMemory` struct scoped to the session. | must |
| FR-002 | WHEN a tool is executed AND shadow memory is enabled THE SYSTEM SHALL invoke the pre-action risk probe BEFORE calling `ToolExecutor::execute`, passing the sanitized tool name, argument summary (first 256 chars of JSON, sanitized), and accumulated risk score from shadow memory. | must |
| FR-003 | WHEN the pre-action risk probe completes THE SYSTEM SHALL update `ShadowMemory::cumulative_risk_score` (range 0.0–1.0) and log the result in `ShadowMemory::turn_history[turn_idx]`. | must |
| FR-004 | WHEN `cumulative_risk_score > [sanitizer.shadow_memory].risk_threshold` (default 0.7) THE SYSTEM SHALL emit a `SecurityEvent::ShadowMemoryAlert` with `threat_level = High` and `action = Block`. | must |
| FR-005 | WHEN a turn ends THE SYSTEM SHALL truncate `ShadowMemory::turn_history` to the most recent `[sanitizer.shadow_memory].max_turns_retained` turns (default 20). Evicted turns are discarded, never persisted. | must |
| FR-006 | WHEN the session ends OR user invokes `/clear` OR agent is stopped THE SYSTEM SHALL discard the entire `ShadowMemory` without persistence. | must |
| FR-007 | WHEN the risk probe evaluates a tool call THE SYSTEM SHALL compute: (a) pattern frequency (same tool name in last N turns), (b) goal-trajectory coherence (LLM rates if turn output aligns with stated intent), (c) risk acceleration (trend of risk scores over last 5 turns). | must |
| FR-008 | WHEN shadow memory stores turn summaries THE SYSTEM SHALL store ONLY: tool name, argument hash (SHA256 of first 256 chars), sanitized risk score, goal coherence rating (1–5 scale). NEVER raw user messages, raw tool outputs, or plaintext arguments. | must |
| FR-009 | WHEN a tool call originates from a subagent (i.e. parent_tool_use_id is set) THE SYSTEM SHALL skip the shadow memory risk probe — subagent execution is isolated from parent session accumulation. | must |
| FR-010 | WHEN `[sanitizer.shadow_memory].risk_assessment_provider` is set AND distinct from the main LLM provider THE SYSTEM SHALL route the risk probe to the specified provider (e.g. fast model for cost). | must |
| FR-011 | WHEN shadow memory is enabled AND a block is triggered THE SYSTEM SHALL NOT retry the tool call regardless of `is_tool_retryable`. Set `error_category = "shadow_memory_alert"`, `error_domain = "security"`. | must |

### SafeHarbor Guardrail Memory Tree

| ID | Requirement | Priority |
|----|------------|----------|
| FR-012 | WHEN an agent starts AND `[sanitizer.guardrail].hierarchical_memory_enabled = true` THE SYSTEM SHALL load the persistent guardrail tree from SQLite (or create an empty tree if no prior sessions). | must |
| FR-013 | WHEN the agent prepares to invoke an LLM call AND guardrail memory is enabled THE SYSTEM SHALL traverse the guardrail tree, select rules matching current request category, and prepend them to the system prompt BEFORE the LLM call. | must |
| FR-014 | WHEN a guardrail classification fires (i.e. `GuardrailFilter::classify` returns Unsafe) THE SYSTEM SHALL record the decision (tool name, arguments, guardrail rule ID, verdict Safe/Unsafe) in `guardrail_decisions` table. | must |
| FR-015 | WHEN the system has recorded >= `[sanitizer.guardrail].decisions_per_evolution` classifications (default 50) THE SYSTEM SHALL invoke SafeHarbor evolution: compute entropy for each tree node, split nodes with entropy > `entropy_split_threshold` (default 0.8), merge nodes with entropy < `entropy_merge_threshold` (default 0.2). | must |
| FR-016 | WHEN splitting a tree node THE SYSTEM SHALL generate new sub-rules via the `boundary_provider` LLM, categorizing the decision set by observed tool/argument patterns. Store new nodes in tree with creation timestamp. | must |
| FR-017 | WHEN merging tree nodes THE SYSTEM SHALL consolidate their rules into parent, preserving rule precedence (higher-confidence rules shadow lower-confidence). | must |
| FR-018 | WHEN the guardrail tree evolves (split/merge) THE SYSTEM SHALL record an `EvolutionEvent` with node ID, action (split/merge), entropy before/after, timestamp. | must |
| FR-019 | WHEN a request matches multiple tree rules THE SYSTEM SHALL apply highest-priority rule (ranked by entropy-adjusted confidence). Tie-breaking: most recent rule wins. | must |
| FR-020 | WHEN `[sanitizer.guardrail].boundary_provider` is configured AND distinct from main provider THE SYSTEM SHALL route SafeHarbor evolution LLM calls to the specified provider. | must |
| FR-021 | WHEN SafeHarbor evolution occurs THE SYSTEM SHALL emit an `EvolutionEvent` audit record with correlation_id linking back to the triggering guardrail classification decisions. | must |
| FR-022 | WHEN a new session starts THE SYSTEM SHALL load the guardrail tree from the persistent store (SQLite) — tree persists across sessions, unlike shadow memory which is session-scoped. | must |
| FR-023 | WHEN guardrail tree grows beyond `[sanitizer.guardrail].max_tree_nodes` (default 256) THE SYSTEM SHALL trigger cleanup: prune lowest-confidence leaf nodes, keeping only highest-evidence nodes. | must |

---

## 3. Architecture

### 3.1 MAGE Shadow Memory Data Model

```rust
/// Session-scoped shadow memory for multi-turn threat detection.
pub struct ShadowMemory {
    /// Accumulated risk score (0.0–1.0) across all turns.
    pub cumulative_risk_score: f32,
    
    /// Per-turn risk signals (max: max_turns_retained).
    pub turn_history: Vec<TurnSummary>,
    
    /// Timestamp of last risk probe.
    pub last_probe_at: SystemTime,
}

pub struct TurnSummary {
    /// Agent turn number (0-indexed).
    pub turn_idx: u32,
    
    /// Tool executed in this turn (empty if no tool call).
    pub tool_name: String,
    
    /// SHA256 hash of first 256 chars of tool args (sanitized, never plaintext).
    pub args_hash: String,
    
    /// Risk score from pre-action probe (0.0–1.0).
    pub risk_score: f32,
    
    /// Goal trajectory coherence rating (1–5, higher = more aligned with intent).
    pub coherence_rating: u8,
    
    /// Pattern frequency: count of same tool in last N turns.
    pub pattern_frequency: usize,
    
    /// Risk trend: acceleration (positive = increasing risk).
    pub risk_acceleration: f32,
    
    /// Timestamp of this turn.
    pub timestamp: SystemTime,
}

/// Config for shadow memory feature.
pub struct ShadowMemoryConfig {
    /// Enable/disable shadow memory accumulation.
    pub enabled: bool,
    
    /// Max turns to retain in shadow (default: 20).
    pub max_turns_retained: usize,
    
    /// Cumulative risk score threshold to trigger alert (default: 0.7).
    pub risk_threshold: f32,
    
    /// LLM provider for risk assessment (empty = use main provider).
    pub risk_assessment_provider: String,
    
    /// Pattern frequency threshold (default: 3 same tool in 5 turns).
    pub pattern_frequency_threshold: usize,
    
    /// Risk acceleration threshold (default: 0.15 per turn).
    pub risk_acceleration_threshold: f32,
}
```

### 3.2 SafeHarbor Guardrail Tree Data Model

```rust
/// Persistent hierarchical guardrail rule tree.
pub struct GuardrailTree {
    /// Root node (category = "root").
    pub root: Arc<RwLock<TreeNode>>,
    
    /// Mapping: rule_id → node for O(1) lookup.
    pub rule_index: Arc<RwLock<HashMap<String, Arc<RwLock<TreeNode>>>>>,
    
    /// Pending decisions awaiting evolution.
    pub pending_decisions: Arc<RwLock<Vec<GuardrailDecision>>>,
}

pub struct TreeNode {
    /// Unique node identifier (e.g., "root.input_validation.shell_injection").
    pub node_id: String,
    
    /// Category for this node (e.g., "input_validation", "tool_execution").
    pub category: String,
    
    /// Decision rules at this node (in priority order).
    pub rules: Vec<GuardrailRule>,
    
    /// Child nodes (subcategories).
    pub children: Vec<Arc<RwLock<TreeNode>>>,
    
    /// Parent node reference.
    pub parent: Option<Weak<RwLock<TreeNode>>>,
    
    /// Shannon entropy of decisions at this node (updated after evolution).
    pub entropy: f32,
    
    /// Confidence of this node's rules (0.0–1.0, based on decision accuracy).
    pub confidence: f32,
    
    /// Creation timestamp.
    pub created_at: SystemTime,
    
    /// Last evolution timestamp.
    pub last_evolved_at: Option<SystemTime>,
}

pub struct GuardrailRule {
    /// Unique rule identifier (e.g., "rule_12345_shell_injection_v1").
    pub rule_id: String,
    
    /// Human-readable rule name (e.g., "Detect rm -rf commands").
    pub name: String,
    
    /// Regex or pattern to match against tool arguments.
    pub pattern: String,
    
    /// Rule verdict: Safe or Unsafe.
    pub verdict: RuleVerdict,
    
    /// Confidence of this rule (0.0–1.0, higher = more accurate).
    pub confidence: f32,
    
    /// Count of times this rule was applied.
    pub application_count: usize,
    
    /// Count of times this rule was accurate (matched expected verdict).
    pub accuracy_count: usize,
    
    /// True positive rate (accuracy_count / application_count).
    pub true_positive_rate: f32,
    
    /// Creation timestamp (used for tie-breaking).
    pub created_at: SystemTime,
}

pub enum RuleVerdict {
    Safe,
    Unsafe,
}

/// A single guardrail classification decision.
pub struct GuardrailDecision {
    /// Tool name that was classified.
    pub tool_name: String,
    
    /// First 256 chars of arguments (sanitized).
    pub arguments_sample: String,
    
    /// Rule ID that matched.
    pub rule_id: String,
    
    /// Guardrail verdict (Safe / Unsafe).
    pub verdict: RuleVerdict,
    
    /// Timestamp of decision.
    pub timestamp: SystemTime,
    
    /// Correlation ID for audit trail.
    pub correlation_id: String,
}

pub struct SafeHarborConfig {
    /// Enable/disable hierarchical guardrail memory.
    pub hierarchical_memory_enabled: bool,
    
    /// Decisions required before triggering evolution (default: 50).
    pub decisions_per_evolution: usize,
    
    /// Entropy threshold for splitting nodes (default: 0.8, range 0.0–1.0).
    pub entropy_split_threshold: f32,
    
    /// Entropy threshold for merging nodes (default: 0.2, range 0.0–1.0).
    pub entropy_merge_threshold: f32,
    
    /// Max tree nodes before cleanup (default: 256).
    pub max_tree_nodes: usize,
    
    /// LLM provider for guardrail boundary evolution (empty = use main provider).
    pub boundary_provider: String,
}
```

### 3.3 Subsystem Mapping

#### MAGE → `zeph-sanitizer` + `zeph-memory` + `zeph-agent-tools`

**Implemented in PR #4215.**

1. **`zeph-sanitizer`** — `ShadowMemory` component (`crates/zeph-sanitizer/src/shadow_memory.rs`):
   - `ShadowMemory::new()` — initialize at session start (VecDeque-backed turn history)
   - `ShadowMemory::record_turn()` — log `ShadowEvent` after tool execution
   - `ShadowMemory::cumulative_score()` — read current risk score
   - Stored in-memory (no persistence), evicted at session end
   - Goal drift detection: `jaccard_distance` on tool-name sets, permission escalation pattern, deviation ratio
   - `GoalDriftResult` carries score, flags, and the triggering event summary
   - `classify_tool_permission()` maps tool names to permission-level tiers for escalation detection

2. **`zeph-memory`** — `shadow_memory` SQLite table reserved for v2 cross-session fingerprinting;
   not written in v1 (session-scoped only)

3. **`zeph-agent-tools`** — pre-action risk probe hook:
   - `pre_action_risk_probe()` — called before `ToolExecutor::execute`
   - Input: tool name, args summary, accumulated risk score
   - Output: new risk score, threat level, block/allow decision
   - Routes to `risk_assessment_provider` if configured

#### SafeHarbor → `zeph-sanitizer::GuardrailFilter` upgrade + `zeph-memory` persistence

1. **`zeph-sanitizer::GuardrailFilter`** — upgrade from static prompt to memory-backed:
   - Old: bundled regex/LLM patterns in static prompt
   - New: traverse `GuardrailTree`, inject dynamic rules per request
   - `classify_with_tree()` — new method taking tree reference
   - Rules matched in priority order (entropy-adjusted confidence)

2. **`zeph-memory`** — new tables:
   - `guardrail_tree_nodes` — persisted tree structure
   - `guardrail_rules` — rule definitions with confidence
   - `guardrail_decisions` — classification history for evolution
   - `guardrail_evolution_events` — tree split/merge history

3. **`zeph-core`** — evolution scheduling:
   - After each guardrail classification, check pending decision count
   - If >= `decisions_per_evolution`: invoke `SafeHarborEvolution::evolve()`
   - Evolution runs synchronously in agent loop (no background task)

---

## 4. Configuration Schema

### MAGE Shadow Memory Config

```toml
[sanitizer.shadow_memory]
# Enable shadow memory accumulation across turns.
enabled = true

# Max turns to retain in shadow history (default: 20).
max_turns_retained = 20

# Cumulative risk threshold to trigger alert (0.0–1.0, default: 0.7).
risk_threshold = 0.7

# LLM provider for risk assessment. Empty = use main LLM provider.
risk_assessment_provider = "fast"

# Pattern frequency threshold: trigger if same tool appears N+ times in last M turns.
# Format: "N:M" (e.g., "3:5" = alert if same tool 3+ times in last 5 turns).
pattern_frequency_threshold = "3:5"

# Risk acceleration threshold: alert if risk score increases by this much per turn.
risk_acceleration_threshold = 0.15
```

### SafeHarbor Guardrail Tree Config

```toml
[sanitizer.guardrail]
# Enable hierarchical guardrail memory tree evolution.
hierarchical_memory_enabled = true

# Number of decisions before triggering tree evolution (default: 50).
decisions_per_evolution = 50

# Entropy threshold for splitting tree nodes (0.0–1.0, default: 0.8).
# Nodes with entropy > this are split into child nodes.
entropy_split_threshold = 0.8

# Entropy threshold for merging tree nodes (0.0–1.0, default: 0.2).
# Nodes with entropy < this are merged with parent.
entropy_merge_threshold = 0.2

# Max tree nodes before cleanup (default: 256).
max_tree_nodes = 256

# LLM provider for guardrail rule evolution. Empty = use main LLM provider.
boundary_provider = "fast"
```

---

## 5. Key Invariants

**Always:**

- Shadow memory stores ONLY sanitized summaries: tool name, arg hash, risk score.
  Never raw user messages, raw tool outputs, or plaintext arguments.
- Risk assessment probe runs synchronously BEFORE every tool execution.
  No background/async risk computation that could race with tool output.
- SafeHarbor tree persists across sessions (unlike shadow memory which is session-scoped).
  Tree is loaded at session start from SQLite, saved after evolution.
- Both mechanisms are opt-in via config. Default: disabled. Existing behavior unchanged
  when features are off.
- Shadow memory is NOT exposed via any API, tool output, user-facing message, or audit log.
  It is internal-only (only visible in debug logs at TRACE level).
- Pre-action risk probe fires EVEN IF the tool is exempt from other security checks
  (e.g., `/code` shell tool still gets risk probed).
- If shadow memory triggers a block, the tool executor SHALL NOT retry the call.

**Never:**

- NEVER store raw user message content in shadow memory (violates input confidentiality).
- NEVER skip the pre-action risk probe when shadow memory is enabled,
  even for "safe" tools like `/code ls`.
- NEVER allow SafeHarbor tree rules to contain model-specific prompts or jailbreak-like text.
  Rules must be model-agnostic decision statements (e.g., "block rm -rf" not "pretend you are...").
- NEVER merge shadow memory with main conversation memory store.
  Shadow memory is a separate, session-scoped, internal-only buffer.
- NEVER expose cumulative risk score, threat level, or shadow memory state to the user
  (via chat, tools, API, or debug output).
- NEVER persist shadow memory to disk. Evict at session end.
- NEVER split/merge SafeHarbor nodes outside the evolution loop.
  All tree mutations go through `SafeHarborEvolution::evolve()`.

---

## 6. Relationship to Existing Security Work

### VIGIL (010-6-vigil-intent-anchoring)

- **VIGIL** = per-turn, low-latency regex tripwire that blocks injection patterns in tool outputs
- **MAGE Shadow Memory** = across-turn accumulator that detects attack *patterns* (e.g. repeated
  tool manipulation) rather than single injections
- **Complementary**: VIGIL catches immediate injection attempts; MAGE detects sophisticated
  multi-turn sequences that individually pass VIGIL checks

### ContentSanitizer (040-sanitizer)

- **ContentSanitizer** = defense-in-depth primary layer with spotlighting, PII redaction,
  exfiltration guards
- **SafeHarbor** = memory-backed guardrail refinement that reduces false positives on
  context-specific edge cases
- **Complementary**: ContentSanitizer provides broad protection; SafeHarbor provides targeted
  precision tuning

### TrajectorySentinel (050-security-capability-governance)

- **TrajectorySentinel** = multi-turn risk accumulation with decay for capability governance
- **MAGE Shadow Memory** = multi-turn accumulation for cross-turn threat pattern detection
- **Distinct**: TrajectorySentinel focuses on *capability* (what actions are allowed);
  MAGE focuses on *threat* (is this sequence malicious?)
- **May collaborate**: Both can feed into a unified risk dashboard (future P2)

### Audit Trail (010-4-audit)

- **Audit Trail** = immutable log of all tool executions, decisions, and security events
- **Shadow Memory** = temporary session-scoped buffer for real-time threat detection
- **Relationship**: When shadow memory triggers a block, that block is recorded in audit trail
  (via `SecurityEvent` + `AuditEntry`). Shadow memory itself is NOT part of the audit trail.

---

## 7. Acceptance Criteria

### MAGE Shadow Memory

- [ ] Shadow memory initializes empty at session start when enabled
- [ ] Pre-action risk probe fires before every tool execution (except subagents)
- [ ] Cumulative risk score accumulates correctly across turns (0.0 ≤ score ≤ 1.0)
- [ ] Risk score resets to 0.0 when session ends or `/clear` is invoked
- [ ] Turn history truncated to max_turns_retained (no memory leak)
- [ ] Pattern frequency detected: same tool called 3+ times in 5 turns raises score
- [ ] Goal coherence rating obtained from LLM (1–5 scale) and logged
- [ ] Risk acceleration computed as trend over last 5 turns
- [ ] Block triggered when cumulative_risk_score > risk_threshold
- [ ] Blocked tool NOT retried (error_category = "shadow_memory_alert")
- [ ] Subagent tool calls skip shadow memory probe (isolated)
- [ ] `risk_assessment_provider` routing works: risk probes go to fast model when configured
- [ ] Turn summary contains ONLY: tool name, arg hash (SHA256), risk score, coherence rating.
  NO raw args, NO raw output
- [ ] Test scenario: 10-turn goal hijacking detected before irreversible action
  (e.g., detect pattern of "ask for sensitive file" → "ask for weaker condition" → "ask for info leak")
- [ ] Manual audit of debug logs confirms no raw user content in shadow memory dumps

### SafeHarbor Guardrail Tree

- [ ] Guardrail tree loads from SQLite at session start (or creates empty tree)
- [ ] Tree persists across sessions (checkpoint after evolution)
- [ ] Rules injected into system prompt before each LLM call
- [ ] Guardrail classification decisions recorded in guardrail_decisions table
- [ ] Evolution triggered after decisions_per_evolution (default: 50) classifications
- [ ] Node splitting: entropy > split_threshold generates new sub-rules via LLM
- [ ] Node merging: entropy < merge_threshold consolidates rules into parent
- [ ] Evolution events recorded with correlation_id linking to triggering decisions
- [ ] Rule priority applied correctly: entropy-adjusted confidence ranked
- [ ] `boundary_provider` routing works: evolution LLM calls go to fast model when configured
- [ ] Tree cleanup triggered at max_tree_nodes: prune lowest-confidence leaves
- [ ] False positive rate on legitimate edge cases measurably lower than static baseline
  (measure: guardrail allow-rate increase on benign patterns after 1–2 evolution cycles)
- [ ] Evolution does NOT modify existing rule confidence values retroactively;
  only creates new nodes/rules
- [ ] Persistent state schema matches declared tables in zeph-memory

---

## 8. Implementation Notes

### MAGE Shadow Memory Storage Strategy

**v1: In-memory only (recommended)**

- Initialize `ShadowMemory` as part of agent session state
- Store `turn_history: Vec<TurnSummary>` in memory, evict on session end
- No SQLite table usage in v1 (avoids DB I/O on hot path)
- Rationale: session-scoped threat detection does not require persistence

**v1.5: Optional SQLite for future cross-session fingerprinting**

- Reserve `shadow_memory_sessions` and `shadow_memory_turns` tables for future use
- Do NOT write to these tables in v1 (no persistence)
- Future v2 can populate tables for anonymized threat fingerprinting

**Risk Probe Implementation**

- `pre_action_risk_probe()` is a synchronous function, not async
- Input: tool name, args summary (first 256 chars, sanitized), accumulated risk score
- Output: (new_risk_score: f32, threat_level: ThreatLevel, action: ProbeAction)
- Calls to `risk_assessment_provider` are async but must complete synchronously
  within tool execution phase (use `.await` in agent loop, not in background)

### SafeHarbor Guardrail Tree Storage Strategy

**Persistent Storage: SQLite**

Create three tables in `zeph-memory`:

```sql
CREATE TABLE guardrail_tree_nodes (
    node_id TEXT PRIMARY KEY,
    parent_node_id TEXT,
    category TEXT NOT NULL,
    entropy REAL,
    confidence REAL,
    created_at TIMESTAMP,
    last_evolved_at TIMESTAMP
);

CREATE TABLE guardrail_rules (
    rule_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES guardrail_tree_nodes(node_id),
    name TEXT,
    pattern TEXT,
    verdict TEXT,  -- "safe" | "unsafe"
    confidence REAL,
    application_count INTEGER,
    accuracy_count INTEGER,
    created_at TIMESTAMP,
    FOREIGN KEY(node_id) REFERENCES guardrail_tree_nodes(node_id)
);

CREATE TABLE guardrail_decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool_name TEXT,
    arguments_sample TEXT,
    rule_id TEXT,
    verdict TEXT,  -- "safe" | "unsafe"
    timestamp TIMESTAMP,
    correlation_id TEXT
);

CREATE TABLE guardrail_evolution_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT,
    action TEXT,  -- "split" | "merge"
    entropy_before REAL,
    entropy_after REAL,
    timestamp TIMESTAMP,
    correlation_id TEXT
);
```

**In-Memory Representation: Arc<RwLock<TreeNode>>**

- Tree is loaded at session start from SQLite
- Mutations (split/merge) are applied in-memory during evolution
- After evolution completes, checkpoint tree back to SQLite
- Rationale: RwLock allows concurrent reads during LLM calls + writes during evolution

**Evolution Scheduling**

- After guardrail classification, add decision to `pending_decisions`
- In agent loop tail (after LLM response but before commit-to-memory):
  check if `pending_decisions.len() >= decisions_per_evolution`
- If yes, invoke `SafeHarborEvolution::evolve()` synchronously
- Evolution reads pending decisions, invokes LLM via `boundary_provider` for new rules,
  updates tree in-memory, checkpoints to SQLite, clears pending decisions

**Rule Injection into System Prompt**

- Before invoking LLM, traverse guardrail tree
- Select rules matching current request (category match or wildcard rules)
- Prepend selected rules to system prompt in priority order (highest confidence first)
- Inject as plain text, not as tool definitions or special sections
- Example format:
  ```
  [GUARDRAIL RULES]
  - Block: rm -rf patterns (confidence: 0.95)
  - Block: private key exposure patterns (confidence: 0.88)
  ...
  
  [ORIGINAL SYSTEM PROMPT]
  You are a helpful AI assistant...
  ```

---

## 9. NEVER Section

These constraints are **non-negotiable** — violation is a spec violation, not a code review note:

1. **NEVER** store raw user message content in shadow memory.
   - Violates confidentiality and defeats the purpose of memory summary.
   - Only store tool name, hashed args, risk scores.

2. **NEVER** skip the pre-action risk probe, even for "safe" tools.
   - Risk probe is the detection mechanism; skipping any tool defeats it.
   - Exception: subagent tool calls (FR-009).

3. **NEVER** allow SafeHarbor rules to contain model-specific jailbreak text.
   - Rules must be decision statements ("block X pattern"), not instructions ("pretend you are...").
   - Rules must work across any LLM, not just one model.

4. **NEVER** merge shadow memory with main conversation memory.
   - Shadow memory is isolation-critical; leaking into conversation enables injection.
   - Use separate data structures.

5. **NEVER** expose shadow memory state to user or tools.
   - Risk score, threat level, cumulative metric = internal only.
   - Logging at TRACE level only (not INFO, DEBUG, or user-visible channels).

6. **NEVER** persist shadow memory across sessions.
   - Session-scoped buffer; evict at session end.
   - (Future v2 may add persistence for anonymized fingerprinting, but v1 does NOT.)

7. **NEVER** split/merge SafeHarbor nodes outside the evolution loop.
   - All tree mutations are controlled by `SafeHarborEvolution::evolve()`.
   - Manual node mutations = spec violation.

8. **NEVER** allow guardrail tree to grow unbounded.
   - Enforce max_tree_nodes; trigger cleanup when exceeded (prune lowest-confidence leaves).

9. **NEVER** apply rule confidence retroactively.
   - Rule confidence is a static metric (application_count, accuracy_count, true_positive_rate).
   - Evolution creates *new* rules with computed confidence, does not modify existing rule metrics.

10. **NEVER** use blocking I/O (e.g., disk fsync) in the risk probe hot path.
    - Risk probe must complete in <100ms to avoid perceptible latency.
    - Use async/await for any I/O (e.g., LLM calls to risk_assessment_provider).

---

## 10. Testing & CI Integration

### Manual Test Scenarios (Playbook: `.local/testing/playbooks/shadow-memory-guardrail.md`)

**Scenario 1: MAGE 10-Turn Goal Hijacking**
1. Start session with `shadow_memory_enabled = true`, `risk_threshold = 0.7`
2. Issue initial intent: "List the files in /home"
3. User (attacker) sends 10 turns of progressively manipulative tool outputs:
   - Turn 1: Tool returns "interesting /root/secrets.txt found"
   - Turn 2: Tool returns "would you like to examine /root/secrets.txt?"
   - ...
   - Turn 10: Tool returns output suggesting file exfiltration
4. Verify: cumulative risk score accumulates from ~0.3 per turn to > 0.7 by turn 8
5. Verify: MAGE triggers block on turn 8–9, BEFORE agent commits to exfil action
6. Verify: subsequent tool call blocked, error_category = "shadow_memory_alert"
7. Verify: no raw user content in debug logs from shadow memory dumps

**Scenario 2: SafeHarbor Rule Evolution**
1. Start fresh session with `hierarchical_memory_enabled = true`
2. Guardrail tree loaded empty (first session)
3. Submit 50+ benign tool calls that trigger guardrail checks
4. After 50 decisions, evolution fires:
   - LLM generates new sub-rules based on decision patterns
   - Tree grows from 1 node (root) to 3–5 nodes
5. Submit next batch of similar benign calls
6. Verify: allow-rate increases (false positive reduction) due to evolved rules
7. Verify: guardrail_tree_nodes and guardrail_rules tables contain new nodes/rules
8. Restart session: verify tree reloaded from SQLite and rule injection still works

**Scenario 3: Pattern Frequency Detection**
1. Set `pattern_frequency_threshold = "3:5"` (alert if same tool 3+ times in 5 turns)
2. Call `shell_execute` tool 3 times in 5 turns with different args
3. Verify: pattern_frequency recorded in shadow memory
4. Verify: risk score incremented on third occurrence
5. Verify: if risk_acceleration also high, cumulative score crosses threshold

### CI Integration

- Shadow memory and SafeHarbor testing is **read-only** — CI does not modify config or state
- CI test session uses `.local/config/testing.toml` with shadow_memory_enabled and guardrail enabled
- CI playbook includes:
  - Turn-by-turn risk score log inspection
  - Entropy computation verification for 2–3 evolution cycles
  - No raw user content in shadow memory dumps (grep for "user message" in shadow dumps)
  - False positive rate comparison (baseline static prompt vs. evolved guardrails)

### Coverage Table Entry

Add to `.local/testing/coverage-status.md`:

```markdown
| Feature | Component | Status | Last Session | Notes |
|---------|-----------|--------|--------------|-------|
| MAGE Shadow Memory | zeph-sanitizer, zeph-agent-tools | Untested | — | Multi-turn goal hijacking detection; playbook: shadow-memory-guardrail.md |
| SafeHarbor Guardrail Tree | zeph-sanitizer, zeph-memory | Untested | — | Entropy-based rule evolution; playbook: shadow-memory-guardrail.md |
```

---

## 11. Spec Compliance Checklist

Before implementation PR:

- [ ] Read `/specs/001-system-invariants/spec.md` and confirm no violations
- [ ] Read `/specs/010-security/spec.md` (parent) and confirm alignment
- [ ] Ensure `risk_assessment_provider` and `boundary_provider` follow multi-model pattern
  (matching `/specs/024-multi-model-design/spec.md`)
- [ ] Config schema added to `zeph-config/src/security.rs` and `config.toml` template
- [ ] SQLite table schemas created in `zeph-db` migrations
- [ ] Rustdoc added to all pub items with examples
- [ ] Doc-tests pass: `cargo test --doc -p zeph-sanitizer`
- [ ] Playbook created: `.local/testing/playbooks/shadow-memory-guardrail.md`
- [ ] Coverage row added: `.local/testing/coverage-status.md`
- [ ] Pre-commit checks pass:
  ```bash
  cargo +nightly fmt --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo nextest run --workspace --all-features --lib --bins
  ```
- [ ] Live test completed: 10-turn goal hijacking scenario passes
- [ ] CHANGELOG.md updated with feature description
