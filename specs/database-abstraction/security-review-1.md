# Security Review: Database Abstraction Layer Spec

> **Reviewer**: rust-security-maintenance agent
> **Date**: 2026-03-28
> **Spec version**: Draft (2026-03-28)
> **Scope**: Full spec (`spec.md` sections 1-18), security-focused

---

## Executive Summary

The spec is well-structured and demonstrates security awareness, particularly in section 18 (Agent Identity) where defense-in-depth is explicitly called out. However, several gaps exist ranging from a Medium-severity credential exposure risk to multiple Low-severity design concerns that should be addressed before implementation begins. No Critical or High findings.

**Totals**: 0 Critical, 0 High, 3 Medium, 7 Low

---

## Findings

### F-01: Connection string credential exposure in config, logs, and errors

**Severity**: Medium
**Sections**: 6.1, 4.6

**Issue**: The `postgres_url` field (e.g., `postgres://user:pass@host:5432/zeph`) may contain plaintext credentials. The spec acknowledges vault resolution as an option but does not mandate it. Three exposure vectors remain unaddressed:

1. **Config file on disk**: If a user writes `postgres_url = "postgres://user:pass@..."` directly in `config.toml`, the password is stored in plaintext. The spec offers vault as an alternative but does not warn or prevent direct embedding.

2. **Log output**: The `DbConfig::connect()` path constructs and passes the URL string. If connection fails, `sqlx::Error` includes the connection URL in its `Display` implementation (confirmed in sqlx 0.8.x for `ConnectOptions` parse errors). This surfaces in `tracing::error!` output and potentially in error messages returned to the user via TUI/CLI.

3. **Error propagation**: `DbError` wraps `sqlx::Error`. If the wrapping is transparent (e.g., `#[error(transparent)]`), the credential-containing URL leaks through the entire error chain.

**Recommendations**:
- Add a `redact_url()` function in `zeph-db` that strips the password component from PostgreSQL URLs before any logging or error message construction. Apply it at the `DbConfig::connect()` boundary.
- In `DbError`, redact the inner `sqlx::Error` display. Do not use `#[error(transparent)]` for connection errors; instead, map to a generic "database connection failed" message and log the detailed (redacted) error at `debug` level.
- Add a startup warning when `postgres_url` contains an inline password (i.e., does not come from vault resolution): "Connection URL contains embedded credentials. Consider using vault resolution (ZEPH_DATABASE_URL) instead."
- Ensure the existing `zeph-core` redaction system (`RedactFilter`) covers `postgres://` URLs. Add a regex pattern for `postgres(ql)?://[^:]+:[^@]+@` to the security patterns list.

---

### F-02: `GlobalScope` lacks authorization boundary

**Severity**: Medium
**Sections**: 18.5, 18.9.4

**Issue**: `GlobalScope` bypasses all `agent_id` filtering and provides unfiltered access to every agent's data. The spec states it is "constructed explicitly by admin CLI commands, never by the normal agent loop" but relies entirely on code review convention to enforce this. The type itself has a trivial constructor (`GlobalScope::new(pool)`) with no authorization check.

**Specific risks**:
- Any code path with access to a `DbPool` can construct a `GlobalScope`. Since `AgentScope::pool()` returns `&DbPool`, and `DbPool` is `Clone`, any store holding an `AgentScope` can escalate to `GlobalScope` in three lines.
- Future contributors unfamiliar with the convention may use `GlobalScope` for convenience in non-admin paths (e.g., "I need to query across conversations for analytics").
- The spec proposes no runtime assertion, no feature gating, and no audit logging when `GlobalScope` is constructed.

**Recommendations**:
- Gate `GlobalScope` construction behind a marker type or capability token that is only available in admin CLI entry points. For example, a `pub struct AdminContext(())` that can only be created by the CLI bootstrap code, required as a parameter to `GlobalScope::new()`.
- Add `tracing::warn!` to `GlobalScope::new()` that logs the call site. This creates an audit trail even if the type is misused.
- Consider feature-gating `GlobalScope` behind an `admin-tools` feature that is not included in `default` or `full`. This makes accidental use in the agent loop a compile error unless explicitly opted in.
- At minimum, add `#[must_use]` and a doc comment with `# Safety` (conceptual, not `unsafe`) explaining the security implications.

---

### F-03: `sql!` macro rewriter is vulnerable to edge cases in dynamic SQL construction

**Severity**: Medium
**Sections**: 4.5, 9.2, 11.5, 18.5

