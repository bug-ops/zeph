# Why Zeph?

## Token Efficiency

Most agent frameworks inject all available tools and instructions into every prompt. Zeph selects only the top-K most relevant skills per query (default: 5) via embedding similarity. Prompt size is O(K), not O(N) — with 50 skills installed, a typical prompt contains ~2,500 tokens of skill context instead of ~50,000. Skills use progressive loading: only metadata (~100 tokens each) is loaded at startup, full body is loaded on first activation, and resource files are fetched on demand.

## Hybrid Inference

Mix local and cloud models in a single setup. Run embeddings through free local Ollama while routing chat to Claude or OpenAI. The orchestrator classifies tasks and routes them to the best provider with automatic fallback chains — if the primary provider fails, the next one takes over. Switch providers with a single config change. Any OpenAI-compatible endpoint works out of the box (Together AI, Groq, Fireworks, and others).

## Skills-First Architecture

Skills are plain markdown files — easy to write, version control, and share. Zeph matches skills by embedding similarity, not keywords, so "check disk space" finds the `system-info` skill even without exact keyword overlap. Edit a `SKILL.md` file and changes apply immediately via hot-reload, no restart required. Skills can evolve autonomously: when the agent detects repeated failures, it reflects on the cause and generates improved skill versions.

## Memory That Persists

Conversation history lives in SQLite, with optional Qdrant for semantic search. Ask "what did we discuss about the API yesterday?" and Zeph retrieves relevant context from past sessions automatically. Long conversations are summarized to stay within the context budget. A two-tier pruning system (tool output pruning first, LLM compaction as fallback) manages memory without manual intervention. Place a `ZEPH.md` in your project root to inject project-specific instructions into every prompt.

## Privacy and Security

Run fully local with Ollama — no API calls, no data leaves your machine. Store API keys in an age-encrypted vault instead of plaintext environment variables. Tools are sandboxed: configure allowed directories, block network access from shell commands, require confirmation for destructive operations like `rm` or `git push --force`. Imported skills start in quarantine with restricted tool access until explicitly trusted.

## Lightweight and Fast

Zeph compiles to a single Rust binary (~15 MB). No Python runtime, no Node.js, no JVM dependency. Native async throughout with no garbage collector overhead. Builds and runs on Linux, macOS, and Windows across x86_64 and ARM64 architectures.
