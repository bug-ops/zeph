# zeph-common Guide

Shared utility functions, security primitives (`Secret`, `VaultError`), and strongly-typed identifiers (`ToolName`, `SessionId`) used across the workspace live here.

- Start with crate-local checks: `cargo build -p zeph-common`, `cargo nextest run -p zeph-common`, `cargo clippy -p zeph-common --all-targets -- -D warnings`.
- This crate must have zero dependencies on other `zeph-*` crates to remain a safe shared foundation.
- Security primitives (`Secret<T>`) must never expose inner values through `Debug`, `Display`, or serialization — verify before adding new impls.
- If the public API changes, downstream crates will be affected workspace-wide; run `cargo build --workspace` after changes here.
- `zeph_common::secrets` is the canonical source for secret-prefix/path-prefix/Bearer-JWT redaction patterns shared by `zeph-core` and `zeph-memory` — do not hand-roll a duplicate list in a consuming crate, the two existing copies had already drifted apart before consolidation (#6091, #5917).