**Issue**: The `rewrite_placeholders()` function performs a simple single-pass character scan, tracking only single-quote state. Several edge cases are not handled:

1. **Escaped quotes**: PostgreSQL allows `''` (doubled single quote) as an escape within string literals. The rewriter toggles `in_string` on each `'`, so `''` correctly toggles twice (back to out-of-string). However, `E'\''` (C-style escape) would incorrectly toggle. While C-style escapes are uncommon in this codebase, the rewriter does not reject them.

2. **Dollar-quoted strings**: PostgreSQL supports `$$...$$` and `$tag$...$tag$` string delimiters. The rewriter does not handle these. If a query contains `$$` strings (e.g., in PL/pgSQL function definitions within migrations), any `?` inside would be incorrectly rewritten.

3. **Double-dash and block comments**: `?` inside `-- comment` or `/* comment */` would be rewritten. While comments in runtime queries are rare, they are not impossible.

4. **Dynamic SQL construction in `agent_filter_clause()`**: Section 18.5 shows `format!("SELECT * FROM graph_entities WHERE name = ?{filter}")` where `filter` comes from `agent_filter_clause()`. The `?` in `filter` is a literal placeholder, but the `format!` output is then passed to `sql!()`. This works correctly because `sql!()` processes the final string. However, if a developer mistakenly applies `sql!()` to the base query and then appends `filter` (which already contains `?`), the second `?` would not be rewritten for PostgreSQL. The spec does not document this footgun.

5. **No injection via `rewrite_placeholders` itself**: The function is purely additive (replaces `?` with `$N`). It cannot introduce SQL injection on its own. The risk is in **incorrect rewriting** causing bind mismatch (wrong `$N` assignment), which would cause a runtime error, not a security breach. This is a correctness concern, not a direct injection vector.

**Recommendations**:
- Document explicitly that `rewrite_placeholders()` assumes no dollar-quoted strings or C-style escapes in runtime queries. Add a debug assertion that the input does not contain `$$`.
- Add unit tests for: escaped quotes (`''`), `--` comments containing `?`, `/* */` comments containing `?`, consecutive `?` markers.
- Document the `agent_filter_clause()` + `sql!()` interaction pattern explicitly. Require that `sql!()` is always applied to the final, fully-assembled query string, never to a partial fragment. Add a code example showing the correct and incorrect patterns.
- Consider a `#[cfg(debug_assertions)]` check in `rewrite_placeholders()` that counts `$N` markers in the input and warns if any exist (indicating double-rewrite).

---

### F-04: `agent_id` filter omission risk ("forgotten WHERE clause")

**Severity**: Low
**Sections**: 18.9.3, 18.5, 18.10

**Issue**: The spec correctly identifies this as the primary data leakage risk and proposes four mitigations (type-system via `AgentScope`, code review, custom clippy lint, integration test). The mitigations are adequate in principle but have gaps:

1. **`AgentScope::pool()` is public**: Any store method can call `self.scope.pool()` and execute a query without agent filtering. The type system does not prevent this. The proposed clippy lint is marked "future" with no implementation plan.

2. **The integration test (grep for `agent_id` in queries)** is fragile. Queries may use aliases, subqueries, CTEs, or dynamically assembled SQL that a text-based grep cannot reliably parse.

3. **No PostgreSQL RLS timeline**: RLS is listed as "future, optional" with no commitment. RLS is the only server-side enforcement that catches application-layer omissions.

**Assessment**: For the current threat model (trusted operators deploying their own agents), the proposed mitigations are reasonable. The `AgentScope` pattern is a significant improvement over passing raw pools. However, in a multi-tenant SaaS scenario (multiple customers sharing a database), the lack of server-side enforcement would be insufficient.

**Recommendations**:
- Make `AgentScope::pool()` return a wrapper type (`ScopedPool`) rather than `&DbPool`, where `ScopedPool` only exposes query methods that accept `&AgentScope` as a parameter. This makes raw pool access require an additional `.inner()` call with a doc comment explaining the risk.
- Promote the RLS recommendation from "future, optional" to "recommended for shared PostgreSQL deployments" and provide a sample RLS policy in the spec.
- The integration test should use AST-level analysis (parsing SQL with a lightweight SQL parser) rather than grep. Alternatively, add a compile-time attribute macro `#[agent_scoped]` on store methods that asserts (via proc macro) the presence of `agent_id` in the query string.

---

