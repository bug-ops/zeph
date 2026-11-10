# First Conversation

This guide takes you from a fresh install to your first productive interaction with Zeph in under 5 minutes.

## Prerequisites

- Zeph [installed](installation.md) and `zeph init` completed
- Either Ollama running locally (`ollama serve`), or a Claude/OpenAI/Gemini API key configured

## Start the Agent

```bash
zeph
```

You see a `You:` prompt. Type a message and press Enter.

For the TUI dashboard with side panels showing skills, memory, and metrics:

```bash
zeph --tui
```

## Ask About Files

```
You: What files are in the current directory?
```

Behind the scenes:

1. Zeph embeds your query and matches the `file-ops` skill by cosine similarity
2. The skill's instructions are injected into the prompt
3. The agent calls the `list_directory` or `find_path` tool
4. You get a structured answer with the directory listing

You did not tell Zeph which skill to use — it figured it out from context.

## Run a Command

```
You: Check disk usage on this machine
```

Zeph matches the `system-info` skill and runs `df -h` via the `bash` tool. Destructive commands (`rm`, `git push --force`, `drop table`) require confirmation:

```
Execute: rm -rf /tmp/old-cache? [y/N]
```

## See Memory in Action

```
You: What files did we just look at?
```

Zeph remembers the full conversation and answers from context without re-running any commands. With [semantic memory](../guides/semantic-memory.md) enabled, Zeph recalls relevant context from past sessions too.

## Project Instructions

Drop a `zeph.md` file in your project root to give Zeph standing context — coding conventions, domain knowledge, project rules. The content is injected into every prompt automatically.

```markdown
# Project Instructions

- Language: TypeScript, strict mode
- Test framework: vitest
- Commit messages follow Conventional Commits
- Never modify files under `generated/`
```

See [Instruction Files](../concepts/instruction-files.md) for provider-specific files and hot-reload behavior.

## Useful Slash Commands

| Command | Description |
|---------|-------------|
| `/skills` | Show active skills and usage statistics |
| `/goal check [condition]` | Ask the model to judge whether a condition is met, based on the conversation so far (advisory, see below) |
| `/skill state [name]` | Show live [skill execution state](../guides/custom-skills.md#skill-execution-state) |
| `/mcp` | List connected MCP tool servers |
| `/new` | Start a fresh conversation without restarting |
| `/image <path>` | Attach an image for visual analysis |
| `/debug-dump` | Enable debug dump for the current session |

### Checking a goal with `/goal check`

`/goal check` runs one advisory self-evaluation and prints `achieved`, `reasoning`, `confidence` and, when not achieved, suggestions. The condition is, in order: the text you pass (`/goal check the tests pass`), the active goal if one exists, or your last message. It never blocks or retries work.

- It uses the `[goals] supervisor_provider` and `supervisor_timeout_secs` settings (empty provider means the main provider).
- It works even with `[goals] enabled = false`; only the active-goal source is unavailable.
- It is rejected while an autonomous goal session is running; use `/goal status` instead.
- Secrets are scrubbed from the conversation excerpt before it is sent.

#### Automatic check after a turn

Set `[goals] enabled = true` and `auto_check = true` to get the same advisory report automatically in the background after a turn that used tools while a goal is active. It judges the active goal only, skips scheduler and `/loop` turns, and shows `Verifying goal…` while it runs. It costs one supervisor call per such turn and is not shown in ACP, daemon (A2A and loopback gateway) or `--json` sessions.

Type `exit`, `quit`, or press Ctrl-D to stop the agent.

## Next Steps

- [Configuration Wizard](wizard.md) — customize providers, memory, and channels
- [Configuration Recipes](../guides/config-recipes.md) — copy-paste configs for common setups
- [Skills](../concepts/skills.md) — how skill matching works
- [Tools](../concepts/tools.md) — shell, files, web, and MCP tools
