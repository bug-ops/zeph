# Performance

Zeph applies targeted optimizations to the agent hot path: context building, token estimation, and skill embedding.

## Benchmarks

Criterion benchmarks cover three critical hot paths:

| Benchmark | Crate | What it measures |
|-----------|-------|------------------|
| `token_estimation` | zeph-memory | `estimate_tokens()` throughput on varying input sizes |
| `matcher` | zeph-skills | In-memory cosine similarity matching latency |
| `context_building` | zeph-core | Full context assembly pipeline |

Run benchmarks:

```bash
cargo bench -p zeph-memory --bench token_estimation
cargo bench -p zeph-skills --bench matcher
cargo bench -p zeph-core --bench context_building
```

## Token Estimation

Token count is estimated as `input.len() / 3` (byte length divided by 3). This avoids the overhead of `chars().count()` while remaining a reasonable approximation for mixed ASCII/UTF-8 text across common LLM tokenizers.

## Concurrent Skill Embedding

Skill embeddings are computed concurrently using `buffer_unordered(50)`, parallelizing API calls to the embedding provider during startup and hot-reload. This reduces initial load time proportionally to the number of skills when using a remote embedding endpoint.

## Parallel Context Preparation

Context sources (summaries, cross-session recall, semantic recall, code RAG) are fetched concurrently via `tokio::try_join!`. Latency equals the slowest single source rather than the sum of all four.

## String Pre-allocation

Context assembly and compaction pre-allocate output strings based on estimated final size, reducing intermediate allocations during prompt construction.

## TUI Render Performance

The TUI applies two optimizations to maintain responsive input during heavy streaming:

- **Event loop batching**: `biased` `tokio::select!` prioritizes keyboard/mouse input over agent events. Agent events are drained via `try_recv` loop, coalescing multiple streaming chunks into a single frame redraw.
- **Per-message render cache**: Syntax highlighting and markdown parsing results are cached with content-hash keys. Only messages with changed content are re-parsed. Cache invalidation triggers: content mutation, terminal resize, and view mode toggle.

## SQLite WAL Mode

SQLite is opened with WAL (Write-Ahead Logging) mode, enabling concurrent reads during writes and improving throughput for the message persistence hot path.

## Cached Prompt Tokens

The system prompt token count is cached after the first computation and reused across agent loop iterations. This avoids re-estimating tokens for the static portion of the prompt on every turn.

## LazyLock System Prompt

Static system prompt fragments (tool definitions, environment preamble) use `LazyLock` for one-time initialization, eliminating repeated string allocation and formatting.

## Content Hash Doom-Loop Detection

The agent loop tracks a BLAKE3 content hash of the last LLM response. If the model produces an identical response twice consecutively, the loop breaks early to prevent infinite tool-call cycles.

## Tokio Runtime

Tokio is imported with explicit features (`macros`, `rt-multi-thread`, `signal`, `sync`) instead of the `full` meta-feature, reducing compile time and binary size.
