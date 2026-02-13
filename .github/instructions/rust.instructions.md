---
applyTo: "**/*.rs"
---

# Rust File Review

## Lints

- Workspace Clippy lints `clippy::all` + `clippy::pedantic` are configured as warnings in `Cargo.toml`
- Review policy: prefer `#![forbid(unsafe_code)]` in crates and reject new `unsafe` blocks unless strictly justified and well-documented
- Review policy: avoid `unwrap()` and `expect()` in non-test code; require explicit error handling instead

## Async Patterns

- Use native async trait methods (Edition 2024)
- Use `Pin<Box<dyn Future<...> + Send + '_>>` only when trait object safety requires it
- Runtime: `tokio` with `#[tokio::main]` entry point

## Type Conventions

- Type aliases for complex pinned types: `type ChatStream = Pin<Box<dyn Stream<...> + Send>>`
- Re-export public API at crate root via `pub use` in `lib.rs`
- Serde: `#[serde(tag = "kind")]` for enums with data, `#[serde(rename_all = "camelCase")]` for A2A types

## Test Code

- Unit tests in inline `#[cfg(test)] mod tests` at module end
- Mock types (MockProvider, MockChannel) inside `#[cfg(test)]` blocks implementing the real trait
- Use `tempfile` for filesystem fixtures, `testcontainers` for Qdrant
- Tests sharing state require `#[serial]` attribute
