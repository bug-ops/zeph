# Performance

Zeph applies targeted optimizations to the agent hot path: context building, token estimation, and skill embedding.

## Benchmarks

Criterion benchmarks cover three critical hot paths:

| Benchmark | Crate | What it measures |
|-----------|-------|------------------|
| `token_estimation` | zeph-memory | `TokenCounter` throughput on varying input sizes |
| `matcher` | zeph-skills | In-memory cosine similarity matching latency |
| `context_building` | zeph-core | Full context assembly pipeline |

Run benchmarks:

```bash
cargo bench -p zeph-memory --bench token_estimation
cargo bench -p zeph-skills --bench matcher
cargo bench -p zeph-core --bench context_building
```

## Token Counting

Token counts are computed by `TokenCounter` in `zeph-memory` using the `tiktoken-rs` BPE tokenizer (`cl100k_base`). Results are cached in a `DashMap` (10,000-entry cap) for O(1) amortized lookups on repeated inputs. An input size guard (64 KiB) prevents oversized text from polluting the cache. When the tokenizer is unavailable, the implementation falls back to `input.len() / 4`.

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

## SQLite Message Index

Migration `015_messages_covering_index.sql` replaces the single-column `conversation_id` index on the `messages` table with a composite covering index on `(conversation_id, id)`. History queries filter by `conversation_id` and order by `id`, so the covering index satisfies both clauses from the index alone, eliminating the post-filter sort step.

The `load_history_filtered` query uses a CTE to express the base filter before applying ordering and limit, replacing the previous double-sort subquery pattern.

## SQLite Connection Pool

The memory layer opens a pool of SQLite connections (default: 5, configurable via `[memory] sqlite_pool_size`). Pooling eliminates per-operation open/close overhead and allows concurrent readers during write transactions.

## In-Memory Unsummarized Counter

`MemoryState` maintains an in-memory `unsummarized_count` counter that is incremented on each message save. This replaces a `COUNT(*)` SQL query that previously ran on every message persistence call, removing a synchronous DB round-trip from the agent hot path.

## SQLite WAL Mode

SQLite is opened with WAL (Write-Ahead Logging) mode, enabling concurrent reads during writes and improving throughput for the message persistence hot path.

## Cached Prompt Tokens

The system prompt token count is cached after the first computation and reused across agent loop iterations. This avoids re-estimating tokens for the static portion of the prompt on every turn.

Context compaction (`should_compact()`) reads this cached value directly — an O(1) field access — instead of scanning all messages to sum token counts. The `token_counter` and `token_safety_margin` fields were removed from `ContextManager`; the single cached value is sufficient.

## LazyLock System Prompt

Static system prompt fragments (tool definitions, environment preamble) use `LazyLock` for one-time initialization, eliminating repeated string allocation and formatting.

## Cached Environment Context

`EnvironmentContext` (working directory, OS, git branch, active model) is built once at agent bootstrap and stored on `Agent`. On skill hot-reload, only `git_branch` and `model_name` are refreshed — no git subprocess is spawned per agent loop turn.

## Content Hash Doom-Loop Detection

The agent loop tracks a content hash of the last LLM response. If the model produces an identical response twice consecutively, the loop breaks early to prevent infinite tool-call cycles.

The hash is computed in-place using `DefaultHasher` with no intermediate `String` allocation. The previous implementation serialized the response to a temporary string before hashing; the current implementation feeds message parts directly into the hasher.

## Tool Output Pruning Token Count

`prune_stale_tool_outputs` counts tokens for each `ToolResult` part exactly once. A prior version called `count_tokens` twice per part (once for the guard condition, once after deciding to prune), doubling token-estimation work for large tool outputs.

## Build Profiles

The workspace provides a `ci` build profile for faster CI release builds:

```toml
[profile.ci]
inherits = "release"
lto = "thin"
codegen-units = 16
```

Thin LTO with 16 codegen units reduces link time by ~2-3x compared to the release profile (fat LTO, 1 codegen unit) while maintaining comparable runtime performance. Production release binaries still use the full `release` profile for maximum optimization.

## Tokio Runtime

Tokio is imported with explicit features (`macros`, `rt-multi-thread`, `signal`, `sync`) instead of the `full` meta-feature, reducing compile time and binary size.
