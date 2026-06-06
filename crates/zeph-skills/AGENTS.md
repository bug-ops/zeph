# zeph-skills Guide

Skill loading, matching, trust, and evolution logic live here.

- Start with crate-local checks: `cargo build -p zeph-skills`, `cargo nextest run -p zeph-skills`, `cargo clippy -p zeph-skills --all-targets -- -D warnings`.
- Preserve matcher quality and trust semantics unless the task explicitly changes retrieval or scoring behavior.
- Multi-model: skill matching, embedding, and self-learning evolution call LLMs — expose `*_provider` fields referencing `[[llm.providers]]` names; never hardcode a model.
- Changes to skill loading or trust should be checked against instruction-file and skill-related docs.
- If external behavior changes, update `crates/zeph-skills/README.md` and the relevant skills docs.
