# Why Zeph?

## Token Efficiency

Most agent frameworks inject all available tools and instructions into every prompt. Zeph takes a different approach at every layer:

- **Skill selection** — only the top-K most relevant skills per query (default: 5) are loaded via embedding similarity. With 50 skills installed, a typical prompt contains ~2,500 tokens of skill context instead of ~50,000. Progressive loading fetches metadata first (~100 tokens each), full body on activation, and resource files on demand.
- **Tool schema filtering** — tool definitions are filtered per-turn based on semantic relevance to the current task, removing irrelevant schemas from the context window entirely.
- **TAFC (Think-Augmented Function Calling)** — for complex tools, the model reasons about parameter values before committing, reducing error-driven retries that waste tokens.
- **Tool result caching** — deterministic tool results are cached within the session, eliminating redundant executions and their token overhead.
- **Semantic response caching** — LLM responses are cached by embedding similarity, so semantically equivalent queries reuse previous answers without an API call.

Prompt size is O(K), not O(N) — and every layer actively works to keep it there.

## Intelligent Context Management

Long conversations are the norm, not an edge case. Zeph manages context pressure automatically:

- **Structured anchored summarization** — summaries follow a typed schema with mandatory sections (goal, files modified, decisions, open questions, next steps), preventing the compressor from silently dropping critical facts.
- **Compaction probe validation** — after every summarization, a Q&A probe verifies that key facts survived compression. If the probe fails, the agent falls back to keeping original turns.
- **Subgoal-aware compaction (HiAgent)** — during multi-step tasks, the agent tracks the current subgoal and only compresses information that is no longer relevant to it, preserving active working memory.
- **Write-time importance scoring** — memory entries receive an importance score at write time based on content markers, information density, and role, so frequently-referenced and explicitly important memories surface higher during retrieval.

## Graph Memory

Beyond flat vector search, Zeph builds a structured knowledge graph from conversations:

- **MAGMA typed edges** — relationships between entities are classified into five types (Causal, Temporal, Semantic, CoOccurrence, Hierarchical), enabling type-filtered traversal.
- **SYNAPSE spreading activation** — retrieval activates a seed entity and propagates through the graph with hop-by-hop decay and lateral inhibition, surfacing multi-hop connections that flat similarity search misses.
- **Community detection** — label propagation identifies entity clusters, providing topic-level context for retrieval.

Ask "why did we choose Kafka?" and Zeph follows causal edges from Kafka through the decision graph to surface the original rationale — not just documents that mention the word.

## Hybrid Inference

Mix local and cloud models in a single setup. Run embeddings through free local Ollama while routing chat to Claude or OpenAI. The orchestrator classifies tasks and routes them to the best provider with automatic fallback chains — if the primary provider fails, the next one takes over. Thompson Sampling exploration balances cost and quality across providers. Switch providers with a single config change. Any OpenAI-compatible endpoint works out of the box (Together AI, Groq, Fireworks, and others).

## Skills-First Architecture

Skills are plain markdown files — easy to write, version control, and share. Zeph matches skills by embedding similarity, not keywords, so "check disk space" finds the `system-info` skill even without exact keyword overlap. Edit a `SKILL.md` file and changes apply immediately via hot-reload, no restart required.

Skills evolve autonomously: when the agent detects repeated failures via the multi-language FeedbackDetector (supporting 7 languages), it reflects on the cause and generates improved skill versions. Wilson score re-ranking ensures that well-performing skills surface first.

## Task Orchestration

For complex goals, Zeph decomposes work into a task DAG and executes it with parallel scheduling:

- **Plan template caching** — successful plans are cached by goal embedding, so similar future requests reuse an adapted template instead of replanning from scratch (50% cost reduction, 27% latency improvement).
- **Tool dependency graph** — tools declare ordering constraints (`requires` for hard gates, `prefers` for soft boosts), enabling the agent to present tools in the right sequence without hardcoded execution order.

## Privacy and Security

Run fully local with Ollama — no API calls, no data leaves your machine. Store API keys in an age-encrypted vault instead of plaintext environment variables. Tools are sandboxed: configure allowed directories, block network access from shell commands, require confirmation for destructive operations like `rm` or `git push --force`. Imported skills start in quarantine with restricted tool access until explicitly trusted. Content from untrusted sources (web scraping, tool output, MCP servers) is sanitized through a multi-layer isolation pipeline before reaching the agent.

## Multi-Channel

Deploy Zeph across CLI, TUI dashboard, Telegram, Discord, and Slack with consistent feature parity across all channels. The TUI provides real-time metrics, a command palette, and live status indicators for background operations. All 7 channels support the same 16-method `Channel` trait — no feature is silently missing in any mode.

## Lightweight and Fast

Zeph compiles to a single Rust binary (~12 MB). No Python runtime, no Node.js, no JVM dependency. Native async throughout with no garbage collector overhead. Builds and runs on Linux, macOS, and Windows across x86_64 and ARM64 architectures.
