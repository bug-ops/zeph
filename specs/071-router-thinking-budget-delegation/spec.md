---
aliases:
  - Router Thinking Budget Delegation
  - Provider Delegation Advisory
tags:
  - sdd
  - spec
  - routing
  - llm
  - thinking-budget
created: 2026-07-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[003-llm-providers/spec]]"
  - "[[023-complexity-triage-routing/spec]]"
---

# Spec: Router Thinking Budget Delegation

> [!abstract]
> When commands like `/think-tokens` and `/reasoning-effort` mutate capabilities on a routed provider pool (Router or Triage),
> the target provider for mutation is determined by `capability_target_index()`.
> On re-sampling strategies (Ema, Thompson, Bandit), the next dispatch may select a different provider than the one just mutated.
> This spec documents the delegation logic, target resolution, and advisory behavior to alert users of this mismatch.

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-llm/src/any.rs` | `AnyProvider::set_thinking_budget`, `apply_reasoning_effort`, `current_thinking_budget`, `current_reasoning_effort`, `capability_delegation_advisory` |
| `crates/zeph-llm/src/router/builder.rs` | `Router::set_thinking_budget_delegated`, `apply_reasoning_effort_delegated`, `current_thinking_budget_delegated`, `current_reasoning_effort_delegated`, `capability_delegation_advisory`, `capability_target_index` |
| `crates/zeph-llm/src/router/triage.rs` | `Triage::set_thinking_budget_delegated`, `apply_reasoning_effort_delegated`, `current_thinking_budget_delegated`, `current_reasoning_effort_delegated`, `capability_delegation_advisory`, `capability_target_index` |

---

## 1. Overview

When a user mutates provider capabilities (thinking token budget or reasoning effort level) on an `AnyProvider::Router` or `AnyProvider::Triage`:

1. The command identifies a **target provider index** (which inner provider to configure)
2. The mutation is applied **in place** to that provider
3. An advisory is returned if the next dispatch may pick a **different provider** than the one just configured

This is a user experience feature: the advisory warns "you configured X, but the next turn may use Y, so check your settings".

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-005 | WHEN a capability mutation (set_thinking_budget, apply_reasoning_effort) is called on Router/Triage WITH a multi-provider pool AND a re-sampling strategy (Ema, Thompson, Bandit) THEN `capability_delegation_advisory()` SHALL return Some(advisory_string) | must |
| FR-005b | WHEN `capability_delegation_advisory()` is called on non-routed provider types (Claude, OpenAI, Ollama, etc.) THEN SYSTEM SHALL return None | must |
| FR-005c | WHEN `capability_delegation_advisory()` is called on Router/Triage WITH single-provider pool (len ≤ 1) OR deterministic strategy (Cascade) THEN SYSTEM SHALL return None | must |
| FR-006 | WHEN `capability_target_index()` returns None (no applicable provider found) or triage times out THEN SYSTEM SHALL use default index 0 (first/cheapest tier) | must |
| FR-007 | WHEN mutating a provider in the Arc<Vec<Provider>> pool via `with_target_provider_mut()` THEN SYSTEM SHALL use `Arc::make_mut()` to rebuild Arc on write, ensuring the mutation is visible to subsequent dispatches | must |
| FR-008 | WHEN `current_thinking_budget()` or `current_reasoning_effort()` is called on Router/Triage THEN SYSTEM SHALL return the value from the applicable target provider (as identified by `capability_target_index()`) | must |
| FR-009 | The advisory string SHALL include the name of the target provider and the routing strategy name (e.g., "applied to claude-sonnet; routing=Thompson may select a different provider on the next turn") | should |

---

## 3. Data Model

### Capability Target Selection (`capability_target_index()`)

Returns `Option<usize>` — the index into `self.tier_providers` (Triage) or `self.state.providers` (Router) that should be configured.

#### Router Implementation
- Looks up the name of the provider that served the most recent call (`self.state.last_active_provider`)
- Searches the pool for the provider matching that name via `position(|p| p.name() == name)`
- Falls back to index 0 if:
  - No dispatch has happened yet this session (name is `None`)
  - The provider is no longer in the pool (config drift; `position()` returns `None`)
- Returns `None` only if `self.state.providers` is empty

#### Triage Implementation
- Reads the cached tier provider index from the most recently **completed** request (`self.last_provider_idx`, an `AtomicUsize`)
- Falls back to `default_index` (0) only when the sentinel value `NO_LAST_PROVIDER` is set (i.e., no request has completed yet this session)
- Returns `None` only if `self.tier_providers` is empty (unreachable in practice — `new()` panics on empty pools)

### Mutation via Arc Rebuilding

The method `with_target_provider_mut()` handles Arc mutation safely:

```rust
fn with_target_provider_mut<F, R>(&mut self, idx: usize, f: F) -> R
where
    F: FnOnce(&mut AnyProvider) -> R,
{
    if Arc::strong_count(&self.state.providers) == 1 {
        // No other Arc copies; mutate in place
        f(&mut self.state.providers[idx])
    } else {
        // Shared Arc; clone Vec, mutate, rebuild Arc
        let mut v: Vec<_> = self.state.providers.iter().cloned().collect();
        let out = f(&mut v[idx]);
        self.state.providers = Arc::from(v);
        out
    }
}
```

This ensures mutations are **immediately visible** to the next dispatch.

### Advisory Logic

```rust
// Router
if self.state.providers.len() <= 1 || self.strategy == RouterStrategy::Cascade {
    None  // Deterministic or single provider; no warning needed
} else {
    Some(format!(
        "applied to {name}; routing={:?} may select a different provider on the next turn",
        self.strategy
    ))
}

