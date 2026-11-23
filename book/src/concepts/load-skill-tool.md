# `load_skill` Tool

The `load_skill` tool lets the LLM fetch the full body of any registered skill on demand, without that body being pre-loaded into the system prompt.

## Problem it solves

Zeph selects the top-K most relevant skills for each message (default: 5) and injects their full bodies into the system prompt. All registered skills also appear in the prompt as compact metadata — name and description — inside an `<other_skills>` catalog. This keeps the prompt lean regardless of how many skills are installed.

The drawback is that the LLM sees a skill is available but cannot read its instructions. When the agent determines a non-TOP skill is actually relevant, it had no way to retrieve its content. `load_skill` closes that gap.

## How it works

When native tool use is enabled, `load_skill` is registered alongside other tools (shell, file, web scrape, etc.) and exposed to the LLM via the tool catalog.

**Signature:**

```json
{
  "tool": "load_skill",
  "parameters": {
    "skill_name": "<name from other_skills catalog>",
    "path": "references/guide.md"
  }
}
```

`path` is optional. Without it the tool returns the skill body. With it, the tool returns one skill-local resource file instead: the path must be one of the skill's `scripts/`, `references/` or `assets/` files, written relative to the skill directory exactly as the skill listing shows it (for example `references/guide.md`). The listing is appended to the skill body in the prompt, with a one-line `load_skill` hint.

The tool reads the skill body from the shared in-memory registry (which holds all registered skills, not just the top-K). The body is returned as the tool result and the LLM continues inference with the full instructions now in context.

## When to use it

The LLM should call `load_skill` when:

1. A skill appears in `<other_skills>` by name and description.
2. The description suggests that skill contains instructions relevant to the current task.
3. The full instructions are needed to proceed correctly.

Example: the user asks to generate an MCP bridge. The `mcp-generate` skill did not rank in the top-K for this session, but its name and description appear in `<other_skills>`. The LLM calls `load_skill("mcp-generate")` to retrieve the full instructions before generating the bridge.

`load_skill` is registered alongside other tools (shell, file, web scrape, etc.) and exposed to the LLM via the standard native tool catalog on all providers.

## Security model

- **Read-only**: the tool only reads from the registry. It cannot create, modify, or delete skills.
- **Registry-scoped**: only skills present in the runtime registry can be loaded. `skill_name` is a name, not a path.
- **Resource paths are validated and listed**: `path` must be relative, start with `scripts/`, `references/` or `assets/`, contain no `..`, backslash or NUL, and be at most 512 bytes. It must also appear in the skill's discovered resource list. Anything else returns `resource not found` without touching the filesystem.
- **Resources use the body's trust gate**: Blocked skills are refused, the integrity check runs, non-Trusted content is sanitized, and Quarantined content is wrapped and lowers the turn's trust floor. Resource files are limited to 256 KiB, must be valid UTF-8 (binary assets are listed but cannot be loaded), and symlinks that leave the skill directory are rejected.
- **Size cap**: bodies are passed through `truncate_tool_output`, which caps output at 30,000 characters. If a body exceeds this limit, the tool returns the head and tail of the body with a truncation notice in the middle.
- **No path traversal**: body loading goes through `SkillRegistry::get_body`, which reads from the pre-validated path stored at registry load time. A resource `path` is resolved against the skill directory only after validation and a canonical-path containment check.

## Error cases

| Situation | Tool result |
|-----------|-------------|
| Skill name not in registry | `skill not found: <name>` |
| Registry lock poisoned (internal error) | `ToolError::InvalidParams` returned to the agent loop |
| `skill_name` field missing from parameters | `ToolError` from parameter deserialization |
| `path` is not a listed resource of the skill | `resource not found: <path> in skill <name>` |
| Resource over 256 KiB or not UTF-8 | `resource could not be loaded: <path> in skill <name>` |
| Body exceeds 30,000 characters | Truncated body with notice: `[... N chars truncated ...]` |

All error messages are descriptive and include the skill name where applicable, so the LLM can report the issue to the user or try an alternative skill.

## Sub-agents

A sub-agent whose `skills.include` is non-empty and whose `tools.allow` list does not name `load_skill` still gets a scoped `load_skill`: it can load only the included skills and their resources, and any other skill name is blocked. Listing `load_skill` in `tools.allow` grants the unscoped tool, and a `deny` or `except` entry for `load_skill` removes it. A definition with no `skills.include` gets no implicit `load_skill`.

## Relationship to skill matching

`load_skill` complements — it does not replace — the automatic top-K matching. The matching pipeline runs first and selects the most semantically relevant skills for the current query. `load_skill` is a fallback for cases where the matcher did not rank a skill highly enough but the LLM's own reasoning identifies it as relevant.

If you find yourself repeatedly needing `load_skill` for the same skill, that skill's description or trigger keywords may need tuning so the matcher picks it up automatically.

## See also

- [Skills](skills.md) — how skills are matched and injected
- [Add Custom Skills](../guides/custom-skills.md) — creating your own skills
- [Context Engineering — Skill Prompt Modes](../advanced/context.md#skill-prompt-modes) — compact vs full body injection
