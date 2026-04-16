# zeph-sanitizer

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

Content sanitization, exfiltration guard, PII filtering, and quarantine for Zeph — untrusted input isolation before LLM context injection.

## Overview

Implements a multi-stage security pipeline that processes all external data before it enters the LLM context window. The pipeline detects prompt injection patterns, wraps content in spotlighting XML delimiters, optionally routes high-risk sources through an isolated quarantine LLM call, and guards outbound paths against data exfiltration. Memory retrieval sources are classified via `MemorySourceHint` to suppress false positive injection flags on recalled user conversations and LLM-generated summaries.

> [!NOTE]
> This crate is marked `publish = false`. It is an internal workspace crate not published to crates.io.

## Key types

| Type | Description |
|------|-------------|
| `ContentSanitizer` | 4-step pipeline: truncate → strip control chars → detect injections → spotlighting XML wrap |
| `TrustLevel` | `Trusted` / `LocalUntrusted` / `ExternalUntrusted` |
| `ContentSourceKind` | Source category (tool output, web scrape, document, etc.) |
| `SanitizedContent` | Output with injection flag list and wrapped content |
| `InjectionFlag` | Detected injection pattern with matched text |
| `QuarantinedSummarizer` | Dual LLM pattern — routes high-risk content through an isolated, tool-less LLM call |
| `ExfiltrationGuard` | Three outbound guards: markdown image tracking, tool URL cross-validation, memory write suppression |
| `ContentSource` | Source metadata with `ContentSourceKind` and optional `MemorySourceHint` for memory retrieval classification |
| `MemorySourceHint` | `ConversationHistory` / `LlmSummary` / `ExternalDocument` — classifies memory retrieval sources to suppress false positive injection flags on recalled user text and LLM-generated summaries |

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
use zeph_sanitizer::{ContentSanitizer, ContentSourceKind, TrustLevel};

let sanitizer = ContentSanitizer::from_config(&config.security.content_isolation);

let result = sanitizer.sanitize(
    &raw_content,
    ContentSourceKind::WebScrape,
    TrustLevel::ExternalUntrusted,
)?;

// result.content contains the wrapped, injection-cleaned text
// result.injection_flags contains any detected patterns
for flag in &result.injection_flags {
    tracing::warn!("Injection detected: {}", flag.pattern);
}
```

## Configuration

```toml
[security.content_isolation]
enabled = true
max_content_size = 65536     # bytes; content truncated before injection detection

[security.content_isolation.quarantine]
enabled = true
sources = ["web_scrape", "document"]  # source kinds routed through quarantine
model = "claude-haiku-4-5-20251001"   # optional; defaults to primary provider
max_tokens = 2048

[security.exfiltration_guard]
enabled = true
block_markdown_images = true
validate_tool_urls = true
block_injection_flagged_memory_writes = true
```

## Features

| Feature | Description |
|---------|-------------|
| `guardrail` | Activates advanced guardrail checks in the sanitization pipeline |

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

This crate is a workspace-internal dependency. Reference it from another workspace crate:

```toml
[dependencies]
zeph-sanitizer = { workspace = true }
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
