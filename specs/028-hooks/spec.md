# Spec: Reactive Hooks

> **Status**: Implemented
> **Crate**: `zeph-core`

## Sources

### Internal

| File | Contents |
|---|---|
| `crates/zeph-core/src/hooks/mod.rs` | `HookRunner`, hook dispatch, env var injection |
| `crates/zeph-core/src/hooks/file_watcher.rs` | `FileChangeWatcher` via `notify-debouncer-mini` |
| `crates/zeph-core/src/tools/native/cwd.rs` | `set_working_directory` native tool |
| `crates/zeph-core/src/config/types/hooks.rs` | `HooksConfig`, `HookEntry` |

---

## Overview

Reactive hooks allow operators to run shell commands (or scripts) in response to
agent lifecycle events. Two event types are supported in v0.18.2:

| Event | Trigger |
|---|---|
| `cwd_changed` | Agent working directory changes (via `set_working_directory` tool) |
| `file_changed` | A watched file or directory subtree is modified on disk |

Hooks are defined as arrays under `[hooks]` in `config.toml`. Each entry specifies
the shell command and optional glob filters. Multiple hooks per event are supported
and run sequentially.

---

## Config

```toml
[[hooks.cwd_changed]]
command = "echo 'cwd changed to $ZEPH_NEW_CWD'"
# No additional filter fields for cwd_changed

[[hooks.file_changed]]
command = "cargo check"
glob = "src/**/*.rs"    # optional; only fire when changed path matches this glob
```

Multiple entries of the same type are permitted:

```toml
[[hooks.file_changed]]
command = "cargo fmt --check"
glob = "**/*.rs"

[[hooks.file_changed]]
command = "python check_config.py"
glob = "config.toml"
```

---

## Environment Variables

Hooks receive context via environment variables injected into the shell command:

| Variable | Event | Value |
|---|---|---|
| `ZEPH_OLD_CWD` | `cwd_changed` | Previous working directory (absolute path) |
| `ZEPH_NEW_CWD` | `cwd_changed` | New working directory (absolute path) |
| `ZEPH_CHANGED_PATH` | `file_changed` | Absolute path of the changed file or directory |

These variables are always set for the relevant event type. For `file_changed`,
`ZEPH_CHANGED_PATH` is the canonicalized absolute path of the change event.

---

## `set_working_directory` Tool

`set_working_directory` is a native tool (always available, no feature flag) that
changes the agent's working directory and fires all `cwd_changed` hooks.

```json
{
  "tool": "set_working_directory",
  "params": { "path": "/absolute/or/relative/path" }
}
```

- Relative paths are resolved relative to the current working directory
- The new directory must exist; non-existent paths produce `ToolError::PermanentFailure`
- After a successful directory change, all `[[hooks.cwd_changed]]` hooks are invoked

---

## FileChangeWatcher

`FileChangeWatcher` uses the `notify-debouncer-mini` crate to watch paths for
filesystem changes. It runs in a background tokio task and fires `file_changed`
hooks via the hook runner.

### Debouncing

Events are debounced with a fixed delay (default 200 ms) to coalesce rapid
file-system events (e.g., editor save sequences that touch a file multiple times).
Only the last event per path within the debounce window is delivered.

### Glob Filtering

When a `[[hooks.file_changed]]` entry has a `glob` field, only change events
whose `ZEPH_CHANGED_PATH` matches the glob are forwarded to that hook's command.
Hooks without a `glob` field fire on every change event.

---

## Hook Execution

Hooks are run as shell commands via the same shell executor infrastructure as the
`bash` tool. Hook commands run with the agent's current working directory as the
working directory.

- Hook stdout and stderr are logged at `DEBUG` level — not injected into agent context
- Hook exit code is logged; non-zero exit code emits a `WARN`
- Hook execution is non-blocking from the agent's perspective: hooks run in a
  background task spawned by the hook runner
- Hook failures are non-fatal — a failing hook does not abort the event or the turn

---

## Key Invariants

- Hook commands execute with the blocked-command list applied — dangerous shell patterns are prevented
- `ZEPH_OLD_CWD`, `ZEPH_NEW_CWD`, `ZEPH_CHANGED_PATH` are always absolute, canonicalized paths
- Hooks do NOT receive agent conversation context — they are environment-aware but not LLM-aware
- `set_working_directory` must fire `cwd_changed` hooks synchronously before returning `ToolOutput` — the new cwd must be committed first
- `FileChangeWatcher` debounce is mandatory — raw filesystem events must never bypass it
- Hook execution is never on the agent hot path — always background task
- NEVER inject hook stdout into the agent's conversation context
- NEVER run hooks with elevated privileges — they inherit the agent process permissions only
- If `[hooks]` section is absent from config, all hook lists are empty and no hooks fire — zero-cost when unused

---

## Agent Boundaries

### Always (without asking)
- Apply the shell blocked-command list to hook commands
- Log hook stdout/stderr at DEBUG, non-zero exit at WARN
- Canonicalize paths before setting `ZEPH_*` env vars

### Ask First
- Adding new hook event types (requires new `FileChanged` / `CwdChanged` variant)
- Changing debounce interval (affects reactivity vs. noise trade-off)

### Never
- Inject hook output into LLM context
- Block the agent turn on hook execution
- Skip the shell blocklist for hook commands
