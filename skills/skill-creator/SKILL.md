---
name: skill-creator
description: Create new Agent Skills following the agentskills.io specification. Use when the user asks to create, write, build, or generate a new skill, or asks how to make a SKILL.md file.
compatibility: Requires write access to skills directory
---
# Skill Creator

Create a new skill following the agentskills.io specification (https://agentskills.io/specification).

## Step 1: Gather requirements

Ask the user:
1. What should the skill do?
2. What tools or commands does it need?
3. Does it need references, scripts, or assets?

If the user already provided enough context, skip asking and proceed.

## Step 2: Choose a name

Rules for the `name` field:
- 1-64 characters
- Lowercase letters, numbers, hyphens only (`a-z`, `0-9`, `-`)
- Must NOT start or end with `-`
- Must NOT contain consecutive hyphens (`--`)
- Must match the parent directory name

## Step 3: Write the description

Rules for the `description` field:
- 1-1024 characters
- Must describe WHAT the skill does AND WHEN to use it
- Include keywords that help semantic matching

Good:
```
description: Extract text and tables from PDF files, fill PDF forms, and merge multiple PDFs. Use when working with PDF documents or when the user mentions PDFs, forms, or document extraction.
```

Bad:
```
description: Helps with PDFs.
```

## Step 4: Create the directory structure

```
skills/<skill-name>/
├── SKILL.md              # Required
├── scripts/              # Optional: executable scripts
├── references/           # Optional: detailed docs loaded on demand
└── assets/               # Optional: templates, schemas, data files
```

## Step 5: Write SKILL.md

Use this template:

```markdown
---
name: <skill-name>
description: <what it does and when to use it>
compatibility: <only if environment requirements exist>
---
# <Skill Title>

<One-line summary of what this skill does.>

## Step 1: <First action>

<Instructions with code blocks showing exact commands or tool invocations.>

## Step 2: <Next action>

<Continue step-by-step. Keep instructions concrete and unambiguous.>
```

### Optional frontmatter fields

| Field | When to use |
|-------|-------------|
| `license` | Skill has specific licensing terms |
| `compatibility` | Skill requires specific binaries, network, or environment |
| `metadata` | Extra key-value pairs (author, version) |
| `allowed-tools` | Pre-approve specific tools (experimental) |

## Step 6: Validate the skill

After writing, verify these rules:

### Frontmatter
- [ ] `name` present and follows naming rules
- [ ] `name` matches directory name
- [ ] `description` present and >= 20 characters
- [ ] `description` says WHEN to use the skill
- [ ] No unknown required fields

### Body
- [ ] Contains step-by-step instructions
- [ ] Code blocks use correct language tags
- [ ] Total SKILL.md under 500 lines
- [ ] No sensitive data (API keys, passwords, paths)

### Progressive disclosure
- [ ] Main SKILL.md < 5000 tokens
- [ ] Detailed references in `references/` directory
- [ ] Scripts in `scripts/` directory

## Writing Guidelines

1. **Optimize for weak LLMs** — use numbered steps, explicit commands, avoid ambiguity
2. **One task per code block** — do not chain unrelated commands
3. **Include examples** — show expected input and output where possible
4. **Security first** — never include secrets, credentials, or destructive commands without warnings
5. **Keep it focused** — one skill = one capability. Split broad topics into multiple skills
