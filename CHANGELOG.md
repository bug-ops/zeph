# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-02-05

### Added

#### M0: Workspace bootstrap
- Cargo workspace with 5 crates: zeph-core, zeph-llm, zeph-skills, zeph-memory, zeph-channels
- Binary entry point with version display
- Default configuration file
- Workspace-level dependency management and lints

#### M1: LLM + CLI agent loop
- LlmProvider trait with Message/Role types
- Ollama backend using ollama-rs
- Config loading from TOML with env var overrides
- Interactive CLI agent loop with multi-turn conversation

#### M2: Skills system
- SKILL.md parser with YAML frontmatter and markdown body (zeph-skills)
- Skill registry that scans directories for `*/SKILL.md` files
- Prompt formatter with XML-like skill injection into system prompt
- Bundled skills: web-search, file-ops, system-info
- Shell execution: agent extracts ```bash``` blocks from LLM responses and runs them
- Multi-step execution loop with 3-iteration limit
- 30-second timeout on shell commands
- Context builder that combines base system prompt with skill instructions

#### M3: Memory + Claude
- SQLite conversation persistence with sqlx (zeph-memory)
- Conversation history loading and message saving per session
- Claude backend via Anthropic Messages API with 429 retry (zeph-llm)
- AnyProvider enum dispatch for runtime provider selection
- CloudLlmConfig for Claude-specific settings (model, max_tokens)
- ZEPH_CLAUDE_API_KEY env var for API authentication
- ZEPH_SQLITE_PATH env var override for database location
- Provider factory in main.rs selecting Ollama or Claude from config
- Memory integration into Agent with optional SqliteStore

#### M4: Telegram channel
- Channel trait abstraction for agent I/O (recv, send, send_typing)
- CliChannel implementation reading stdin/stdout via tokio::task::spawn_blocking
- TelegramChannel adapter using teloxide with mpsc-based message routing
- Telegram user whitelist via `telegram.allowed_users` config
- ZEPH_TELEGRAM_TOKEN env var for Telegram bot activation
- Bot commands: /start (welcome), /reset, /skills forwarded as ChannelMessage
- AnyChannel enum dispatch for runtime channel selection
- zeph-channels crate with teloxide 0.17 dependency
- TelegramConfig in config.rs with TOML and env var support

#### M5: Integration tests + release
- Integration test suite: config, skills, memory, and agent end-to-end
- MockProvider and MockChannel for agent testing without external dependencies
- Graceful shutdown via tokio::sync::watch + tokio::signal (SIGINT/SIGTERM)
- Ollama startup health check (warn-only, non-blocking)
- README with installation, configuration, usage, and skills documentation

### Changed

- Agent is now generic over both LlmProvider and Channel (`Agent<P, C>`)
- Agent::new() accepts a Channel parameter instead of reading stdin directly
- Agent::run() uses channel.recv()/send() instead of direct I/O
- Agent calls channel.send_typing() before each LLM request
- Agent::run() uses tokio::select! to race channel messages against shutdown signal
