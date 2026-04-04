# zeph-common

Shared primitive types and utilities used across the workspace.

## Purpose

`zeph-common` consolidates duplicated types and utilities that were previously defined independently in multiple crates. It sits at Layer 0 of the dependency graph — no workspace dependencies.

## Contents

- `Secret` — zeroize-on-drop wrapper for sensitive strings
- `VaultError` — shared error type for vault operations
- Common type aliases and utility functions used across crates

## Design Rationale

Before `zeph-common`, types like `Secret` were duplicated or re-exported through multiple crates, creating fragile dependency chains. Extracting them into a single leaf crate eliminates ~320 `#[cfg]` gates and simplifies cross-crate type sharing.
