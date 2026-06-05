---
aliases:
  - Ephemeral Plugins and Provider Overrides NFR
  - Parity NFR 3918
tags:
  - sdd
  - nfr
  - parity
  - plugins
  - provider-persistence
created: 2026-05-29
status: approved
related:
  - "[[specs/065-ephemeral-plugins-provider-overrides/brd]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/srs]]"
  - "[[specs/065-ephemeral-plugins-provider-overrides/spec]]"
---

# NFR: Ephemeral Plugin Loading and Provider Override Persistence (GitHub #3918)

ISO/IEC 25010:2011 quality model.

---

## Performance Efficiency

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-PE-01 | Ephemeral plugin download + extraction must not block the agent event loop | Runs in `tokio::task::spawn_blocking` or equivalent off-thread |
| NFR-PE-02 | Provider overrides restore adds no observable latency to session startup | < 1 ms (single SQLite row read) |
| NFR-PE-03 | Overrides blob size is capped to prevent unbounded deserialization time | Hard cap: 1 KB; validated before `serde_json::from_str` |

---

## Reliability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-RE-01 | Ephemeral plugin TempDir is cleaned up even on panic unwind | Guaranteed by `TempDir` Drop impl (RAII) |
| NFR-RE-02 | A corrupted or oversized overrides blob must not prevent session startup | Discard blob + log warning; proceed without overrides |
| NFR-RE-03 | If `--plugin-url` download fails (network error, bad digest), the session starts without the plugin and reports the error; agent does not crash | Verified by unit test with a mock that returns network error |

---

## Security

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-SE-01 | `--plugin-url` accepts only HTTPS scheme | `validate_url_scheme` must return `InsecureUrl` for `http://` or any non-HTTPS scheme |
| NFR-SE-02 | Path traversal in ephemeral plugin archives is blocked | Reuse existing extraction guard in `add_remote` |
| NFR-SE-03 | `scan_skill_entries` failures are blocking for ephemeral plugins | `strict_scan = true` causes load abort, not advisory warning |
| NFR-SE-04 | Ephemeral plugins never write to permanent plugin store | `add_remote_ephemeral` uses a `TempDir`, not `plugins_dir` |
| NFR-SE-05 | Overrides blob rejects unknown fields | `#[serde(deny_unknown_fields)]` on `ProviderOverrides` struct |

---

## Maintainability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-MA-01 | `add_remote_ephemeral` shares core logic with `add_remote` — no duplication | Extract shared download/verify/extract into a helper; both variants call it |
| NFR-MA-02 | `ProviderOverrides` is a concrete typed struct, not `HashMap<String, Value>` | Enables compile-time completeness checks and `deny_unknown_fields` |
| NFR-MA-03 | All public APIs introduced by this spec have doc comments with `# Examples` | Pre-merge doc check: `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc -p zeph-plugins -p zeph-core` |

---

## Portability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-PO-01 | Ephemeral plugin extraction uses `std::env::temp_dir()` or `tempfile::TempDir` | Works on Linux, macOS, Windows |

---

## Usability

| ID | Requirement | Target |
|----|-------------|--------|
| NFR-US-01 | Error message for `--plugin-url http://...` clearly states HTTPS is required | Contains the word "HTTPS" and the rejected URL |
| NFR-US-02 | Error message for scan failure names the offending SKILL.md entry | Contains the skill name and pattern that triggered blocking |
| NFR-US-03 | `plugin list` output shows `[ephemeral]` tag for session-loaded plugins | Verified by snapshot test |
