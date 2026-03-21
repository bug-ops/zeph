# Handoff Protocol Specification

**Status:** Active
**Issue:** #2023

## Overview

Inter-agent communication in Zeph uses a skill-based YAML protocol. Subagents work in isolated contexts and cannot see each other's conversations. Structured YAML handoff files in `.local/handoff/` enable context propagation between agents.

The protocol is defined entirely in the `rust-agent-handoff` skill (`.zeph/skills/rust-agent-handoff/`). There is no code-level validation or typed Rust structs for handoffs — the skill documentation is the contract.

## Architecture

```
Parent Agent (orchestrator)
    │
    ├── Task(agent-A): "Do X"
    │       └── writes .local/handoff/{id}.yaml
    │       └── returns handoff path
    │
    ├── reads handoff, decides next step
    │
    └── Task(agent-B): "Do Y. Handoff: <path>"
            └── reads parent handoff
            └── writes own handoff
            └── returns handoff path
```

Agents never call each other directly. The parent reads returned handoff paths and orchestrates the next step.

## Handoff File Format

**Location:** `.local/handoff/{id}.yaml`
**Naming:** `{timestamp}-{agent}.yaml` where timestamp = `%Y-%m-%dT%H-%M-%S`
**ID constraint:** filename (without `.yaml`) must equal the `id` field inside the YAML

### Base Schema

```yaml
id: "2025-01-09T14-30-45-architect"
parent: "2025-01-09T14-00-00-developer"  # or null, or array for parallel merge
agent: architect
timestamp: "2025-01-09T14:30:45"
status: completed  # completed | blocked | needs_discussion

context:
  task: "Original task description"
  phase: "01"

output:
  # Agent-specific fields (see references/*.md)

next:
  agent: rust-developer
  task: "Task description for next agent"
  priority: high  # high | medium | low
  acceptance_criteria:
    - "Criterion 1"
```

### Agent Suffixes

| Agent | Suffix |
|-------|--------|
| rust-architect | `architect` |
| rust-developer | `developer` |
| rust-testing-engineer | `testing` |
| rust-performance-engineer | `performance` |
| rust-security-maintenance | `security` |
| rust-code-reviewer | `review` |
| rust-cicd-devops | `cicd` |
| rust-debugger | `debug` |
| rust-critic | `critic` |

## Agent Lifecycle

### On Startup

1. Generate timestamp: `TS=$(date +%Y-%m-%dT%H-%M-%S)`
2. Read agent-specific schema from `references/<agent>.md`
3. If handoff path provided: read the file and follow parent chain recursively
4. Proceed with task

### On Completion

1. Write handoff YAML to `.local/handoff/{TS}-{agent}.yaml`
2. Return handoff path and recommended next agent in response

## Role-Specific Schemas

Each agent type has a reference file defining its expected `output` structure:

- `references/architect.md` — design decisions, file map, constraints
- `references/developer.md` — files changed, test count delta
- `references/testing.md` — test plan, coverage
- `references/performance.md` — benchmarks, hotspots
- `references/security.md` — vulnerabilities, audit results
- `references/review.md` — findings by severity
- `references/cicd.md` — pipeline config, checks
- `references/debug.md` — root cause, reproduction steps
- `references/critic.md` — critique by category

These files are documentation for agents — not parsed by code at runtime.

## Parallel Work and Context Merge

When multiple agents work in parallel, the merging agent receives an array of parent IDs:

```yaml
parent:
  - "2025-01-09T15-00-00-developer"
  - "2025-01-09T15-00-00-testing"
```

The agent reads all parent handoffs and synthesizes the combined context.

## Key Invariants

- YAML handoff files are the single communication channel between subagents
- Agents NEVER call each other directly — parent orchestrates all routing
- Every agent MUST write a handoff file before finishing, even if blocked
- Every agent MUST return the handoff path in its response
- The `id` field MUST match the filename (without `.yaml`)
- Parent chain reading is recommended but not enforced at code level
- No Rust structs or compile-time validation exist for handoff content — the skill documentation is the contract
- All handoff files go to `.local/handoff/` — never to source directories

## Historical Context

PRs #2076 and #2078 introduced typed `HandoffContext` Rust structs with compile-time validation (inspired by MAST research on multi-agent coordination failures). This was reverted in PR #2082 — the skill-based YAML protocol remains the active approach. Future hardening may revisit typed validation with a simpler design.
