---
aliases:
  - Shared Primitives
  - Common Utilities
  - Security Primitives
tags:
  - sdd
  - spec
  - common
  - primitives
  - security
created: 2026-04-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/spec]]"
  - "[[038-vault/spec]]"
---

# Spec: Shared Primitives (`zeph-common`)

> [!info]
> Pure utility functions, security primitives, and strongly-typed identifiers shared by
> multiple `zeph-*` crates. Has no `zeph-*` dependencies; it is the lowest layer in the
> workspace dependency DAG.

## 1. Overview

### Problem Statement

Multiple crates in the workspace needed the same utility functions (text manipulation,
network helpers, hash utilities), security primitives (`Secret` with zeroize-on-drop,
trust levels, sanitization helpers), and strongly-typed identifiers (`ToolName`, `SessionId`,
`ToolDefinition`). Without a shared crate, these were duplicated or forced awkward
dependencies through higher-level crates.

### Goal

Provide a single foundation crate (`zeph-common`) that all other `zeph-*` crates may
depend on for shared types and utilities. The crate must remain free of `zeph-*` peer
dependencies to avoid cycles in the dependency DAG.

### Out of Scope

- Business logic (agent loop, LLM calls, memory operations)
- Configuration parsing (owned by `zeph-config`)
- Vault key storage and encryption (owned by `zeph-vault`)
- Database access

---

## 2. User Stories

### US-001: Secret Handling

AS A any `zeph-*` crate handling user credentials or API keys
I WANT a `Secret<T>` wrapper that redacts output in logs and zeroes memory on drop
SO THAT secrets never appear in debug output and are not retained in memory after use.

**Acceptance criteria:**

```
GIVEN a Secret wrapping a string value
WHEN Debug or Display is formatted
THEN the output is "[REDACTED]" not the actual value

GIVEN a Secret that goes out of scope
WHEN the drop runs
THEN the inner memory is overwritten via zeroize
```

### US-002: Strongly-Typed Tool Names

AS A tool registry or LLM response parser
I WANT a `ToolName` newtype backed by `Arc<str>`
SO THAT cloning is O(1) and accidental `String` conversion is prevented at compile time.

**Acceptance criteria:**

```
GIVEN a ToolName("shell")
WHEN it is cloned N times
THEN all clones share the same Arc allocation (O(1) clone cost)

GIVEN a ToolName
WHEN serialized to JSON
THEN the output is a plain string (serde transparent)
```

### US-003: Session Identity

AS A channel adapter or memory store
I WANT a `SessionId` newtype wrapping UUIDs
SO THAT session identifiers are type-safe and cannot be confused with tool names or other IDs.

**Acceptance criteria:**

```
GIVEN a SessionId
WHEN compared to another SessionId for the same session
THEN equality holds

GIVEN a SessionId generated via SessionId::new()
THEN it is globally unique (UUID v4)
```

### US-004: Text and Hash Utilities

AS A sanitizer or context builder
I WANT utility functions for text truncation, Unicode normalization, and BLAKE3 hashing
SO THAT each crate does not reimplement these operations with subtle differences.

**Acceptance criteria:**

