# zeph-tools Guide

Tool execution, shell permissions, filtering, scraping, and audit behavior live here.

- Start with crate-local checks: `cargo build -p zeph-tools`, `cargo nextest run -p zeph-tools`, `cargo clippy -p zeph-tools --all-targets -- -D warnings`.
- Treat shell execution, permissions, trust gating, network access, and audit logging as security-sensitive behavior.
- Prefer explicit safeguards over implicit defaults; regressions here can affect the whole agent.
- If external behavior changes, update `crates/zeph-tools/README.md` and the relevant tools/security docs.
