# zeph-agent-feedback

Implicit correction detection for the [Zeph](https://github.com/bug-ops/zeph) AI agent.

Provides two detection strategies:

- `FeedbackDetector` — regex-only, zero LLM calls; supports 7 languages.
- `JudgeDetector` — LLM-backed classifier for borderline or missed cases, with a sliding-window rate limiter.