// Triage
if self.tier_providers.len() <= 1 {
    None  // Single tier; no ambiguity
} else {
    Some(format!(
        "applied to {name}; routing=triage may select a different provider on the next turn"
    ))
}
```

---

## 4. Key Invariants

### INV-1: Mutation Visibility
When a capability is mutated on a target provider, the mutation **must** be visible to the next dispatch.
- **Mechanism**: `with_target_provider_mut()` uses `Arc::make_mut()` to ensure isolation on write.
- **Consequence**: Users see their `/think-tokens` or `/reasoning-effort` command take effect immediately.

### INV-2: Default Fallback Behavior
When no target provider can be identified (pool is empty or triage times out), always fall back to index 0.
- **Mechanism**: `capability_target_index()` returns `None`; calling code defaults to `Some(0)` or skips the mutation.
- **Consequence**: No panic on empty pools; graceful degradation.

### INV-3: Advisory Scope
An advisory is returned **if and only if**:
- The provider is routed (Router or Triage)
- The pool has more than one provider
- The strategy is non-deterministic (Ema, Thompson, Bandit) for Router, or multi-tier for Triage

Any other case returns `None` — the advisory is only useful when routing may pick a different provider on the next turn.

### INV-4: Delegation Does Not Fail
Setting a thinking budget or reasoning effort on Router/Triage does not fail due to routing strategy or pool composition.
- **Consequence**: The command always succeeds (or fails only if the inner provider doesn't support the capability).

### INV-5: Symmetry with Read Methods
`current_thinking_budget()` and `current_reasoning_effort()` read from the **same** target provider that mutation methods write to.
- **Mechanism**: Both use `capability_target_index()` to locate the provider.
- **Consequence**: Users see consistent get/set behavior: what you set is what you get (within the same turn).

---

## 5. Resolved Edge Cases

### §5.1: Multi-Provider Cascade Strategy
**Case**: Router with 5 providers, Cascade strategy (deterministic).

**User action**: `/think-tokens 8000`

**Behavior**:
1. Mutation applies to the cheapest provider (index 4)
2. `capability_delegation_advisory()` returns `None` (Cascade is deterministic)
3. Next turn: Cascade re-evaluates and may pick a different provider (e.g., if the first provider fails, cascade escalates)
4. **Result**: The thinking budget configured on provider 4 may not apply next turn (cascade may pick provider 0).
5. **Design choice**: No warning, because Cascade is designed to escalate on failure. Thinking config on the cheapest provider is advisory.

### §5.2: Ema-Routed Multi-Provider Pool
**Case**: Router with 3 providers, Ema strategy (re-sampling).

**User action**: `/reasoning-effort high`

**Behavior**:
1. Mutation applies to the target provider as determined by Ema state
2. `capability_delegation_advisory()` returns `"applied to <name>; routing=Ema may select a different provider on the next turn"`
3. Next turn: Ema re-evaluates and may pick a different provider based on reputation
4. **Result**: High reasoning effort is configured on provider X, but provider Y may be selected next
5. **User experience**: Warning alerts the user that their `/reasoning-effort` preference may not apply next turn

### §5.3: Triage Classification with Per-Tier Providers
**Case**: TriageRouter with simple→medium→expert tiers, each mapped to a different provider.

**User action**: `/think-tokens 12000` (when current triage class is "medium")

**Behavior**:
1. `capability_target_index()` returns the index of the medium-tier provider
2. Mutation applies to that provider
3. `capability_delegation_advisory()` returns `"applied to <medium_provider>; routing=triage may select a different provider on the next turn"`
4. Next turn: Triage re-classifies the input and may pick a different tier (e.g., simple or expert)
5. **Result**: Thinking budget is on the medium-tier provider; next turn may use a different tier provider
6. **User experience**: Advisory alerts the user that triage may select a different provider on the next turn

### §5.4: Single-Provider Pools (Degenerate Case)
**Case**: Router with 1 provider, any strategy.

**User action**: `/think-tokens 4000`

**Behavior**:
1. Mutation applies to the only provider
2. `capability_delegation_advisory()` returns `None` (single provider, so no ambiguity)
3. Next turn: The only provider is used (guaranteed)
4. **Result**: Thinking budget is applied; next turn uses the same provider
5. **Design choice**: No warning, because there's no routing ambiguity

### §5.5: Arc Rebuilding Under Contention
**Case**: Multiple Arc copies of the providers Vec exist (e.g., router cloned into a background task).

**User action**: `/think-tokens 6000` (on the main agent thread)

**Behavior**:
1. `with_target_provider_mut()` detects `Arc::strong_count() > 1`
2. Clones the entire `Vec<AnyProvider>`, mutates the target index, and rebuilds the Arc
3. Other Arc copies are unaffected (they still point to the old Vec)
4. **Result**: Mutation is visible to the main agent thread only; background tasks see the old state
5. **Design rationale**: Router is designed for the agent's main loop; sharing across threads is edge-case (see crate usage).

---

## 6. Integration Points

### AnyProvider Delegation
`AnyProvider` enum dispatches capability methods to Router/Triage:

```rust
pub fn set_thinking_budget(&mut self, budget: Option<u32>) -> Result<(), LlmError> {
    match self {
        Self::Router(r) => r.set_thinking_budget_delegated(budget),
        Self::Triage(t) => t.set_thinking_budget_delegated(budget),
        // ... other variants
    }
}

pub fn capability_delegation_advisory(&self) -> Option<String> {
    match self {
        Self::Router(r) => r.capability_delegation_advisory(),
        Self::Triage(t) => t.capability_delegation_advisory(),
        // ... other variants return None
    }
}
```

### Slash Command Layer (zeph-commands)
Commands like `/think-tokens N` and `/reasoning-effort <level>`:
1. Parse the user input
2. Call `provider.set_thinking_budget()` or `provider.apply_reasoning_effort()`
3. Retrieve advisory via `provider.capability_delegation_advisory()`
4. Display advisory to the user (if Some)

---

## 7. Testing Strategy

- **Unit tests**: `capability_delegation_advisory()` returns None for single-provider pools and Cascade strategy; returns Some for multi-provider + re-sampling
- **Integration tests**: Verify that mutations are visible to the next dispatch
- **Arc contention tests**: Verify that Arc rebuilding works correctly when strong_count > 1
- **Triage classification tests**: Verify that capability_target_index() returns the correct tier provider index

