# Crates Guide

This directory contains the workspace crates. The root [`AGENTS.md`](../AGENTS.md) remains the primary instruction file; use local `AGENTS.md` files in individual crates for crate-specific workflow notes.

- Keep changes local to the crate you are editing unless shared APIs require coordinated updates.
- Default verification is crate-first: `cargo build -p <crate>`, `cargo nextest run -p <crate>`, then workspace checks only if the change crosses crate boundaries.
- Use `cargo clippy -p <crate> --all-targets -- -D warnings` before considering crate-local work done.
- If a crate's public behavior or configuration changes, update the crate `README.md`, root docs in `docs/src/`, and any relevant config or CLI surfaces.
