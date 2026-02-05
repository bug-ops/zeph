# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Cargo workspace with 4 crates: zeph-core, zeph-llm, zeph-skills, zeph-memory
- Binary entry point with version display
- Default configuration file
- Workspace-level dependency management and lints
- LlmProvider trait with Message/Role types
- Ollama backend using ollama-rs
- Config loading from TOML with env var overrides
- Interactive CLI agent loop with multi-turn conversation
- SKILL.md parser with YAML frontmatter and markdown body (zeph-skills)
- Skill registry that scans directories for `*/SKILL.md` files
- Prompt formatter with XML-like skill injection into system prompt
- Bundled skills: web-search, file-ops, system-info
- Shell execution: agent extracts ```bash``` blocks from LLM responses and runs them
- Multi-step execution loop with 3-iteration limit
- 30-second timeout on shell commands
- Context builder that combines base system prompt with skill instructions
