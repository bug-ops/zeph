# zeph-common

Shared utility functions for the Zeph workspace. Zero `zeph-*` dependencies.

## Modules

| Module | Description |
|--------|-------------|
| `text` | Unicode-safe string truncation (`truncate_chars`, `truncate_to_chars`, `truncate_to_bytes`, `truncate_to_bytes_ref`) |
| `net` | Network helpers (private IP detection, SSRF guard) |
| `sanitize` | Sanitization primitives |
| `treesitter` | Tree-sitter query constants and helpers for Rust/Python/JS/TS/Go (optional, feature `treesitter`) |

## Usage

```toml
[dependencies]
zeph-common = { workspace = true }

# With tree-sitter support:
zeph-common = { workspace = true, features = ["treesitter"] }
```

```rust
use zeph_common::text::{truncate_chars, truncate_to_chars};

let preview = truncate_chars("hello world", 5);      // "hello"
let owned   = truncate_to_chars("hello world", 5);   // "hello…"
```

## License

MIT OR Apache-2.0