```
GIVEN the same input string
WHEN hash::blake3_hex() is called multiple times
THEN the same hex string is returned (deterministic)
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `Secret::new()` is called THEN the system SHALL wrap the value in `Zeroizing<String>` so memory is overwritten on drop | must |
| FR-002 | WHEN `Secret` is formatted via `Debug` or `Display` THEN the system SHALL output `"[REDACTED]"` | must |
| FR-003 | WHEN `ToolName::new()` is called THEN the system SHALL store the string in an `Arc<str>` for O(1) clones | must |
| FR-004 | WHEN `ToolName` is serialized THEN the system SHALL use `#[serde(transparent)]` producing a plain JSON string | must |
| FR-005 | WHEN `SessionId::new()` is called THEN the system SHALL generate a UUID v4 | must |
| FR-006 | WHEN `hash::blake3_hex()` is called with the same input THEN the system SHALL return the same output (deterministic) | must |
| FR-007 | WHEN the `treesitter` feature is enabled THEN the system SHALL expose tree-sitter query constants and language helpers | should |
| FR-008 | WHEN `policy::PolicyLlmClient` is used THEN the system SHALL provide a minimal LLM interface for policy checks that does not depend on `zeph-llm` | must |
| FR-009 | WHEN `trust_level::SkillTrustLevel` is evaluated THEN the system SHALL express the Untrusted/Provisional/Trusted trust gradient | must |
| FR-010 | WHEN `sanitize` utilities are called THEN the system SHALL provide content sanitization helpers usable by any crate without depending on `zeph-sanitizer` | should |
| FR-011 | WHEN `BlockingSpawner::spawn_blocking_named` is called THEN the system SHALL dispatch the closure to the OS blocking thread pool with the given `Arc<str>` task name | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Isolation | `zeph-common` must have zero `zeph-*` peer dependencies |
| NFR-002 | Security | `Secret` must use `Zeroizing<String>` from the `zeroize` crate; Clone must not be derived |
| NFR-003 | Performance | `ToolName::clone()` must be O(1) via `Arc<str>` |
| NFR-004 | Correctness | `ToolName` must not implement `Deref<Target=str>` to prevent `.to_owned()` footgun |
| NFR-005 | Safety | No `unsafe` code |
| NFR-006 | Minimalism | This crate must not grow into a utility dumping ground; new additions require justification |

---

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `Secret` | Redacted, zeroized secret wrapper | `Zeroizing<String>` inner; no `Clone`; `expose() -> &str` |
| `ToolName` | Strongly-typed tool name label | `Arc<str>` inner; `as_str()`, `PartialEq<str>` |
| `SessionId` | Strongly-typed session identifier | UUID v4; `new()`, `Display` |
| `ToolDefinition` | Shared tool schema struct | Name, description, JSON schema blob |
| `SkillTrustLevel` | Trust gradient for skills | Enum: `Untrusted`, `Provisional`, `Trusted` |
| `PolicyLlmClient` | Minimal LLM interface for policy | Trait for content-policy checking without `zeph-llm` dependency |
| `PolicyMessage` | Message for policy evaluation | Role + content |
| `PolicyRole` | Role in policy context | `User`, `Assistant` |
| `BlockingSpawner` | Trait for dispatching blocking tasks by name | `spawn_blocking_named(name: Arc<str>, f: Box<dyn FnOnce() + Send + 'static>)` |

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Empty string passed to `Secret::new()` | Allowed; `expose()` returns `""` |
| `ToolName` constructed from empty string | Allowed; represents a degenerate tool name (validation is caller's responsibility) |
| `blake3_hex()` called with empty input | Returns deterministic BLAKE3 hash of empty bytes |
| `treesitter` feature disabled | Module is excluded from compilation; no dead-code warnings |
| `BlockingSpawner::spawn_blocking_named` with `Arc<str>` name | Name is forwarded to the implementation; no heap leak per call |

---

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Compile isolation | `cargo check -p zeph-common` succeeds with no `zeph-*` in dependency graph |
| SC-002 | Secret zeroization | Unit test confirms `Secret` does not implement `Clone` (compile_fail doctest) |
| SC-003 | ToolName O(1) clone | Unit test or doctest verifies `Arc` semantics |

---

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run -p zeph-common` after changes
- Keep the crate free of `zeph-*` peer dependencies

### Ask First
- Adding new public types (affects all dependent crates)
- Enabling new features in `Cargo.toml`
- Removing or renaming public items (breaking change before v1.0)

### Never
- Add a dependency on any `zeph-*` crate
- Derive `Clone` on `Secret`
- Use `unsafe` blocks
- Use `&'static str` or `Box::leak` for task names — always `Arc<str>`

---

## 9. Open Questions

None.

---

## 10. See Also

- [[constitution]] — project principles
- [[001-system-invariants/spec]] — system-wide invariants
- [[010-security/spec]] — security model that `zeph-common` primitives enforce
- [[038-vault/spec]] — vault crate that uses `Secret` from this crate
- [[039-background-task-supervisor/spec]] — `TaskSupervisor` implements `BlockingSpawner`
- [[017-index/spec]] — `CodeIndexer` consumes `BlockingSpawner` via `with_spawner()`
- [[MOC-specs]] — all specifications
