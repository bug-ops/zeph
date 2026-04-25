---
aliases:
  - ML Classifiers
  - Candle Classifiers
  - Injection Detection
tags:
  - sdd
  - spec
  - ml
  - security
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[010-security/spec]]"
  - "[[015-self-learning/spec]]"
---

# Spec: ML Classifiers

> [!info]
> Candle-backed ML classifiers for injection detection and PII detection;
> lazy-loaded and cached for the session; provides signals for [[010-security/spec|Security Framework]].

## Overview

`zeph-classifiers` (feature: `classifiers`, implies `candle`) — Candle-backed ML classifiers for injection detection and PII detection. All classifiers are lazy-loaded on first use and cached for the session.

## Sources

| File | Contents |
|---|---|
| `crates/zeph-classifiers/src/classifier.rs` | `ClassifierBackend` trait, `CandleClassifier` |
| `crates/zeph-classifiers/src/pii.rs` | `CandlePiiClassifier`, `PiiDetector` trait |
| `crates/zeph-classifiers/src/sanitizer.rs` | `ContentSanitizer::classify_injection()`, `detect_pii()` |
| `crates/zeph-classifiers/src/llm.rs` | `LlmClassifier` wrapping `AnyProvider` |

## ClassifierBackend Trait

Object-safe async trait for ML classifiers:

```rust
trait ClassifierBackend: Send + Sync {
    async fn classify(&self, text: &str) -> Result<ClassificationResult, ClassifierError>;
}
```

## Injection Detection

`CandleClassifier` uses `deberta-v3-small-prompt-injection-v2`:
- Token-based chunking: 448-token chunks with 64-token overlap
- Every chunk framed with `[CLS]` at position 0 and `[SEP]` at end
- Special-token labels stripped from `token_labels` before BIO decode
- `ContentSanitizer::classify_injection()` — async path, separate from sync `sanitize()`

## PII Detection

`CandlePiiClassifier` uses `iiiorg/piiranha-v1-detect-personal-information` (DeBERTa-v2 NER):
- BIO span extraction with special-token masking
- 448-token chunked inference with max-confidence overlap merge
- `PiiSpan { start, end, label, confidence }` — char-level spans
- SHA-256 hash verification before loading safetensors

## Unified Sanitization Pipeline

When `pii_enabled = true`, `ContentSanitizer::sanitize()` uses regex+NER union merge:
1. Regex `PiiFilter::detect_spans()` → span list A
2. `CandlePiiClassifier` NER → span list B
3. Merge with O(n) char→byte precompute, dedup overlapping spans
4. Single-pass redaction

This eliminates double-redaction offset corruption from independent-path design.

## LlmClassifier

`LlmClassifier` wrapping `AnyProvider` returns `FeedbackVerdict` for feedback/correction detection without implementing `ClassifierBackend`. Used by self-learning when `DetectorMode::Model` is set in `LearningConfig`.

`build_feedback_classifier()` in `AppBuilder` resolves the `feedback_provider` named reference from `[[llm.providers]]` and falls back gracefully if not found.

## Config

```toml
[classifiers]
enabled = false
pii_enabled = false
pii_threshold = 0.75

[self_learning]
detector_mode = "model"       # "model" uses LlmClassifier
feedback_provider = "fast"    # named provider reference
```

`--migrate-config` adds `[classifiers]` section with `enabled = false` to existing configs.

## CLI

```bash
zeph classifiers download             # pre-cache all models
zeph classifiers download --model pii|injection|all
```

## Internal Tool Bypass

The ML injection classifier (DeBERTa) is bypassed for outputs from internal Zeph tools that produce only Zeph-generated text (#3384, #3394, #3396). The pattern-based sanitizer continues to run on these outputs for telemetry purposes.

**Bypassed tool names** (exact match on unnamespaced tool name):
`invoke_skill`, `load_skill`, `memory_save`, `memory_search`, `compress_context`,
`complete_focus`, `start_focus`, `schedule_periodic`, `schedule_deferred`, `cancel_task`

**Adversarial-MCP guard**: colon-namespaced tool names (e.g. `server:invoke_skill`) are NEVER matched against the bypass allowlist — only bare names qualify. This prevents a malicious MCP server from registering a tool named `server:invoke_skill` to escape ML classification.

Unit test `skip_ml_fires_for_internal_tool_names` in `zeph-core` proves this invariant (#3396).

## Key Invariants

- Every NER chunk (including middle and last) must be framed with `[CLS]` at position 0 and `[SEP]` at end
- Special-token labels must be stripped before BIO decode — not after
- Regex and NER spans must be merged via union before single-pass redaction — never applied independently
- SHA-256 hash must be verified before loading safetensors
- `classify_injection()` is async and separate from sync `sanitize()` — never block the sync path
- Latency must be traced (`task`, `latency_ms`) on every classifier inference
- NEVER load models on startup — lazy-load on first use
- NEVER apply PII redaction when `pii_enabled = false`, even if NER model is loaded
- NEVER emit a `WARN` security scan false-positive for `.bundled` skill content
- NEVER match colon-namespaced tool names (e.g. `server:invoke_skill`) against the internal tool bypass allowlist — bare names only
