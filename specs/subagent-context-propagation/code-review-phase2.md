# Code Review: Phase 2 Infrastructure (#2579, #2580, #2581)

**Date**: 2026-04-03  
**Verdict**: REQUEST CHANGES  
**Validators**: Security (PASS), Performance (PASS), Impl-Critic (7 issues), Tester (BLOCKING)

---

## Blocking Issues (must fix before merge)

### 1. #2581 Not Implemented

`mcp_tool_names` is still discarded (`_` prefix). The MCP annotation block is never appended to `system_prompt` in `run_agent_loop()`.

Required actions:
- Remove `_` prefix from `mcp_tool_names` destructuring
- Append MCP annotation block to `system_prompt` in `run_agent_loop()`
- Verify the 2 tests exist — they are currently missing from the codebase

### 2. Missing Tests

**#2579 — spawn depth guard:**
- Add `spawn_depth_zero_disables_all_spawning` test: `spawn_depth=0, max_spawn_depth=0 → rejected`

**#2580 — summary context injection:**
- Add timeout test: exercise the actual `tokio::time::timeout` `Elapsed` path
- Add fallback test: verify no panic/error when summary returns `None`
- Add budget truncation test: verify `last_assistant_turn` is dropped when context is too large

**Integration:**
- Add one integration test covering all three features together

---

## Medium Issues (should fix for code quality)

### 3. `spawn_depth` Missing from `AgentLoopArgs`

Spec says remove the `_` prefix to keep the field for future propagation tracking. Currently the field is removed entirely. It must be preserved in `AgentLoopArgs` even if unused at the call site today.

### 4. Summary Uses Default Provider Instead of Sub-Agent's Provider

Spec says summary uses the same provider as the sub-agent. Currently the default provider is used. Pass the sub-agent's resolved provider name to `summarize_parent_context()`.

### 5. No `warn!` Log on Summary Fallback

When `summarize_parent_context()` returns `None` (timeout, empty, error), a `warn!` log must be emitted so operators can diagnose silently skipped summaries.

---

## Low Issues (nice to have)

### 6. MCP Annotation Missing Guidance Text

The annotation block identifies MCP tools but provides no guidance on when/how to use them. Add a brief instructional sentence.

### 7. `summarize_parent_context` Includes System Messages

The function should filter to assistant-only messages (as the injection path already does). System messages in the summary prompt inflate tokens and add noise.

### 8. No Token Limit Check Before Summary Prompt

Large `parent_messages` can cause the summary prompt to explode. Add a token count check before passing to the LLM and truncate if over budget.

---

## Non-Blocking Recommendations (from Performance validator)

- Expose `summary_provider` config field so operators can assign a cheaper model
- Keep `last_assistant_turn` default as-is (current behavior acceptable)

---

## Summary Table

| # | Severity | Area | Issue |
|---|----------|------|-------|
| 1 | BLOCKING | #2581 | `mcp_tool_names` discarded, annotation not appended, 2 tests missing |
| 2 | BLOCKING | Tests | spawn_depth=0 test, timeout test, fallback test, budget test, integration test |
| 3 | MEDIUM | #2579 | `spawn_depth` removed from `AgentLoopArgs` instead of kept |
| 4 | MEDIUM | #2580 | Summary uses default provider, not sub-agent's provider |
| 5 | MEDIUM | #2580 | No `warn!` log on summary fallback |
| 6 | LOW | #2581 | MCP annotation missing guidance text |
| 7 | LOW | #2580 | `summarize_parent_context` includes system messages |
| 8 | LOW | #2580 | No token limit guard before summary prompt |
