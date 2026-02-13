# Copilot Code Review Instructions

## Project Context

Zeph is a Rust AI agent workspace (Edition 2024, resolver 3) with 10 crates. Review all changes against these standards.

## Architecture Rules

- `zeph-core` orchestrates all leaf crates; no reverse dependencies allowed
- `zeph-a2a`, `zeph-mcp`, `zeph-tui` are feature-gated and optional
- All dependencies defined in root `[workspace.dependencies]` with no features at workspace level
- Crates inherit via `workspace = true` and specify features locally
- TLS: `rustls` only — reject any `openssl-sys` introduction
- Dependencies must be sorted alphabetically in Cargo.toml

## Rust Code Standards

- Edition 2024 native async traits — reject `async-trait` crate usage
- `unsafe` code is not allowed — if you see any `unsafe` block, treat it as a blocking issue and call it out explicitly
- Avoid `.unwrap()` and `.expect()` in non-test code — prefer `?` with error propagation
- Library crates use `thiserror`; application code uses `anyhow` with `.context()`
- Logging via `tracing` crate only — reject `log` crate or `println!` in library code
- Builder methods must return `self` and be annotated with `#[must_use]`
- String parameters in constructors: accept `impl Into<String>`
- Feature-gated modules: `#[cfg(feature = "name")]` on `pub mod` in `lib.rs`
- Formatting: use rustfmt’s default settings (no project-specific rustfmt.toml)

## Security Review (Critical)

- Flag hardcoded secrets, API keys, tokens, or credentials
- Flag SQL injection risks in SQLite queries (must use parameterized queries)
- Flag command injection in shell executor (validate and sanitize inputs)
- Flag path traversal in skill loader and file operations
- Flag missing input validation at system boundaries
- Flag XSS in any user-facing output
- Verify `deny.toml` compliance for new dependencies

## Error Handling

- Every fallible public function must have `# Errors` doc section
- Error types must be specific and actionable — reject generic `Box<dyn Error>`
- Propagate context: `.context("descriptive message")` for anyhow chains
- Reject silent error swallowing (empty `catch`, `let _ = fallible()`)

## Testing Requirements

- New public functions require corresponding unit tests
- Tests use inline `#[cfg(test)] mod tests` blocks
- Async tests use `#[tokio::test]`
- Tests sharing state must use `#[serial]` from `serial_test`
- Mock types implement the real trait — reject mocking frameworks
- Integration tests requiring external services must be `#[ignore]`

## Performance

- Flag N+1 query patterns in database code
- Flag unbounded collections (Vec, HashMap) populated from external input without limits
- Flag blocking operations inside async contexts (use `spawn_blocking`)
- Flag unnecessary cloning — prefer borrowing

## Review Style

- Be specific and actionable: explain the "why" behind each suggestion
- Reference the specific rule or convention being violated
- Acknowledge well-written code patterns
- Prioritize: security > correctness > performance > style
