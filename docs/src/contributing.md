# Contributing

Thank you for considering contributing to Zeph.

## Getting Started

1. Fork the repository
2. Clone your fork and create a branch from `main`
3. Install Rust 1.88+ (Edition 2024 required)
4. Run `cargo build` to verify the setup

## Development

### Build

```bash
cargo build
```

### Test

```bash
# Run unit tests only (exclude integration tests)
cargo nextest run --workspace --lib --bins

# Run all tests including integration tests (requires Docker)
cargo nextest run --workspace --profile ci
```

**Nextest profiles** (`.config/nextest.toml`):
- `default`: Runs all tests (unit + integration)
- `ci`: CI environment, runs all tests with JUnit XML output for reporting

### Integration Tests

Integration tests use [testcontainers-rs](https://github.com/testcontainers/testcontainers-rs) to automatically spin up Docker containers for external services (Qdrant, etc.).

**Prerequisites:** Docker must be running on your machine.

```bash
# Run only integration tests
cargo nextest run --workspace --test '*integration*'

# Run unit tests only (skip integration tests)
cargo nextest run --workspace --lib --bins

# Run all tests
cargo nextest run --workspace
```

Integration test files are located in each crate's `tests/` directory and follow the `*_integration.rs` naming convention.

### Lint

```bash
cargo +nightly fmt --check
cargo clippy --all-targets
```

### Benchmarks

```bash
cargo bench -p zeph-memory --bench token_estimation
cargo bench -p zeph-skills --bench matcher
cargo bench -p zeph-core --bench context_building
```

### Coverage

```bash
cargo llvm-cov --all-features --workspace
```

## Workspace Structure

| Crate | Purpose |
|-------|---------|
| `zeph-core` | Agent loop, config, channel trait |
| `zeph-llm` | LlmProvider trait, Ollama + Claude + OpenAI + Candle backends |
| `zeph-skills` | SKILL.md parser, registry, prompt formatter |
| `zeph-memory` | SQLite conversation persistence, Qdrant vector search |
| `zeph-channels` | Telegram adapter |
| `zeph-tools` | Tool executor, shell sandbox, web scraper |
| `zeph-index` | AST-based code indexing, semantic retrieval, repo map |
| `zeph-mcp` | MCP client, multi-server lifecycle |
| `zeph-a2a` | A2A protocol client and server |
| `zeph-tui` | ratatui TUI dashboard with real-time metrics |

## Pull Requests

1. Create a feature branch: `feat/<scope>/<description>` or `fix/<scope>/<description>`
2. Keep changes focused — one logical change per PR
3. Add tests for new functionality
4. Ensure all checks pass: `cargo +nightly fmt`, `cargo clippy`, `cargo nextest run --lib --bins`
5. Write a clear PR description following the template

## Commit Messages

- Use imperative mood: "Add feature" not "Added feature"
- Keep the first line under 72 characters
- Reference related issues when applicable

## Code Style

- Follow workspace clippy lints (pedantic enabled)
- Use `cargo +nightly fmt` for formatting
- Avoid unnecessary comments — code should be self-explanatory
- Comments are only for cognitively complex blocks

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](https://github.com/bug-ops/zeph/blob/main/LICENSE).
