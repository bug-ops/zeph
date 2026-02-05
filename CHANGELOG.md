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
