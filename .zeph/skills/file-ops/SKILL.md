---
name: file-ops
description: File system operations — list directory contents, find files by name or glob pattern, search text inside files, read file contents, analyze file size and type, count lines, compare files with diff.
---
# File Operations

Before running any file system command, detect the user's OS and use the matching reference:

- **Linux** — `references/linux.md` (GNU coreutils, grep, find, ripgrep, fd)
- **macOS** — `references/macos.md` (BSD find/stat/sed, Spotlight, Homebrew tools)
- **Windows** — `references/windows.md` (PowerShell: Get-ChildItem, Select-String, Get-Content)

OS detection:
```bash
uname -s 2>/dev/null || echo Windows
```

## Path specification

### Relative vs absolute

| Syntax | Meaning |
|--------|---------|
| `.` | Current working directory |
| `..` | Parent directory |
| `./src` | `src/` relative to cwd |
| `../../config` | Two levels up, then into `config/` |
| `/home/user/project` | Absolute path (Unix) |
| `C:\Users\user\project` | Absolute path (Windows) |
| `~/Documents` | Home directory shortcut (Unix shells expand `~`) |

### Quoting paths with spaces

Paths containing spaces, parentheses, or special characters must be quoted:
- `"./my project/src"` — double quotes (variables expand inside)
- `'./my project/src'` — single quotes (literal, no expansion)
- `./my\ project/src` — backslash escaping per character

### Trailing slash behavior

- `path/to/dir` — refers to the directory itself
- `path/to/dir/` — refers to the contents (relevant for `rsync`, `cp -r`, tab completion)

## Glob patterns

Globs are shell-level filename matching patterns. They do NOT use regex syntax.

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `*` | any sequence of characters (except `/`) | nothing across directories |
| `?` | exactly one character | empty string |
| `[abc]` | one of `a`, `b`, or `c` | `d`, `ab` |
| `[a-z]` | one lowercase letter | `A`, `1` |
| `[!abc]` or `[^abc]` | any char except `a`, `b`, `c` | `a` |
| `**` | any depth of directories (in tools that support it) | — |

### Common glob examples

| Task | Pattern |
|------|---------|
| All Rust source files | `*.rs` |
| All TOML configs | `*.toml` |
| All test files | `*_test.*` or `test_*.*` |
| All hidden files | `.*` |
| All files in `src/` subtree | `src/**/*` (requires `**` support) |
| Images by extension | `*.{png,jpg,svg}` (brace expansion, bash/zsh) |
| Single-char extension | `*.?` |
| Files starting with `config` | `config*` |
| Logs with date suffix | `app-2025-??-??.log` |

### Brace expansion (bash/zsh only, not POSIX)

`{a,b,c}` expands to multiple alternatives before glob matching:
- `*.{rs,toml}` → `*.rs *.toml`
- `{src,tests}/**/*.rs` → `src/**/*.rs tests/**/*.rs`
- `file.{bak,orig,tmp}` → `file.bak file.orig file.tmp`

## Regex patterns for content search

Regex is used for searching _inside_ file contents (grep, ripgrep, Select-String). Not to be confused with globs.

### Basic syntax

| Pattern | Matches |
|---------|---------|
| `.` | any single character |
| `\d` | digit `[0-9]` (PCRE/ripgrep; use `[0-9]` in POSIX) |
| `\w` | word character `[a-zA-Z0-9_]` |
| `\s` | whitespace (space, tab, newline) |
| `\b` | word boundary |
| `^` | start of line |
| `$` | end of line |
| `[abc]` | character class |
| `[^abc]` | negated character class |
| `(a\|b)` | alternation |
| `(...)` | grouping |

### Quantifiers

| Pattern | Meaning |
|---------|---------|
| `*` | zero or more of preceding |
| `+` | one or more of preceding |
| `?` | zero or one of preceding |
| `{3}` | exactly 3 |
| `{2,5}` | 2 to 5 |
| `{3,}` | 3 or more |

### Common regex examples

| Task | Pattern |
|------|---------|
| Function definitions (Rust) | `fn\s+\w+\(` |
| Function definitions (Python) | `def\s+\w+\(` |
| Function definitions (JS/TS) | `(function\|const\|let)\s+\w+\s*[=(]` |
| Import/use statements | `^(use\|import\|from)\s+` |
| TODO/FIXME/HACK markers | `(TODO\|FIXME\|HACK)` |
| IP addresses (v4, approximate) | `\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}` |
| Email addresses (simple) | `[\w.+-]+@[\w-]+\.[\w.]+` |
| URLs | `https?://[^\s"'>]+` |
| Struct/class definitions | `(struct\|class\|interface)\s+\w+` |
| Error/panic patterns | `(error\|Error\|panic\|PANIC\|unwrap\(\))` |
| Environment variables | `[A-Z_]{2,}=[^\s]+` |
| Semantic version | `\d+\.\d+\.\d+` |
| Lines with trailing whitespace | `\s+$` |
| Empty lines | `^$` |
| Lines longer than 100 chars | `.{101,}` |
| Hex color codes | `#[0-9a-fA-F]{3,8}` |
| SQL-like queries | `(SELECT\|INSERT\|UPDATE\|DELETE)\s+` |
| JSON keys | `"[^"]+"\s*:` |

### Escaping special characters

To search for literal `.`, `*`, `+`, `?`, `(`, `)`, `[`, `]`, `{`, `}`, `|`, `^`, `$`, `\` — prefix with backslash:
- Find `fn()` literally → `fn\(\)`
- Find `[TODO]` literally → `\[TODO\]`
- Find `*.rs` literally → `\*\.rs`
- Find `$HOME` literally → `\$HOME`

## Directories to exclude

Common directories that should be excluded from search to avoid noise and improve performance:

| Directory | Context |
|-----------|---------|
| `target/` | Rust build artifacts |
| `node_modules/` | JavaScript dependencies |
| `.git/` | Git internal data |
| `__pycache__/` | Python bytecode cache |
| `.venv/`, `venv/` | Python virtual environments |
| `build/`, `dist/` | Generic build output |
| `.next/`, `.nuxt/` | Framework build caches |
| `vendor/` | Vendored dependencies (Go, PHP, Ruby) |
| `.idea/`, `.vscode/` | IDE configuration |
| `coverage/`, `.nyc_output/` | Test coverage reports |

## Available operations

### List directory contents
Show files and directories at a given path with details (permissions, size, timestamps). Support recursive listing with depth limits.

### Find files by name or pattern
Locate files by exact name, extension, glob/regex pattern. Filter by modification time, size, type (file/directory). Exclude build artifacts and VCS directories.

### Search text inside files
Find lines matching a pattern across files. Support case-insensitive search, regex, context lines, file type filtering, directory exclusions, word-boundary matching, inverted matching, and match counting.

### Read file contents
Display full file or specific portions: first/last N lines, line range, with line numbers.

### File metadata and analysis
Determine file type/encoding, line/word/byte counts, size, disk usage. Find largest files, count files by extension.

### Compare files
Show differences between two files in side-by-side or unified (patch) format.

### Permissions and ownership
View and modify file permissions. Find files by owner or permission bits.

### Checksums
Compute file hashes (SHA-256, MD5) for integrity verification.