### F-05: Shared mode isolation model is ambiguous for writes

**Severity**: Low
**Sections**: 18.3, 18.5

**Issue**: The spec defines shared mode as "Queries see all rows regardless of agent_id. Writes use agent_id = NULL." This means:

1. Agent A writes a graph entity with `agent_id = NULL`.
2. Agent B writes a graph entity with `agent_id = NULL`.
3. Both see all rows.

This is correct for a collaborative knowledge graph. However, several scenarios are underspecified:

- **Conflict resolution**: If agent A and agent B both create a graph entity with the same `(name, entity_type)` (which has a UNIQUE constraint), who wins? The spec does not define whether this is an error, a last-write-wins upsert, or a merge.
- **Deletion in shared mode**: If agent A deletes a shared entity, agent B loses it too. There is no ownership tracking for shared rows.
- **Mode switching**: If a subsystem is switched from shared to isolated, existing rows have `agent_id = NULL`. These NULL rows become invisible to all agents (isolated queries filter by `agent_id = 'X'`, which does not match NULL). The spec does not describe a migration path for this mode switch.

**Assessment**: These are operational concerns, not security vulnerabilities per se. But the NULL-row invisibility issue after mode switch could cause data loss confusion.

**Recommendations**:
- Document conflict resolution strategy for shared tables (recommend: upsert with last-write-wins, log the conflict).
- Document the mode-switch migration: provide a SQL snippet that updates `agent_id = NULL` rows to a specific agent ID when switching from shared to isolated.
- Add a startup warning if shared tables contain rows with `agent_id = NULL` but the subsystem is configured as isolated: "Shared rows exist but subsystem is in isolated mode; these rows are invisible."

---

### F-06: `agent_id` validation allows characters safe for SQL but with operational risks

**Severity**: Low
**Sections**: 18.2

**Issue**: The `AgentId::parse()` validation allows `[a-z0-9_-]` with a 64-character limit. This is safe for SQL injection (parameterized queries are used). However:

- The hyphen (`-`) at certain positions can cause issues in shell contexts (e.g., `--agent-id` looks like a flag).
- The underscore is a SQL wildcard in `LIKE` patterns. If `agent_id` is ever used in a `LIKE` clause (not currently, but possible in admin search), `agent_id = 'test_agent'` would match `test%agent` patterns.

**Assessment**: Low risk. The character set is conservative and well-validated. SQL injection is impossible because `agent_id` is always bound as a parameter (confirmed in all spec examples). The `LIKE` concern is theoretical.

**Recommendations**:
- No changes needed to the character set.
- Add a note that `agent_id` must never be used in `LIKE` patterns without escaping underscores.
- The validation is sufficient as specified.

---

### F-07: PostgreSQL `ssl_mode` defaults to "prefer" rather than "require"

**Severity**: Low
**Section**: 6.1

**Issue**: The spec shows `postgres_ssl_mode = "prefer"` as the example value. In "prefer" mode, the client attempts TLS but falls back to plaintext if the server does not support it. This allows a network-level MITM to downgrade the connection.

**Assessment**: For local development (localhost), "prefer" is fine. For production deployments over a network, "prefer" is insecure. The spec does not distinguish between deployment contexts.

**Recommendations**:
- Default `postgres_ssl_mode` to `"require"` when `postgres_url` points to a non-localhost host.
- At minimum, document the security implications of each mode in the config comments:
  - `disable`: no TLS (development only)
  - `prefer`: tries TLS, falls back to plaintext (vulnerable to downgrade)
  - `require`: enforces TLS (recommended for production)
  - `verify-ca` / `verify-full`: enforce TLS with certificate validation (recommended for sensitive deployments)
- Add a startup warning when `ssl_mode` is `disable` or `prefer` and the host is not `localhost`/`127.0.0.1`.

---

### F-08: `testcontainers` image not pinned to digest

**Severity**: Low
**Section**: 16

**Issue**: The test fixture uses `Postgres::default().with_tag("16-alpine")`. This pulls `postgres:16-alpine` by tag, not by digest. A compromised Docker Hub account or registry MITM could replace the image. The CI job (section 16.6) also uses `postgres:16-alpine` by tag.

**Assessment**: Low severity in practice. The image is an official Docker Library image with automated security scanning. The attack requires compromising Docker Hub, which is a supply-chain risk shared by most projects. Test containers are ephemeral and do not process production data.

