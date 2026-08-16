# zeph-sanitizer

[![Crates.io](https://img.shields.io/crates/v/zeph-sanitizer)](https://crates.io/crates/zeph-sanitizer)
[![docs.rs](https://img.shields.io/docsrs/zeph-sanitizer)](https://docs.rs/zeph-sanitizer)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Content sanitization, exfiltration guard, PII filtering, and quarantine for Zeph — untrusted input isolation before LLM context injection.

## Overview

Implements a multi-stage security pipeline that processes all external data before it enters the LLM context window. The pipeline detects prompt injection patterns, wraps content in spotlighting XML delimiters, optionally routes high-risk sources through an isolated quarantine LLM call, and guards outbound paths against data exfiltration. Memory retrieval sources are classified via `MemorySourceHint` to suppress false positive injection flags on recalled user conversations and LLM-generated summaries.

## Key types

| Type | Description |
|------|-------------|
| `ContentSanitizer` | 4-step pipeline: truncate → strip control chars → detect injections → spotlighting XML wrap |
| `ContentTrustLevel` | `Trusted` / `LocalUntrusted` / `ExternalUntrusted` |
| `ContentSourceKind` | Source category (tool output, web scrape, document, etc.) |
| `SanitizedContent` | Output with `body`, `source`, `injection_flags`, and `was_truncated` |
| `InjectionFlag` | Detected injection pattern (`pattern_name`, `byte_offset`, `matched_text`) |
| `pii::PiiFilter` | Regex PII scrubber (email, phone, SSN, credit card; opt-in name heuristic) |
| `guardrail::GuardrailFilter` | LLM-based pre-screener at the input boundary |
| `quarantine::QuarantinedSummarizer` | Dual LLM pattern — routes high-risk content through an isolated, tool-less LLM call |
| `response_verifier::ResponseVerifier` | Post-LLM response scanner |
| `exfiltration::ExfiltrationGuard` | Three outbound guards: markdown image tracking, tool URL cross-validation, memory write suppression |
| `memory_validation::MemoryWriteValidator` | Structural write guards for the memory store |
| `causal_ipi::TurnCausalAnalyzer` | Behavioral deviation detection at tool-return boundaries |
| `nli::NliSanitizer` | Probabilistic NLI entailment check for injected instructions |
| `secret_mask::SecretMaskRegistry` | Vault-secret placeholder masking at the LLM boundary — masks *registered* secret values |
| `secret_shape::scrub_secret_shapes` | Shape-based redaction — catches strings that merely *look* like a secret (known API-key prefix, `Authorization: Bearer` header, standalone JWT, PEM private-key body, prefix-less AWS secret keys) even when the value was never registered with `SecretMaskRegistry` |
| `shadow_memory::ShadowMemory` | Goal-drift tracking (`GoalDriftResult`, `ShadowEvent`, `classify_tool_permission`) |
| `ipi_filter::IpiFilter` / `IpiVerdict` | Indirect prompt injection filter and verdict type |
| `ContentSource` | Source metadata with `ContentSourceKind` and optional `MemorySourceHint` for memory retrieval classification |
| `MemorySourceHint` | `ConversationHistory` / `LlmSummary` / `ExternalContent` — classifies memory retrieval sources to suppress false positive injection flags on recalled user text and LLM-generated summaries |
| `media::MediaSanitizer` | Validation pipeline for MCP-sourced images: magic-byte vs declared-MIME check, format allowlist, byte-size cap, and decoded-dimension/pixel caps (decompression-bomb defense) before an image is attached as a native `MessagePart::Image` |
| `media::MediaRejected` | Typed rejection reason (`SizeExceeded`, `DimensionExceeded`, `MimeMismatch`, `DecodeFailed`, format-not-allowed); the text placeholder always remains as a fallback |

## Architecture

The crate is a layered defense-in-depth pipeline; each layer is independently configurable and optional except layer 1:

| Layer | Type | Description |
|-------|------|-------------|
| 1 | `ContentSanitizer` | Regex-based injection detection + spotlighting |
| 2 | `pii::PiiFilter` | Regex PII scrubber (email, phone, SSN, credit card) |
| 3 | `guardrail::GuardrailFilter` | LLM-based pre-screener at the input boundary |
| 4 | `quarantine::QuarantinedSummarizer` | Isolated LLM fact extractor |
| 5 | `response_verifier::ResponseVerifier` | Post-LLM response scanner |
| 6 | `exfiltration::ExfiltrationGuard` | Outbound channel guards (markdown images, tool URLs) |
| 7 | `memory_validation::MemoryWriteValidator` | Structural write guards for the memory store |
| 8 | `causal_ipi::TurnCausalAnalyzer` | Behavioral deviation detection at tool-return boundaries |
| 9 | `nli::NliSanitizer` | Probabilistic NLI entailment check for injected instructions |
| 10 | `secret_mask::SecretMaskRegistry` | Vault-secret placeholder masking at the LLM boundary |

> [!NOTE]
> `media::MediaSanitizer` is a separate, image-specific validation pipeline for MCP tool-result passthrough (`[mcp.media]`) — it does not sit in the text-content layer chain above and is invoked directly by `zeph-mcp`'s tool executor when a server has `media_passthrough` enabled.

> [!NOTE]
> `secret_shape::scrub_secret_shapes` is likewise outside the layer chain — a stateless function
> called at the boundaries where secret-shaped text can appear without ever having been
> registered as a vault value (subagent result forwarding, compression guidelines, debug
> redaction). It complements layer 10: `SecretMaskRegistry` masks *known* secret values,
> `scrub_secret_shapes` catches anything merely *shaped* like one.

### Sanitization pipeline (layer 1 detail)

```
External data
    ↓ 1. Truncate to max_content_size
    ↓ 2. Strip null bytes and control characters
    ↓ 3. Detect 27 injection patterns (OWASP variants + encoding + exfiltration)
    ↓ 4. Wrap in spotlighting XML delimiters
        <tool-output>…</tool-output>       (local sources)
        <external-data>…</external-data>   (external sources)
```

## Usage

```rust
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};
use zeph_config::ContentIsolationConfig;

let config = ContentIsolationConfig::default();
let sanitizer = ContentSanitizer::new(&config);

let source = ContentSource::new(ContentSourceKind::WebScrape);
let result = sanitizer.sanitize("Hello world", source);

// result.body contains the wrapped, injection-cleaned text
// result.injection_flags contains any detected patterns (advisory — content is never removed)
for flag in &result.injection_flags {
    tracing::warn!("Injection detected: {}", flag.pattern_name);
}
```

## Configuration

```toml
[security.content_isolation]
enabled = true
max_content_size = 65536     # bytes; content truncated before injection detection

[security.content_isolation.quarantine]
enabled       = false   # default: false — opt-in
sources       = ["web_scrape", "a2a_message"]  # source kinds routed through quarantine
model         = "claude-haiku-4-5-20251001"    # optional; defaults to primary provider
timeout_ms    = 30000
fail_strategy = "closed"   # "closed" (default, block on error) or "open" (allow)

[security.exfiltration_guard]
block_markdown_images = true   # default: true
validate_tool_urls    = true   # default: true
guard_memory_writes   = true   # default: true

[security.pii_filter]
enabled            = true   # default: true — scrubs before LLM context and debug dumps
filter_email       = true
filter_phone       = true
filter_ssn         = true
filter_credit_card = true
filter_names       = false  # opt-in: higher-recall, lower-precision name heuristic

[security.content_isolation.secret_masking]
enabled        = true   # default: true — vault-resolved secrets replaced with placeholders before outbound LLM calls
min_secret_len = 8      # values shorter than this are not masked (too collision-prone)
```

> [!NOTE]
> `PiiFilterConfig` and `SecretMaskingConfig` both default to `enabled = true`. An operator's
> explicit `enabled = false` in an existing `config.toml` is always respected.

## Features

`zeph-memory` (and transitively `zeph-db`) needs exactly one backend selected to compile; `sqlite` is the default so the crate builds in isolation.

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend for `zeph-memory` |
| `postgres` | no | PostgreSQL backend for `zeph-memory` |
| `classifiers` | no | ML-backed injection detection (`classify_injection`) and NER-based PII detection (`detect_pii`); requires an attached classifier backend via `with_classifier` / `with_pii_detector` |

## Security metrics

`ContentSanitizer` exposes metrics via the shared `MetricsSnapshot`:

| Metric | Description |
|--------|-------------|
| `sanitizer_runs` | Total sanitization invocations |
| `sanitizer_injection_flags` | Cumulative injection pattern detections |
| `sanitizer_truncations` | Content truncations applied |
| `quarantine_invocations` | Quarantine LLM calls triggered |
| `quarantine_failures` | Quarantine LLM call failures (falls back to direct sanitization) |
| `exfiltration_images_blocked` | Markdown image pixel-tracking attempts blocked |
| `exfiltration_tool_urls_flagged` | Tool URLs cross-validated against untrusted sources |
| `exfiltration_memory_guards` | Memory write suppression events |

## Installation

```bash
cargo add zeph-sanitizer
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
