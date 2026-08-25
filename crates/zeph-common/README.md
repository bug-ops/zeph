# zeph-common

[![Crates.io](https://img.shields.io/crates/v/zeph-common)](https://crates.io/crates/zeph-common)
[![docs.rs](https://img.shields.io/docsrs/zeph-common)](https://docs.rs/zeph-common)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Shared utility functions and security primitives for the Zeph workspace. Zero `zeph-*` dependencies — safe to depend on from any crate.

## Overview

Provides foundational utilities used across multiple Zeph crates: Unicode-safe string truncation, network SSRF guards, sanitization primitives, and optional tree-sitter query helpers. Having these in a dedicated leaf crate prevents circular dependencies and ensures the utilities are tested in isolation.

## Key modules

| Module | Description |
|--------|-------------|
| `text` | Unicode-safe string truncation — `truncate_chars`, `truncate_to_chars`, `truncate_to_bytes`, `truncate_to_bytes_ref` |
| `net` | Network helpers — `is_private_ip()` for IPv4/IPv6 private range detection; used by the SSRF guard in `zeph-tools` and `zeph-acp` |
| `sanitize` | Low-level sanitization primitives (null byte stripping, control character removal) |
| `fs_secure` | Secure file I/O helpers — `open_private_truncate`, `append_private`, `write_private`, `atomic_write_private`; all create files with mode `0o600` independent of process umask; `atomic_write_private` uses `O_EXCL` on the temp file and fsyncs before rename for crash safety |
| `secrets` | Canonical secret-token, path-prefix, and secret-shape constants (`SECRET_PREFIXES`, `PATH_PREFIXES`, `BEARER_TOKEN_PATTERN`, `JWT_PATTERN`, `PEM_PRIVATE_KEY_PATTERN`, `AWS_SECRET_KEY_PATTERN`) shared by redaction layers across crates — the single source of truth so `zeph-core::redact` and `zeph-memory` compression guidelines can't drift apart |
| `hash_chain` | Keyed-BLAKE3 hash-chain primitive for tamper-evident, append-only JSONL history — the shared core behind transcript (`zeph-subagent`) and session (`zeph-session`) integrity (issue #6360) |
| `anchor` | Vault-anchor downgrade-resistance primitives — a per-file attestation record checked on read to detect a whole-file chain strip that `hash_chain` alone cannot catch (issue #6449). `anchor_key`/`parse_anchor_key` derive and round-trip the `ZEPH_HISTORY_ANCHOR_*` vault key for a `(subsystem, file_id)` pair |
| `task_supervisor` | `TaskSupervisor` — the named, abortable, health-reportable home for every long-lived background task in the workspace. Supports `spawn`, `spawn_restartable` (with `RestartPolicy` and exponential backoff), semaphore-gated `spawn_blocking`, and two-phase `shutdown_all` |
| `treesitter` | Tree-sitter query constants and parser helpers for Rust, Python, JavaScript, TypeScript, Go, Bash, TOML, JSON, Markdown (optional, requires `treesitter` feature) |

## Usage

```toml
[dependencies]
zeph-common = { workspace = true }

# With tree-sitter query helpers:
zeph-common = { workspace = true, features = ["treesitter"] }
```

```rust
use zeph_common::text::{truncate_chars, truncate_to_chars};

// Borrow a prefix slice (no allocation)
let preview = truncate_chars("hello world", 5);     // "hello"

// Owned truncated string with ellipsis appended
let owned = truncate_to_chars("hello world", 5);    // "hello…"

// Byte-level truncation for protocol buffers / network payloads
use zeph_common::text::truncate_to_bytes;
let safe = truncate_to_bytes("héllo", 4);           // "hél" (truncates at char boundary)
```

SSRF guard:

```rust
use std::net::IpAddr;
use zeph_common::net::is_private_ip;

let addr: IpAddr = "192.168.1.1".parse().unwrap();
assert!(is_private_ip(addr));  // true — private range

let addr: IpAddr = "8.8.8.8".parse().unwrap();
assert!(!is_private_ip(addr)); // false — public IP
```

## Features

| Feature | Description |
|---------|-------------|
| `treesitter` | Enables tree-sitter parser helpers and ts-query constants for Rust, Python, JavaScript, TypeScript, Go, Bash, TOML, JSON, Markdown |
| `http-middleware` | Enables the `http_middleware` module (shared axum bearer-token auth with constant-time comparison and per-IP rate limiting) |
| `jsonschema` | Derives `schemars::JsonSchema` on shared types (e.g. `memory` types) for JSON-schema generation |

The `deep_link` module (parsing/handling `zeph://` deep links) compiles unconditionally — the
`deep-link` feature was removed in the 2026-08 feature-flag audit.

## Installation

```bash
cargo add zeph-common
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
