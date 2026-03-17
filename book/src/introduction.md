# Zeph

You have an LLM. You want it to actually do things — run commands, search files, remember context, learn new skills. But wiring all that together means dealing with token bloat, provider lock-in, and context that evaporates between sessions.

Zeph is a lightweight AI agent written in Rust that connects to any LLM provider (local Ollama, Claude, OpenAI, or HuggingFace models), equips it with tools and skills, and manages conversation memory — all while keeping prompt size minimal. Only the skills relevant to your current query are loaded, so adding more capabilities never inflates your token bill.

## What You Can Do with Zeph

**Development assistant.** Point Zeph at your project directory, and it reads files, runs shell commands, searches code, and answers questions with full context. Drop a `ZEPH.md` file in your repo to give it project-specific instructions.

**Chat bot.** Deploy Zeph as a Telegram, Discord, or Slack bot with streaming responses, user whitelisting, and voice message transcription. Your team gets an AI assistant in the channels they already use.

**Self-hosted agent.** Run fully local with Ollama — no data leaves your machine. Encrypt API keys with age vault. Sandbox tool access with path restrictions and command confirmation. You control everything.

## Get Started

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
zeph init
zeph
```

Three commands: install the binary, generate a config, start talking.

**Cross-platform**: Linux, macOS, Windows (x86_64 + ARM64).

## Next Steps

- [Why Zeph?](why-zeph.md) — what sets Zeph apart from other LLM wrappers
- [First Conversation](getting-started/first-conversation.md) — from zero to "aha moment" in 5 minutes
- [Installation](getting-started/installation.md) — all installation methods (source, binaries, Docker)
