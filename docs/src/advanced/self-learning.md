# Self-Learning Skills

Automatically improve skills based on execution outcomes. When a skill fails repeatedly, Zeph uses self-reflection and LLM-generated improvements to create better skill versions.

## Configuration

```toml
[skills.learning]
enabled = true
auto_activate = false     # require manual approval for new versions
min_failures = 3          # failures before triggering improvement
improve_threshold = 0.7   # success rate below which improvement starts
rollback_threshold = 0.5  # auto-rollback when success rate drops below this
min_evaluations = 5       # minimum evaluations before rollback decision
max_versions = 10         # max auto-generated versions per skill
cooldown_minutes = 60     # cooldown between improvements for same skill
```

## How It Works

1. Each skill invocation is tracked as success or failure
2. When a skill's success rate drops below `improve_threshold`, Zeph triggers self-reflection
3. The agent retries with adjusted context (1 retry per message)
4. If failures persist beyond `min_failures`, the LLM generates an improved skill version
5. New versions can be auto-activated or held for manual approval
6. If an activated version performs worse than `rollback_threshold`, automatic rollback occurs

## Chat Commands

| Command | Description |
|---------|-------------|
| `/skill stats` | View execution metrics per skill |
| `/skill versions` | List auto-generated versions |
| `/skill activate <id>` | Activate a specific version |
| `/skill approve <id>` | Approve a pending version |
| `/skill reset <name>` | Revert to original version |
| `/feedback` | Provide explicit quality feedback |

> Set `auto_activate = false` (default) to review and manually approve LLM-generated skill improvements before they go live.

Skill versions and outcomes are stored in SQLite (`skill_versions` and `skill_outcomes` tables).
