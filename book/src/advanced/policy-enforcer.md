# Policy Enforcer

The policy enforcer provides declarative, TOML-based authorization rules that are evaluated before any tool call executes. It is the outermost layer of the tool execution stack, sitting above `TrustGateExecutor`.

> **Feature flag:** `policy-enforcer` (optional, included in `full`). The feature is off by default and adds no overhead when disabled.

## Security Model

- **Deny-wins semantics:** deny rules are evaluated first across all rules. If any deny rule matches, the call is blocked regardless of allow rules.
- **Insertion-order independent:** the order of rules in the config does not affect the deny-wins outcome.
- **Path normalization (CRIT-01):** path parameters are lexically normalized before matching — `/tmp/../etc/passwd` becomes `/etc/passwd`. This prevents traversal bypasses. No filesystem I/O occurs during normalization.
- **Tool name normalization (CRIT-02):** tool names are lowercased and trimmed before glob matching, preventing aliasing via mixed case.
- **Generic LLM error (MED-03):** when a call is blocked, the LLM receives only `"Tool call denied by policy"`. The rule trace goes to the audit log only.
- **Compile-time limits:** max 256 rules, max 1024 bytes per regex pattern. Prevents OOM from malformed policy files.
- **User confirmation bypass prevention (MED-04):** `execute_tool_call_confirmed` also enforces policy. User confirmation does not bypass declarative authorization.

## Configuration

```toml
[tools.policy]
enabled = true
default_effect = "deny"     # Fallback when no rule matches: "allow" or "deny"
# policy_file = "policy.toml"  # Optional external rules file (overrides inline rules)
```

### Inline Rules

```toml
[[tools.policy.rules]]
effect = "deny"             # "allow" or "deny"
tool = "shell"              # Glob pattern for tool name (case-insensitive)
paths = ["/etc/*", "/root/*"]  # Path globs; matched after lexical normalization
# trust_level = "verified"  # Optional: rule only applies when trust <= this level
# args_match = ".*sudo.*"   # Optional: regex matched against individual string param values

[[tools.policy.rules]]
effect = "allow"
tool = "shell"
paths = ["/tmp/*"]
```

### External Policy File

When `policy_file` is set, rules are loaded from that TOML file instead of inline `[[tools.policy.rules]]`. The file is read once at startup. Format:

```toml
[[rules]]
effect = "deny"
tool = "shell"
paths = ["/etc/*"]

[[rules]]
effect = "allow"
tool = "shell"
paths = ["/tmp/*"]
```

File size is capped at 256 KiB.

## CLI Flag

```bash
zeph --policy-file /path/to/policy.toml
```

This overrides `tools.policy.policy_file` from the config file and enables the policy enforcer (`enabled = true`).

## Slash Commands

| Command | Description |
|---------|-------------|
| `/policy status` | Show whether policy is enabled, rule count, default effect, and optional file path. |
| `/policy check <tool> [args_json]` | Dry-run evaluation. Returns Allow or Deny with the matching rule trace. |

Examples:

```
/policy status
/policy check shell {"file_path":"/etc/passwd"}
/policy check bash {"command":"sudo rm -rf /"}
```

## Rule Fields

| Field | Type | Description |
|-------|------|-------------|
| `effect` | `"allow"` or `"deny"` | Action when this rule matches. |
| `tool` | glob string | Tool name pattern (case-insensitive). `*` matches any tool. |
| `paths` | `[string]` | Optional path globs. Extracted from `file_path`, `path`, `directory`, `dest`, `source`, and absolute paths in `command`. |
| `trust_level` | trust level string | Optional maximum trust level for this rule to apply (`"trusted"`, `"verified"`, `"quarantined"`, `"blocked"`). |
| `args_match` | regex string | Optional regex matched against each individual string param value. |
| `env` | `[string]` | Optional list of environment variable names that must be present. |

## Examples

### Allow-list: only `/tmp` is writable

```toml
[tools.policy]
enabled = true
default_effect = "deny"

[[tools.policy.rules]]
effect = "allow"
tool = "shell"
paths = ["/tmp/*"]

[[tools.policy.rules]]
effect = "allow"
tool = "file_*"
paths = ["/tmp/*"]
```

### Block `sudo` commands

```toml
[[tools.policy.rules]]
effect = "deny"
tool = "shell"
args_match = ".*sudo.*"
```

### Restrict quarantined callers to read-only

```toml
[[tools.policy.rules]]
effect = "deny"
tool = "shell"
trust_level = "quarantined"

[[tools.policy.rules]]
effect = "allow"
tool = "file_read"
trust_level = "quarantined"
paths = ["/tmp/*", "/home/*"]
```

## Wiring Order

```
PolicyGateExecutor       ← outermost (policy check)
  └─ TrustGateExecutor   ← trust level enforcement
       └─ CompositeExecutor
            └─ ShellExecutor / FileExecutor / ...
```

Policy is checked before trust level gating. A deny decision short-circuits the entire chain.

## Audit Logging

When an `[tools.audit]` logger is attached, every policy decision (allow and deny) is recorded with timestamp, tool name, truncated params, and result. Deny entries include the full rule trace in the `reason` field — this trace is never sent to the LLM.

```toml
[tools.audit]
enabled = true
destination = ".zeph/audit.jsonl"
```

## OAP Authorization Config

A separate `[tools.authorization]` section provides a supplementary authorization layer that sits alongside the policy enforcer. Unlike the inline `[[tools.policy.rules]]`, authorization rules are merged into `PolicyEnforcer` at startup after policy rules (policy takes precedence). This lets you split operational rules (in `[tools.policy]`) from access-control rules (in `[tools.authorization]`) across different config files or config management systems.

```toml
[tools.authorization]
enabled = true

[[tools.authorization.rules]]
effect    = "deny"
tool      = "bash"
args_match = ".*sudo.*"

[[tools.authorization.rules]]
effect = "allow"
tool   = "read"
paths  = ["/home/user/*"]
```

Rule fields are identical to `[[tools.policy.rules]]`. The `capabilities` field on `PolicyRuleConfig` is reserved for future use when tools expose structured capability metadata (M4).

> [!NOTE]
> Authorization rules do not replace policy rules — they extend them. The wiring order is: `[tools.policy.rules]` first, then `[tools.authorization.rules]`. First-match-wins semantics apply across the merged set.

## Migrate Config

When upgrading from a config that predates policy enforcer support, run:

```bash
zeph --migrate-config --in-place
```

This adds `[tools.policy]` with `enabled = false` as a commented-out block so you can discover and enable it without manual editing.
