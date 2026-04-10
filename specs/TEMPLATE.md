# Spec Template

Use this template when creating new specifications in `.local/specs/NNN-feature-name/spec.md`.

---

## Frontmatter

```yaml
---
aliases:
  - [Alternative names for search]
  - [Short acronym if applicable]
tags:
  - sdd
  - spec
  - [domain: core|llm|memory|skills|tools|channels|tui|mcp|security|orchestration|protocols|config|database|benchmarking]
  - [optional: cross-cutting|infra|contract|experimental]
created: [YYYY-MM-DD]
status: [draft|approved|deprecated]
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[related-spec-if-applicable]]"
---
```

---

## Content Structure

### Required Sections

```markdown
# Spec: [Feature Name]

> [!info]
> [1-2 sentence overview of what this spec defines and why it matters]

## Sources

### External
- [Academic papers, RFCs, or external references with authors and URLs]
- Link format: `[Title (Author, Year)](URL)`

### Internal
| File | Contents |
|---|---|
| `crates/.../file.rs` | [Module responsibility] |
| `crates/.../types.rs` | [Type definitions] |

---

## 1. Overview

### Problem Statement
[What problem does this feature solve? Why does it matter? Who is affected?]

### Goal
[What is the intended outcome? What will be true when this is done?]

### Out of Scope
[What is explicitly NOT included in this spec?]

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN [condition] THE SYSTEM SHALL [behavior] | must |
| FR-002 | WHEN [condition] THE SYSTEM SHALL [behavior] | should |

---

## 3. Architecture / Data Model / Design Principles

[Describe structural design, data flow, algorithmic approach, or design decisions.
Adapt section name to the feature (e.g., "Architecture", "Data Model", "Design Principle").]

---

## 4. Key Invariants

### Always (without asking)
- [Guarantee 1 — must be maintained]
- [Guarantee 2 — non-negotiable contract]

### Ask First
- [Requires approval for deviation]
- [Architectural decision needed to change]

### Never
- [Forbidden patterns or behaviors]
- [Anti-patterns that violate the contract]

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| [Boundary condition 1] | [How system responds] |
| [Error case 1] | [Recovery strategy] |
| [Concurrent access] | [Handling strategy] |

---

## 6. Success Criteria

Measurable indicators that the feature works:

- [ ] [Test/validation 1]
- [ ] [Test/validation 2]
- [ ] [Integration test]
- [ ] [Performance threshold met]

---

## 7. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[related-spec]] — Related architectural decisions
- [[related-spec-2]] — Dependencies or extensions
```

---

## Guidelines

### Tagging Strategy

**Tier 1 (Type)**: Always include exactly one
- `sdd` — specification document
- `moc` — map of content
- `constitution` — project principles

**Tier 2 (Phase)**: For SDD specs, include one
- `spec` — Phase 1 (requirements)
- `plan` — Phase 2 (technical design)
- `tasks` — Phase 3 (implementation tasks)
- `research` — gap analysis or investigation
- `report` — findings document

**Tier 3 (Domain)**: Include appropriate domains
- `core` — zeph-core subsystem
- `llm` — LLM providers and routing
- `memory` — Memory and graph systems
- `skills` — Skills and self-learning
- `tools` — Tool execution
- `channels` — I/O channels
- `tui` — TUI dashboard
- `mcp` — Model context protocol
- `security` — Security and isolation
- `orchestration` — Multi-agent planning
- `protocols` — A2A, ACP, handoff
- `config` — Configuration
- `database` — Data persistence
- `benchmarking` — Performance measurement

**Tier 4 (Cross-cutting)**: Optional, add as needed
- `contract` — Defines API contract or invariant
- `infra` — Infrastructure or runtime
- `cross-cutting` — Applies to multiple subsystems
- `experimental` — Early-stage or research

### Naming Convention

**File structure**:
```
.local/specs/NNN-feature-name/
├── spec.md      (Phase 1: specification)
├── plan.md      (Phase 2: technical plan)
└── tasks.md     (Phase 3: implementation tasks)
```

**Directory name**: `NNN-feature-name` where:
- `NNN` = sequential ID (001, 002, ..., 034)
- `feature-name` = kebab-case, short and descriptive

