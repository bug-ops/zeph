# zeph-sanitizer Guide

Untrusted content isolation: sanitization pipeline, injection detection, truncation, and spotlighting for all external content entering the agent context live here.

- Start with crate-local checks: `cargo build -p zeph-sanitizer`, `cargo nextest run -p zeph-sanitizer`, `cargo clippy -p zeph-sanitizer --all-targets -- -D warnings`.
- Treat every change here as security-sensitive: sanitization is the primary defense against prompt injection.
- Every new injection pattern or bypass discovered in live testing must get a regression test before the fix is merged.
- Do not weaken truncation limits or bypass conditions without an explicit security review.
- If sanitization behavior changes, verify cross-channel consistency (CLI, TUI, Telegram) — a bypass in one channel is a bypass everywhere.
- PII filtering (`pii.rs`) and secret masking (`secret_mask.rs`) are enabled by default (`enabled: true`, #6295) — treat any new gate in this crate that defaults to disabled as a security regression requiring explicit sign-off.
