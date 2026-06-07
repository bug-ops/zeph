---
aliases:
  - Deep Link NFR
tags:
  - nfr
  - deep-link
  - quality
created: 2026-06-07
status: approved
spec_id: "066"
standard: "ISO/IEC 25010:2011"
---

# NFR: zeph:// Deep Link Scheme

Standard: ISO/IEC 25010:2011 (product quality characteristics).

## NFR-1 — Performance Efficiency

**NFR-1.1 — URI parse latency.**
`parse_deep_link` must complete in < 1 ms on any valid or invalid URI of up to 4 KiB.
Measurement: `criterion` benchmark, warm binary, no I/O.

**NFR-1.2 — End-to-end launch latency.**
From OS handler invocation (`zeph url-open <uri>`) to first agent prompt display: < 3 seconds
on a warm Rust binary with a local config file, measured on Linux x86_64 and Windows x86_64.
This includes: URI parse, cwd validation (canonicalize), config load, agent bootstrap.

**NFR-1.3 — Registration latency.**
`zeph url-scheme register` must complete in < 2 seconds on Linux (xdg-mime + desktop-database)
and < 500 ms on Windows (registry write). macOS dispatch is a no-op in v1.

## NFR-2 — Reliability

**NFR-2.1 — No crash on malformed input.**
`parse_deep_link` must never panic on any arbitrary byte sequence passed as the URI argument.
Coverage: `proptest` or `quickcheck` fuzz over arbitrary strings.

**NFR-2.2 — Registration idempotency.**
Running `zeph url-scheme register` N times produces the same system state as running it once.
No duplicate desktop entries, no duplicate registry keys.

**NFR-2.3 — Unregister completeness.**
After `zeph url-scheme unregister`, no artefacts written by `register` remain on disk or in
the registry. `status` must report "not registered".

## NFR-3 — Security

**NFR-3.1 — Denylist bypass prevention.**
`parse_deep_link` + cwd validation must reject all paths resolving to denylist roots, including
variants using: symlinks, `..` traversal, percent-encoding (`%2F`), URL-encoded dots, and
case variations on case-insensitive filesystems.
Coverage: unit tests covering each bypass technique.

**NFR-3.2 — No privilege escalation.**
`zeph url-scheme register` must not call `sudo`, `pkexec`, or any Windows UAC elevation API.
Coverage: code review + grep for elevation primitives.

**NFR-3.3 — Prompt trust isolation.**
`prompt` injected via deep link must arrive at the agent loop with `trust_level = Untrusted`.
Coverage: unit test asserting `TrustLevel::Untrusted` on the queued message.

**NFR-3.4 — No auto-execution.**
A URI with `?auto=true` or `?-y=1` must not bypass the confirmation step or set any
auto-approve flag. Coverage: unit test.

## NFR-4 — Maintainability

**NFR-4.1 — Parser isolation.**
`parse_deep_link` must be a pure, sync function with zero async dependencies, zero `zeph-*`
peer crate imports (only std + existing `percent-encoding`-family crate already in the
workspace), and zero `unwrap`/`expect` in production paths.

**NFR-4.2 — Platform code isolation.**
Platform-specific registration code (Linux, Windows, macOS stubs) must be isolated behind
`#[cfg(target_os = "...")]` blocks within a single module. No platform-specific code must
leak into the shared URI parsing or bootstrap mapping logic.

**NFR-4.3 — Test coverage.**
All security-critical validation paths (FR-6 cwd order, FR-7 prompt trust, FR-10 unknown
host) must have unit tests with ≥ 90% branch coverage measured by `cargo llvm-cov`.

## NFR-5 — Portability

**NFR-5.1 — Build portability.**
The `deep-link` feature must compile without errors or warnings on Linux x86_64, Windows
x86_64, and macOS aarch64 (Apple Silicon). Windows and macOS compilation may use
`#[cfg(target_os)]` stubs for registration, but the URI parsing and CLI dispatch code is
platform-agnostic.

**NFR-5.2 — Runtime graceful degradation on unsupported distros.**
On Linux distros where `xdg-mime` or `update-desktop-database` is absent, `register` must
not exit non-zero. It must print actionable manual instructions.

## NFR-6 — Usability

**NFR-6.1 — Error message quality.**
All error messages from `parse_deep_link` and the registration commands must:
- Name the specific problem (not "invalid URI").
- Suggest a corrective action.
- Never expose internal stack traces to the user.

**NFR-6.2 — Registration discoverability.**
`zeph --help` and `zeph --init` must mention the `url-scheme` subcommand group.

## NFR-7 — Compatibility

**NFR-7.1 — Backward compatibility.**
Users who do not install the `deep-link` feature (default build without `desktop` bundle)
must see no behaviour change. The `url-open` and `url-scheme` subcommands must not appear
in their `--help` output.

**NFR-7.2 — Config forward compatibility.**
The `[deep_link]` config section uses `#[serde(default)]` throughout. A config file without
the section must load without error.

## NFR-8 — Safety

**NFR-8.1 — No infinite loop.**
`zeph url-open` must detect if it is already in a url-open dispatch context and hard-exit
with a clear error message. Detection mechanism: check for a specific env var
`ZEPH_URL_OPEN_DEPTH` set to `1` by the dispatcher; if already `1`, exit 1 with message
"deep-link dispatch loop detected". (See INV-LOOP in spec.md for details.)

**NFR-8.2 — No silent data loss.**
If prompt confirmation is declined by the user or discarded due to no TTY, a WARN log entry
must be written. The session must still start (blank session), not abort silently.