### Content Guidelines

**Frontmatter aliases**: Provide alternatives for Obsidian search
- Full name: "Feature: Multi-Model Design"
- Short form: "Multi-Model Design"
- Acronym: "MMD" (if applicable)

**Related links**: Link to dependent and influencing specs
- Always include `[[MOC-specs]]`
- Link to `[[constitution]]` if the spec defines project-wide contracts
- Link to system invariants: `[[001-system-invariants/spec#Section]]`
- Link to related features that depend on or extend this one

**Sources section**: Credit academic papers and references
- External: peer-reviewed papers, RFCs, design documents
- Internal: crate structure and file locations

**Key Invariants**: Define non-negotiable contracts
- "Always" = preconditions and post-conditions that must hold
- "Ask First" = decisions that need explicit approval to override
- "Never" = patterns that violate the contract

**Edge Cases**: Enumerate boundary conditions and failure modes
- Input validation (empty, huge, null, concurrent)
- Resource exhaustion (memory, CPU, database)
- Partial failures (external service down, timeout)
- State conflicts (concurrent writes, race conditions)

### Length and Depth

- **Minimal spec** (3-5 sections): Simple features, single responsibility
- **Standard spec** (7-10 sections): Most features, multi-component systems
- **Complex spec** (10+ sections): Large architectural systems (like 004-memory, 006-tools, 008-mcp)

For complex systems, break major subsystems into `## Subsystem Name` sub-sections with their own Key Invariants.

---

## Example Minimal Spec

```markdown
---
aliases:
  - Feature X
tags:
  - sdd
  - spec
  - core
created: 2026-04-10
status: draft
related:
  - "[[MOC-specs]]"
---

# Spec: Feature X

> [!info]
> Brief description of Feature X and its purpose.

## Sources

### External
- [Reference 1](URL)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-core/src/module.rs` | Implementation |

---

## 1. Overview

### Problem Statement
[Why this feature is needed]

### Goal
[What success looks like]

### Out of Scope
[What is not included]

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN ... THE SYSTEM SHALL ... | must |

---

## 3. Architecture

[Design and structure]

---

## 4. Key Invariants

### Always
- [Contract 1]

### Never
- [Anti-pattern 1]

---

## 5. Edge Cases

| Case | Behavior |
|------|----------|
| Edge 1 | Response |

---

## 6. Success Criteria

- [ ] Test 1 passes
- [ ] Integration test passes

---

## 7. See Also

- [[MOC-specs]]
- [[related-spec]]
```

---

## When to Create a New Spec

1. **New feature** affecting architecture or multiple crates
2. **Breaking change** to existing systems
3. **New integration point** (channel, protocol, tool type)
4. **Cross-cutting concern** (security, performance, config)
5. **Long-term design decision** that affects many contributors

Do NOT create specs for:
- Bug fixes (log in GitHub issue)
- Refactorings without architectural impact (code review)
- Minor config additions (document in code comments)

---

## Checklist Before Finalizing

- [ ] Frontmatter complete (aliases, tags, created, status, related)
- [ ] No YAML syntax errors
- [ ] All wikilinks use quoted format: `"[[link]]"`
- [ ] External sources have proper citations with URLs
- [ ] Key Invariants clearly distinguish Always / Ask First / Never
- [ ] Success Criteria are measurable (checkboxes or metrics)
- [ ] See Also section includes [[MOC-specs]]
- [ ] No `[NEEDS CLARIFICATION]` markers (or all resolved)
- [ ] Spell-checked and proofread
- [ ] Added to [[MOC-specs]] with unique ID

---

## Quick Reference

**EARS format for requirements**:
```
WHEN [condition] THE SYSTEM SHALL [behavior]
```

**Givenhen-Then format for acceptance criteria**:
```
GIVEN [precondition]
WHEN [action]
THEN [expected result]
```

**Wikilink format**:
```markdown
[[path/to/spec]]           # internal link
[[path/to/spec|display]]   # with custom text
[[spec#Section]]           # link to section
"[[link]]"                 # quoted in frontmatter related field
```

**Callout format**:
```markdown
> [!info] Title
> Content

> [!warning]
> Important warning

> [!example]
> Code example

> [!note]
> General note
```
