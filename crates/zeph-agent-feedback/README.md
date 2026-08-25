# zeph-agent-feedback

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-feedback)](https://crates.io/crates/zeph-agent-feedback)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-feedback)](https://docs.rs/zeph-agent-feedback)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Implicit correction detection for the [Zeph](https://github.com/bug-ops/zeph) AI agent.

Detects when a user is implicitly correcting a previous agent response — without waiting for explicit "that was wrong" phrasing. Two detection strategies run in tandem:

| Detector | LLM calls | Latency | Use case |
|---|---|---|---|
| `FeedbackDetector` | None | ~0 ms | High-confidence regex patterns, 7 languages |
| `JudgeDetector` | Yes (rate-limited) | ~300 ms | Borderline signals, unsupported languages |

## Installation

```toml
[dependencies]
zeph-agent-feedback = "0.22"
```

Or with cargo-add:

```bash
cargo add zeph-agent-feedback
```

**Note:** Requires Rust 1.98 or later (Edition 2024).

## Usage

### Regex detector (no LLM)

```rust
use zeph_agent_feedback::{FeedbackDetector, CorrectionKind};

let detector = FeedbackDetector::new(0.6); // confidence threshold

let history = ["explain how async works"];
if let Some(signal) = detector.detect("that's wrong, try again", &history) {
    assert_eq!(signal.kind, CorrectionKind::ExplicitRejection);
    assert!(signal.confidence >= 0.6);
}
```

### LLM judge (borderline cases)

```rust,no_run
use std::time::Duration;
use zeph_agent_feedback::{FeedbackDetector, JudgeDetector};

// new(adaptive_low, adaptive_high, rate_limit, rate_window)
let mut judge = JudgeDetector::new(0.4, 0.7, 5, Duration::from_secs(60));
let signal = detector.detect(user_msg, &history);

if judge.should_invoke(signal.as_ref()) && judge.check_rate_limit() {
    // evaluate(provider, user_msg, assistant_msg, confidence_threshold, timeout)
    let verdict =
        JudgeDetector::evaluate(&provider, user_msg, assistant_msg, 0.6, Duration::from_secs(30))
            .await?;
    if let Some(correction) = verdict.into_signal(user_msg) {
        // handle confirmed correction
    }
}
```

## Correction Kinds

| Kind | Example trigger |
|---|---|
| `ExplicitRejection` | "no", "that's wrong", "нет" |
| `AlternativeRequest` | "try another approach", "можешь иначе?" |
| `Repetition` | user repeats a prior message (>80% token overlap) |
| `SelfCorrection` | "actually I meant…", "ごめん、間違えました" |

## Multi-language Support

`FeedbackDetector` matches patterns across **7 languages**: English, Russian, Spanish, German, French, Chinese (Simplified), and Japanese.

Each language uses two pattern tiers:
- **Anchored** (`^`): message starts with the phrase — base confidence.
- **Unanchored**: phrase embedded mid-sentence — base confidence minus 0.10.

**Note:** CJK repetition detection falls through to `JudgeDetector` because whitespace tokenisation does not segment Chinese/Japanese text. For Korean, Arabic, and other unsupported languages the regex always returns `None`, triggering the judge (rate-limited to 5 calls/min).

## Rate Limiting

`JudgeDetector` uses a sliding window of 5 calls per minute per detector instance. Call `check_rate_limit()` synchronously before spawning the async judge task — the check modifies `&mut self` and must happen before the task is spawned.

```rust,no_run
// check synchronously, then spawn
if judge.check_rate_limit() {
    tokio::spawn(async move {
        let _ = JudgeDetector::evaluate(
            &provider,
            user_msg,
            assistant_msg,
            threshold,
            Duration::from_secs(30),
        )
        .await;
    });
}
```

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
