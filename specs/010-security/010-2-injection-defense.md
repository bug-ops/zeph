---
aliases:
  - Injection Defense
  - IPI Protection
  - DeBERTa Injection Detection
  - AlignSentinel
  - PII NER Detection
tags:
  - sdd
  - spec
  - security
  - classifiers
  - contract
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-3-authorization]]"
  - "[[010-4-audit]]"
  - "[[025-classifiers/spec]]"
---

# Spec: Injection Defense & Content Isolation

Indirect Prompt Injection (IPI) defense, regex-based detection, ML classifier soft signals, TurnCausalAnalyzer, PII NER detection, content spotlighting.

## Overview

Zeph processes untrusted content from web scraping, MCP tool outputs, A2A calls, and memory retrieval. IPI attacks attempt to override agent behavior by embedding instructions in observed content. Zeph defends via multi-layer detection: regex spotlighting with content source attribution, optional ML-backed classifiers, turn-level causal analysis for tool-chain anomalies, and PII NER redaction.

## Key Invariants

**Always:**
- All untrusted content (`WebScrape`, `MemoryRetrieval`, `A2A`, `McpToolResult`) wrapped in spotlighting delimiters and source attribution
- External content injected at `ContentTrustLevel::ExternalUntrusted` or `LocalUntrusted` depending on source
- Regex-based injection pattern detection runs unconditionally on external content (`flag_injection_patterns = true`)
- PII entities truncated before ML model inference (max 4096 chars) to prevent OOM
- Secrets masked via `SecretMaskRegistry` at LLM boundary to prevent leakage

**Never:**
- Run IPI ML classifiers on agent-generated content — only on external/tool-sourced content
- Accumulate cross-turn injection signals for confirmation decisions — `CrossToolCorrelator` clears state at turn boundaries
- Log or expose redacted PII values or secret shapes in debug output
- Skip spotlight wrapping for untrusted content even if regex confidence is low

## Content Sanitizer: Spotlighting & Injection Detection

`ContentSanitizer` (`crates/zeph-sanitizer/src/sanitizer.rs`) wraps untrusted content with source attribution and scans for regex injection patterns:

```rust
pub struct ContentSanitizer { /* ... */ }

impl ContentSanitizer {
    /// Sanitize external content: spot-light, truncate, flag injection patterns.
    pub fn sanitize(
        &self,
        text: &str,
        source: ContentSource,
    ) -> SanitizedContent {
        // 1. Apply content trust level based on source.kind
        // 2. Truncate if > max_content_size
        // 3. If external + untrusted: spotlight with <external-data>...</external-data>
        // 4. Scan for regex injection patterns; populate injection_flags
        // 5. Return wrapped content ready for LLM context
    }
}

pub struct SanitizedContent {
    pub source: ContentSource,           // Source of the content (web, tool, MCP, etc.)
    pub body: String,                    // Spotlighted, truncated content
    pub injection_flags: Vec<InjectionFlag>,  // Detected pattern names
    pub was_truncated: bool,             // True if exceeded max size
}
```

**Trust levels** (`ContentTrustLevel`):
- `Trusted` — passes unchanged (system prompt, validated user input)
- `LocalUntrusted` — tool results from local executors; wrapped in `<tool-output>` with NOTE header
- `ExternalUntrusted` — web, MCP, A2A, memory; wrapped in `<external-data>` with IMPORTANT warning

## Optional ML-Backed Soft Signals

When feature `classifiers` is enabled and a backend is attached, `ContentSanitizer::classify_injection` provides DeBERTa-backed injection probability (advisory only):

```rust
impl ContentSanitizer {
    /// Optional: classify content with ML classifier (requires backend).
    pub async fn classify_injection(&self, text: &str) 
        -> Result<InjectionVerdict> {
        // DeBERTa model (via candle): returns continuous [0.0–1.0] probability
        // Policy-blocked outputs are skipped; ML classification only on advisory path
    }
}
```

**Note**: IPI detection is part of the `[security.content_isolation]` configuration and controlled via the `flag_injection_patterns` flag. There is no separate `[security.ipi]` section; injection detection runs unconditionally on external content.

## Turn-Level Causal Analysis

`TurnCausalAnalyzer` (`crates/zeph-sanitizer/src/causal_ipi.rs`) detects anomalous patterns in tool-call sequences within a turn:

```rust
pub struct TurnCausalAnalyzer { /* ... */ }

impl TurnCausalAnalyzer {
    /// Analyze whether a pair of (prior_response, current_response) shows causal anomalies.
    /// 
    /// Synchronous local computation (no LLM call). Returns a value with .is_flagged / .deviation_score fields.
    pub fn analyze(&self, pre_response: &str, post_response: &str) -> CausalAnalysis {
        // Local embedding-based comparison of prior response vs current response
        // Returns anomaly score if deviation is high
    }
}
```

