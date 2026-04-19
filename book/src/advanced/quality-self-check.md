# MARCH Quality Self-Check

The MARCH (Multi-Agent Rational Consistency Hierarchy) self-check pipeline implements post-response factual consistency validation. After the LLM generates a response, two sub-agents automatically verify the response's claims: a Proposer decomposes the response into atomic verifiable assertions, and a Checker validates each assertion against retrieved context only — deliberately not seeing the original response to break confirmation bias.

This feature is opt-in and disabled by default.

## Why Factual Consistency Matters

LLMs excel at plausible-sounding prose but hallucinate specific facts, especially when:

- The context window does not include relevant information
- The query involves recent events or specialized domains
- Chain-of-thought reasoning contradicts earlier facts

MARCH detects these inconsistencies in real time, before the response is delivered to the user. Unlike batch evaluation tools that run offline, MARCH is synchronous and can flag problems immediately.

## How It Works

### Phase 1: Proposer

After the LLM generates a response, the Proposer sub-agent receives the response and breaks it into `max_assertions` independent, verifiable claims. For example:

**Response:** "The Paris office opened in 2019 and is currently managed by Sarah Chen."

**Proposed assertions:**
1. "The Paris office opened in 2019"
2. "Sarah Chen manages the Paris office"

Proposer uses the same LLM provider as the main response (respecting `--thinking` mode if active). The output must be valid JSON in a `claims` array.

### Phase 2: Checker

The Checker sub-agent receives only:

- The proposed assertions
- Retrieved context from memory (semantic recall, graph facts, session summaries)
- **NOT** the original response

For each assertion, the Checker answers: "Can this be confirmed from the context? Yes / No / Unclear."

If confidence is below `min_evidence` (default: 0.6), the assertion is flagged.

### Phase 3: Flagging

When `flag_marker` is set (default: `"--- MARCH CHECK"`), the response is appended with a marker line and a summary:

```text
[Original response here]

--- MARCH CHECK
Result: 2 assertions verified, 0 flagged, 0 unclear
Unconfirmed: [none]
```

If any assertion is flagged, it appears in the `Unconfirmed` list, alerting the user to review those specific claims.

## Configuration

Enable MARCH in the `[quality]` section:

```toml
[quality]
self_check = false                       # Enable MARCH self-check (default: false)
trigger = "always"                       # "always", "smart", or "manual" (default: "always")
latency_budget_ms = 5000                 # Per-turn budget in milliseconds (default: 5000)
per_call_timeout_ms = 3000              # Timeout per LLM call (default: 3000)
max_assertions = 10                      # Max claims extracted by Proposer (default: 10)
min_evidence = 0.6                       # Min confidence [0.0-1.0] for a claim (default: 0.6)
flag_marker = "--- MARCH CHECK"          # Marker appended to response (default: "--- MARCH CHECK")
```

### Trigger Strategies

| Trigger | Behavior |
|---------|----------|
| `always` | Run on every response |
| `smart` | Run on complex responses (multi-paragraph, multiple claims), skip simple acks |
| `manual` | Wait for explicit `/quality check` command |

The `smart` strategy uses heuristics: response length, sentence count, presence of numbers/dates, conditional statements. Lightweight responses ("yes", "no", "done") are skipped. Use `smart` to reduce latency on simple confirmations.

### Latency Budget

`latency_budget_ms` controls the total wall-clock time available for both Proposer and Checker calls. If either call exceeds `per_call_timeout_ms` (whichever is smaller), it times out and is retried once. If the second attempt also times out, the check is skipped with a warning.

The budget is per-turn; if a turn has multiple responses (e.g., streaming + final), only the final response is checked.

## Graceful Degradation

All errors are non-fatal:

- **Timeout:** warning logged, check skipped, response delivered
- **Parse error:** best-effort JSON recovery with fallback to empty assertions list
- **Provider error:** check skipped, response delivered
- **Qdrant unavailable:** context retrieval returns empty, all assertions marked "unclear"

The response is never withheld or degraded due to a check failure. The user always receives the original LLM response.

## Prompt Cache Integration

When using Claude with prompt caching enabled, Proposer and Checker calls suppress `cache_control` markers to prevent context leakage. This is transparent — no configuration needed. OpenAI (no cache_control field) has a documented no-op.

## Multi-Provider Consistency

MARCH uses the same provider stack as the main response:

- If the main response used `gpt-5.4`, Proposer and Checker use `gpt-5.4`
- If thinking mode is active (`--thinking extended:10000`), Proposer inherits the thinking budget
- If a provider is unavailable at check time, the check is skipped

This ensures consistency in reasoning and tone across the response and its verification.

## Disabling Per-Session

To skip checks for a specific session while `self_check = true` globally:

```bash
# No built-in flag to disable via CLI in MVP
# Workaround: use `trigger = "manual"` in config and omit `/quality` commands
```

This is planned for a future `--no-quality-check` CLI flag.

## Limitations & Future Work

**Current limitations:**

- `async_run = true` is reserved for future async integration but currently synchronous
- Daemon and ACP agent construction paths not yet wired (#TBD)
- No Ollama KV-cache suppression on Checker path

**Accepted trade-offs:**

- Doubles LLM calls on every response (when enabled)
- Proposer must output valid JSON (best-effort recovery on parse errors)
- Checker has no visibility into the original response (intentional asymmetry)

## See Also

- [Configuration Reference — Quality Section](../reference/configuration.md#quality)
- [Memory & Context — Cross-Session Recall](../concepts/memory.md#semantic-memory)
- [LLM Providers](../concepts/providers.md) — provider selection and routing
