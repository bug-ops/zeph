# Memory and Context

Zeph uses a dual-store memory system: SQLite for structured conversation history and a configurable vector backend (Qdrant or embedded SQLite) for semantic search across past sessions.

## Conversation History

All messages are stored in SQLite. The CLI channel provides persistent input history with arrow-key navigation, prefix search, and Emacs keybindings. History persists across restarts.

When conversations grow long, Zeph generates summaries automatically (triggered after `summarization_threshold` messages, default: 100). Summaries are stored in SQLite and injected into the context window to preserve long-term continuity.

## Semantic Memory

With semantic memory enabled, messages are embedded as vectors for similarity search. Ask "what did we discuss about the API yesterday?" and Zeph retrieves relevant context from past sessions automatically.

Two vector backends are available:

| Backend | Use case | Dependency |
|---------|----------|------------|
| `qdrant` (default) | Production, large datasets | External Qdrant server |
| `sqlite` | Development, single-user, offline | None (embedded) |

Semantic memory uses hybrid search — vector similarity combined with SQLite FTS5 keyword search — to improve recall quality. When the vector backend is unavailable, Zeph falls back to keyword-only search.

Setup with embedded SQLite vectors (no external dependencies):

```toml
[memory]
vector_backend = "sqlite"

[memory.semantic]
enabled = true
recall_limit = 5
```

For Qdrant (production):

```toml
[memory]
vector_backend = "qdrant"  # default

[memory.semantic]
enabled = true
recall_limit = 5
```

See [Set Up Semantic Memory](../guides/semantic-memory.md) for the full setup guide.

## Context Engineering

When `context_budget_tokens` is set (default: 0 = unlimited), Zeph allocates the context window proportionally:

| Allocation | Share | Purpose |
|-----------|-------|---------|
| Summaries | 15% | Compressed conversation history |
| Semantic recall | 25% | Relevant messages from past sessions |
| Recent history | 60% | Most recent messages in current conversation |

A two-tier pruning system manages overflow:

1. **Tool output pruning** (cheap) — replaces old tool outputs with short placeholders
2. **LLM compaction** (fallback) — summarizes middle messages when pruning is not enough

Both tiers run automatically. See [Context Engineering](../advanced/context.md) for tuning options.

## Project Context

Drop a `ZEPH.md` file in your project root and Zeph discovers it automatically. Project-specific instructions are included in every prompt as a `<project_context>` block. Zeph walks up the directory tree looking for `ZEPH.md`, `ZEPH.local.md`, or `.zeph/config.md`.

## Embeddable Trait and EmbeddingRegistry

The `Embeddable` trait provides a generic interface for any type that can be embedded in Qdrant. It requires `id()`, `content_for_embedding()`, `content_hash()`, and `to_payload()` methods. `EmbeddingRegistry<T: Embeddable>` is a generic sync/search engine that delta-syncs items by BLAKE3 content hash and performs cosine similarity search. This pattern is used internally by skill matching, MCP tool registry, and code indexing.

## Credential Scrubbing

When `memory.redact_credentials` is enabled (default: `true`), Zeph scrubs credential patterns from message content before sending it to the LLM context pipeline. This prevents accidental leakage of API keys, tokens, and passwords stored in conversation history. The scrubbing runs via `scrub_content()` in the context builder and covers the same patterns as the output redaction system (see [Security — Secret Redaction](../reference/security.md#secret-redaction)).

## Deep Dives

- [Set Up Semantic Memory](../guides/semantic-memory.md) — Qdrant setup guide
- [Context Engineering](../advanced/context.md) — budget allocation, compaction, recall tuning
