# Conversation Summarization

Automatically compress long conversation histories using LLM-based summarization to stay within context budget limits.

Requires an LLM provider (Ollama or Claude). Set `context_budget_tokens = 0` to disable proportional allocation and use unlimited context.

> For the full context management pipeline (semantic recall, message trimming, compaction, tool output management), see [Context Engineering](context.md).

## Configuration

```toml
[memory]
summarization_threshold = 100
context_budget_tokens = 8000  # Set to LLM context window size (0 = unlimited)
```

## How It Works

- Triggered when message count exceeds `summarization_threshold` (default: 100)
- Summaries stored in SQLite with token estimates
- Batch size = threshold/2 to balance summary quality with LLM call frequency
- Context builder allocates proportional token budget:
  - **15%** for summaries
  - **25%** for semantic recall (if enabled)
  - **60%** for recent message history

## Token Estimation

Token counts are estimated using a chars/4 heuristic (100x faster than tiktoken, ±25% accuracy). This is sufficient for proportional budget allocation where exact counts are not critical.
