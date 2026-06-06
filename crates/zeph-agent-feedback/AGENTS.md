# zeph-agent-feedback Guide

Implicit correction detection from user messages: the regex-only `FeedbackDetector` (zero LLM calls) and the LLM-backed `JudgeDetector` for borderline or missed cases.

- Start with crate-local checks: `cargo build -p zeph-agent-feedback`, `cargo nextest run -p zeph-agent-feedback`, `cargo clippy -p zeph-agent-feedback --all-targets -- -D warnings`.
- Read `specs/016-agent-feedback/spec.md` before changing detection strategy, thresholds, or rate limits.
- Multi-language patterns span 7 languages (en, ru, es, de, fr, zh, ja). Any new or altered pattern needs regression tests for both tiers — anchored (`^`, base confidence) and unanchored (mid-sentence, base − 0.10). Preserve that tier semantics.
- Mind the CJK limitations documented in `lib.rs`: `token_overlap()` uses whitespace tokenisation and does not segment Chinese/Japanese; keep the 2+ character minimum for unanchored CJK patterns to limit false positives.
- Keep regexes compiled once into the flat `Vec<(Regex, f32)>` per correction kind — this is a hot path; never recompile per message.
- Multi-model: `JudgeDetector` calls an LLM — resolve the provider via a `*_provider` field referencing a named `[[llm.providers]]` entry; never hardcode a model. Do not bypass the `judge_rate_limit` / `judge_rate_window_secs` rate limiting from `LearningConfig`.
