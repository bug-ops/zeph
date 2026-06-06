# Integration Tests Guide

This directory contains top-level integration scenarios for the workspace binary and cross-crate behavior.

- Prefer `cargo nextest run --test <name>` or targeted workspace `nextest` filters over `cargo test`.
- Keep tests deterministic and focused on externally visible behavior.
- Add regression coverage here when a bug spans multiple crates or depends on root-binary wiring.
- Document external service requirements clearly when a test needs Docker, Qdrant, or network-like fixtures.
