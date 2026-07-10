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
| `quarantine::QuarantinedSummarizer` | Dual LLM pattern — routes high-risk content through an isolated, tool-less LLM call |
| `exfiltration::ExfiltrationGuard` | Three outbound guards: markdown image tracking, tool URL cross-validation, memory write suppression |
| `ContentSource` | Source metadata with `ContentSourceKind` and optional `MemorySourceHint` for memory retrieval classification |
| `MemorySourceHint` | `ConversationHistory` / `LlmSummary` / `ExternalContent` — classifies memory retrieval sources to suppress false positive injection flags on recalled user text and LLM-generated summaries |

## Sanitization pipeline

```
External data
    ↓ 1. Truncate to max_content_size
    ↓ 2. Strip null bytes and control characters
    ↓ 3. Detect 17 injection patterns (OWASP variants + encoding)
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
enabled = true
sources = ["web_scrape", "a2a_message"]  # source kinds routed through quarantine
model = "claude-haiku-4-5-20251001"   # optional; defaults to primary provider
max_tokens = 2048

[security.exfiltration_guard]
enabled = true
block_markdown_images = true
validate_tool_urls = true
block_injection_flagged_memory_writes = true
```

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
