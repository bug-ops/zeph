# File Read Sandbox

The `[tools.file]` configuration section restricts which paths the agent is
allowed to read via the file tool. This provides a per-path sandbox that
complements the shell tool's `allowed_paths` setting.

## How It Works

Evaluation follows a **deny-then-allow** order:

1. If `deny_read` is non-empty and the path matches a deny pattern, access is
   denied.
2. If the path also matches an `allow_read` pattern, the deny is overridden and
   access is granted.
3. Empty `deny_read` means no read restrictions are applied.

All patterns are matched against the **canonicalized** path — absolute and with
all symlinks resolved — so symlink traversal cannot bypass the sandbox.

## Configuration

```toml
[tools.file]
# Glob patterns for paths denied for reading. Evaluated first.
deny_read = ["/etc/shadow", "/root/*", "/home/*/.ssh/*"]

# Glob patterns for paths allowed despite a deny match. Evaluated second.
allow_read = ["/etc/hostname"]
```

| Field        | Type             | Default | Description                                              |
|--------------|------------------|---------|----------------------------------------------------------|
| `deny_read`  | `Vec<String>`    | `[]`    | Glob patterns for paths to block. Empty = no restriction |
| `allow_read` | `Vec<String>`    | `[]`    | Glob patterns that override a `deny_read` match          |

## Glob Syntax

Patterns use standard glob syntax:

| Pattern     | Matches                                      |
|-------------|----------------------------------------------|
| `/etc/shadow` | Exact path `/etc/shadow`                   |
| `/root/*`   | All direct children of `/root/`              |
| `/home/*/.ssh/*` | `.ssh` contents for any user in `/home/` |
| `**`        | Any path segment, including nested           |

## Examples

### Deny all sensitive system files

```toml
[tools.file]
deny_read = [
    "/etc/shadow",
    "/etc/sudoers",
    "/root/*",
    "/home/*/.ssh/*",
    "/home/*/.gnupg/*",
]
```

### Deny all of `/etc` except a few safe entries

```toml
[tools.file]
deny_read  = ["/etc/*"]
allow_read = ["/etc/hostname", "/etc/os-release", "/etc/timezone"]
```

## Security Notes

- Patterns are applied to canonicalized paths. Symlinks pointing into a denied
  directory are still blocked after resolution.
- An empty `deny_read` list disables the sandbox entirely — all paths readable
  by the process are accessible to the file tool.
- `allow_read` has no effect when `deny_read` is empty.
- This setting does not restrict the shell tool. Use `[tools.shell]
  allowed_paths` for shell-level path restrictions.
