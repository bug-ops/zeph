# zeph-scheduler Guide

Scheduled task persistence, cron handling, and update-check jobs live here.

- Start with crate-local checks: `cargo build -p zeph-scheduler`, `cargo nextest run -p zeph-scheduler`, `cargo clippy -p zeph-scheduler --all-targets -- -D warnings`.
- Read `specs/018-scheduler/spec.md` ("RTW-A Temporal Re-Entry Defense") before changing task provenance, trust gating, or adding a new `TaskHandler`.
- Preserve deterministic scheduling and storage behavior; time parsing and job-state transitions should get explicit test coverage.
- A new `TaskHandler` must declare `reads_external_content()`/`injects_agent_prompt()` accurately — RTW-A Mech4 (external-read suppression) gates on these capability flags for every registered handler, not on a `TaskKind` match (#6126).
- `TaskProvenance` is a trust boundary, not a display label: DB-hydrated jobs have their provenance forced to `External` at `init()` regardless of the stored column value — never trust a writer-controllable provenance field verbatim (#6125).
- Keep scheduler behavior aligned with root CLI/config and any built-in scheduling skills.
- If external behavior changes, update `crates/zeph-scheduler/README.md` and the relevant scheduler docs.
