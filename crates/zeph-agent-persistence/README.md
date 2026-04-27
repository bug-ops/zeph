# zeph-agent-persistence

Agent persistence service for Zeph: history loading, message persistence, tool-pair sanitization, and background extraction enqueueing.

## Overview

This crate provides `PersistenceService` — a stateless facade for agent message persistence. It extracts persistence logic from `zeph-core` so that editing persistence code does not trigger recompilation of the tool dispatcher or context assembly paths.

## Key types

- `PersistenceService` — stateless facade; all inputs via explicit parameters
- `PersistMessageRequest` — owned request struct (no lifetimes)
- `PersistMessageOutcome` / `LoadHistoryOutcome` — owned outcome structs
- `MemoryPersistenceView`, `SecurityView`, `MetricsView` — borrow-lens views constructed at call site

## Design

`zeph-core` depends on this crate. This crate does **not** depend on `zeph-core`. `zeph-agent-tools` depends on this crate for `PersistMessageRequest`.
