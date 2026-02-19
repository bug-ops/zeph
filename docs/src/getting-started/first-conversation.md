# First Conversation

This guide takes you from a fresh install to your first productive interaction with Zeph.

## Prerequisites

- Zeph [installed](installation.md) and `zeph init` completed
- Either Ollama running locally (`ollama serve`), or a Claude/OpenAI API key configured

## Start the Agent

```bash
zeph
```

You see a `You:` prompt. Type a message and press Enter.

## Ask About Files

```
You: What files are in the current directory?
```

Behind the scenes:
1. Zeph embeds your query and matches the `file-ops` skill (ranked by cosine similarity)
2. The skill's instructions are injected into the prompt
3. The agent calls the `glob` tool to list files
4. You get a structured answer with the directory listing

You did not tell Zeph which skill to use — it figured it out from context.

## Run a Command

```
You: Check disk usage on this machine
```

Zeph matches the `system-info` skill and runs `df -h` via the `bash` tool. If a command is potentially destructive (like `rm` or `git push --force`), Zeph asks for confirmation first:

```
Execute: rm -rf /tmp/old-cache? [y/N]
```

## See Memory in Action

```
You: What files did we just look at?
```

Zeph remembers the full conversation. It answers from context without re-running any commands. With [semantic memory](../guides/semantic-memory.md) enabled (Qdrant), Zeph can also recall relevant context from past sessions.

## Useful Slash Commands

| Command | Description |
|---------|-------------|
| `/skills` | Show active skills and usage statistics |
| `/mcp` | List connected MCP tool servers |
| `/reset` | Clear conversation context |
| `/image <path>` | Attach an image for visual analysis |

Type `exit`, `quit`, or press Ctrl-D to stop the agent.

## Next Steps

- [Configuration Wizard](wizard.md) — customize providers, memory, and channels
- [Skills](../concepts/skills.md) — understand how skill matching works
- [Tools](../concepts/tools.md) — what the agent can do with shell, files, and web