**Recommendations**:
- Pin the CI service container to a specific digest (e.g., `postgres:16-alpine@sha256:...`) and update it periodically via Dependabot or Renovate.
- For `testcontainers-rs` in dev-dependencies, pinning by digest is not natively supported by the `testcontainers-modules` crate. Accept the tag-based reference for test code but document the risk.
- Add a comment in the CI workflow explaining why the image is tagged (and a renovation schedule).

---

### F-09: Migration files as an attack surface

**Severity**: Low
**Section**: 5.1, 5.2

**Issue**: `sqlx::migrate!` embeds migration SQL at compile time from the `migrations/` directory. If an attacker can modify files in the migration directory (e.g., via a compromised dependency, a supply chain attack on the build pipeline, or a malicious PR), they can execute arbitrary DDL/DML at application startup.

**Assessment**: Low severity. This is a standard supply-chain risk shared by all applications using compile-time embedded migrations. The migration files are checked into git and reviewed in PRs. The attack requires write access to the repository or build environment, at which point the attacker can modify any source file.

**Recommendations**:
- No architectural change needed.
- Ensure migration files are covered by CODEOWNERS and require explicit reviewer approval.
- The existing PR review process is sufficient. Add a note to the migration porting guide (section 5.2) that PostgreSQL migrations must be reviewed with extra care since they support procedural SQL (`DO $$ ... $$`) which could execute arbitrary commands if PG extensions like `plpythonu` are installed.
- For defense-in-depth: run migrations with a PostgreSQL user that has DDL privileges but not superuser privileges. Document the recommended PostgreSQL role setup.

---

### F-10: No write-level audit trail for multi-agent database

**Severity**: Low
**Section**: 18 (general)

**Issue**: The spec adds `agent_id` to every row for query isolation but does not propose an audit trail for write operations. With multiple agents sharing a database:

- There is no record of which agent modified or deleted a shared row (e.g., a graph entity).
- The existing `zeph-tools` audit log is per-process (a local JSONL file) and does not extend to database operations.
- For shared tables, `agent_id` is NULL, so even the row data does not indicate who wrote it.

**Assessment**: Low severity for the current use case (collaborative agents under single operator). Higher concern if the system evolves toward multi-tenant or compliance-sensitive deployments.

**Recommendations**:
- Add a `created_by` column (nullable `TEXT`) to shared tables that records the `agent_id` of the writing agent, even in shared mode. This provides attribution without affecting query isolation.
- For sensitive operations (DELETE, bulk UPDATE), add a `db_audit_log` table or a PostgreSQL trigger that records `(timestamp, agent_id, table_name, operation, row_id)`. This can be opt-in via config.
- Document that the existing per-process audit log (`audit-test.jsonl`) does not cover database-level operations and should not be relied upon for multi-agent forensics.

---

## Summary Table

| ID | Severity | Section | Title |
|----|----------|---------|-------|
| F-01 | Medium | 6.1, 4.6 | Connection string credential exposure |
| F-02 | Medium | 18.5 | `GlobalScope` lacks authorization boundary |
| F-03 | Medium | 4.5, 9.2 | `sql!` macro rewriter edge cases |
| F-04 | Low | 18.9.3 | `agent_id` filter omission risk |
| F-05 | Low | 18.3 | Shared mode isolation ambiguity for writes |
| F-06 | Low | 18.2 | `agent_id` character validation |
| F-07 | Low | 6.1 | SSL mode default "prefer" is insecure |
| F-08 | Low | 16 | Docker image not pinned to digest |
| F-09 | Low | 5.1 | Migration files as attack surface |
| F-10 | Low | 18 | No write-level audit trail |

---

## Overall Assessment

The spec demonstrates good security awareness. The `AgentScope` / `GlobalScope` type separation, the `AgentId` newtype with validation, and the defense-in-depth discussion in section 18.9.3 are all positive patterns. The main gaps are:

1. **Credential handling** (F-01) is the most actionable finding. Connection string redaction should be designed into `zeph-db` from Phase 1, not retrofitted.

2. **`GlobalScope` authorization** (F-02) is a design-level concern that is cheapest to address now, before implementation begins.

3. **`sql!` rewriter robustness** (F-03) is a correctness concern more than a security concern, but incorrect placeholder rewriting could cause subtle data integrity issues.

The remaining findings are Low severity and appropriate for tracking as enhancement issues during or after implementation.

No findings warrant blocking the spec approval. All three Medium findings can be addressed by amending the spec before implementation begins.
