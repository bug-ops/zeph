# zeph-scheduler Guide

Scheduled task persistence, cron handling, and update-check jobs live here.

- Start with crate-local checks: `cargo build -p zeph-scheduler`, `cargo nextest run -p zeph-scheduler`, `cargo clippy -p zeph-scheduler --all-targets -- -D warnings`.
- Preserve deterministic scheduling and storage behavior; time parsing and job-state transitions should get explicit test coverage.
- Keep scheduler behavior aligned with root CLI/config and any built-in scheduling skills.
- If external behavior changes, update `crates/zeph-scheduler/README.md` and the relevant scheduler docs.