This check runs **within the turn only** — state is cleared at turn boundaries per the parent spec's NEVER rule. A separate async LLM-backed method exists for probe generation but isn't what's described here.

## PII Detection & Redaction

`CandlePiiClassifier` (feature: `classifiers`) performs Named Entity Recognition (NER) on tool inputs/outputs when the feature is enabled:

```rust
impl ContentSanitizer {
    /// Optional: detect PII via NER model (requires feature + backend).
    pub async fn detect_pii(&self, text: &str) 
        -> Result<Vec<PiiEntity>> {
        // Truncate to pii_max_input_chars (default 4096) before model inference
        // NER inference: returns PII entities (SSN, credit card, email, phone, etc.)
        // Detected entities are redacted from final content
    }
}

pub struct PiiFilter { 
    regex_patterns: Vec<Regex>,  // Fallback: regex-based PII detection
}

impl PiiFilter {
    /// Regex-based PII scrubbing (email, phone, SSN, credit card).
    /// 
    /// Returns a `Cow<'a, str>` — if no patterns matched, returns a borrow of the original;
    /// if patterns matched, returns an owned String with scrubbed content.
    pub fn scrub<'a>(&self, text: &'a str) -> Cow<'a, str> {
        // Regex-based PII scrubbing (email, phone, SSN, credit card)
        // Runs unconditionally; always precedes ML classification
    }
}
```

Config:
```toml
[security.pii_filter]
enabled = true                     # Master switch for PII redaction (default: true)
filter_email = true                # Scrub email addresses
filter_phone = true                # Scrub US phone numbers
filter_ssn = true                  # Scrub US Social Security Numbers
filter_credit_card = true          # Scrub credit card numbers
filter_names = false               # Scrub personal names via heuristic (opt-in, default: false)
# custom_patterns = []             # Custom regex patterns on top of built-ins
```

## Secret Shape Masking

`SecretMaskRegistry` (`crates/zeph-sanitizer/src/secret_mask.rs`) masks vault-secret placeholders at the LLM boundary to prevent leakage via side-channel analysis:

```rust
pub enum SecretCategory {
    ApiKey,
    Token,
    Password,
    Certificate,
    Webhook,
    Generic,
}

pub struct SecretMaskRegistry { /* ... */ }

impl SecretMaskRegistry {
    /// Mask all vault secret references in text before LLM inference.
    pub fn mask(&self, text: &str) -> String {
        // Replaces secret values with [MASKED_<category>] placeholders
        // Preserves schema/structure for debugging
    }
}
```

## Guardrail Filter (Optional Gating)

`GuardrailFilter` provides an optional LLM-based pre-screener at the input boundary. When external content passes through, it is classified as SAFE/UNSAFE via Llama Guard classifier before being added to context:

```rust
pub struct GuardrailFilter { /* ... */ }

impl GuardrailFilter {
    /// Screen content via LLM guardrail classifier before adding to context.
    pub async fn screen(&self, text: &str) 
        -> Result<GuardrailVerdict> {
        // Returns SAFE, UNSAFE, or UNKNOWN with confidence
        // High-confidence UNSAFE blocks content from context
    }
}
```

Config:
```toml
[security.guardrail]
enabled = false                    # Enable LLM-based guardrail classifier (default: false)
# provider = "ollama"              # LLM provider for guardrail calls
# model = "llama-guard-3:1b"       # Model to use for classification
timeout_ms = 500                   # Timeout for each guardrail call (milliseconds)
action = "block"                   # Action on flagged content: "block" or "warn"
fail_strategy = "closed"           # On timeout/LLM error: "open" (allow) or "closed" (block)
scan_tool_output = false           # Scan tool outputs before context (default: false)
max_input_chars = 4096             # Max chars sent to guard model (default: 4096)
```

## Quarantine & Dual-LLM Extraction

`QuarantinedSummarizer` (`crates/zeph-sanitizer/src/quarantine.rs`) applies a Dual-LLM approach: one LLM processes untrusted content in isolation; a second LLM summarizes into trusted context. This prevents the agent from seeing potentially malicious instructions directly.

## Integration Points

- [[008-mcp/spec]] — MCP tool sanitization via `sanitize_tools()`
- [[025-classifiers/spec]] — DeBERTa (IPI), Candle PII NER (optional classifiers feature)
- [[010-4-audit]] — Audit signals (`AuditSignal`, `AuditSignalType`) ingested by `TrajectoryRiskAccumulator`
- WebScrape tool — Content sanitized before returning
- Context assembly — Spotlight wrapping applied per source trust level

## See Also

- [[010-security/spec]] — Parent; cross-turn NEVER rule for `CrossToolCorrelator`
- [[010-1-vault]] — Prevent secret leakage via masking
- [[025-classifiers/spec]] — Classifier infrastructure (DeBERTa, PII NER)
