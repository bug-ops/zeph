---
aliases:
  - Deep Link Implementation Plan
tags:
  - plan
  - deep-link
created: 2026-06-07
status: approved
spec_id: "066"
---

# Implementation Plan: zeph:// Deep Link Scheme (066)

## Phases

### Phase 1 — Foundation (PR #1 of 3)

**Goal**: URI parsing, data types, CWD validation, feature flag, config section.
No CLI wiring, no OS registration, no bootstrap integration. Reviewable in isolation.

Deliverables:
- `zeph-common/src/deep_link.rs`: `parse_deep_link`, `DeepLink`, `NewSessionParams`, `DeepLinkError`
- `zeph-config/src/types/deep_link.rs`: `DeepLinkConfig`, `AcpPreference`
- `src/url_scheme/validate.rs`: `validate_deep_link_cwd` (INV-CWD)
- `Cargo.toml`: `deep-link` feature flag; `desktop` bundle includes it
- `--migrate-config` migration step for `[deep_link]` section
- Unit tests: parser (valid URIs, all param types, malformed, unknown host, deferred host),
  cwd validation (denylist, allowlist, symlink, percent-encoded, case variations),
  prompt length cap, trust level assertion

Acceptance: `cargo nextest run -p zeph-common -p zeph-config --features deep-link` passes.

### Phase 2 — CLI dispatch + bootstrap integration (PR #2 of 3)

**Goal**: `url-open` and `url-scheme` subcommands wired into the CLI and runner.
No OS registration yet (stubs only). Includes TUI status message and `--init` wizard step.

Deliverables:
- `src/cli.rs`: `UrlOpen` and `UrlScheme` variants in `Command` enum (feature-gated)
- `src/runner.rs`: `handle_url_open`, `handle_url_scheme` dispatch arms
- Loop prevention (INV-LOOP): `ZEPH_URL_OPEN_DEPTH` check
- Prompt confirmation gate (FR-5a steps 4–5, FR-12 no-TTY path)
- `profile` and `model` validation against config at runtime
- TUI status message (FR-14)
- `--init` wizard step offering `url-scheme register`
- `zeph url-scheme status` (reads artefact paths; does not write)

Acceptance: `cargo run --features deep-link -- url-open "zeph://new-session"` starts a blank
session. `cargo run --features deep-link -- url-open "zeph://new-session?prompt=hello"` shows
confirmation prompt.

### Phase 3 — OS Registration (PR #3 of 3)

**Goal**: Full `register` and `unregister` on Linux and Windows. macOS stub with instructions.

Deliverables:
- `src/url_scheme/register.rs`: platform-specific code behind `#[cfg(target_os)]`
- Linux: write `.desktop`, invoke `xdg-mime`, invoke `update-desktop-database` (graceful
  degradation if tools absent)
- Windows: `winreg`-based HKCU write/delete
- macOS: instructions printed, exit 0
- End-to-end manual test playbook (`.local/testing/playbooks/deep-link.md`)
- Coverage-status row added

Acceptance: `zeph url-scheme register` succeeds on Linux + Windows CI. Manual test: open
`zeph://new-session` from browser on registered system.

## Milestones

| Milestone | Content | Target |
|---|---|---|
| M1 | Phase 1 PR merged | Sprint N |
| M2 | Phase 2 PR merged | Sprint N+1 |
| M3 | Phase 3 PR merged + issue #4687 closed | Sprint N+1 |

## Dependencies

- `percent-encoding` crate (already in workspace via zeph-plugins; confirm dep chain)
- `winreg` crate (Windows target only; add to workspace deps)
- `zeph-acp/src/agent/mod.rs` denylist review for FR-6a parity (read-only reference)

## Risk Register

| Risk | Mitigation |
|---|---|
| Windows CI runner not available | Phase 3 Linux-only CI; Windows manual test documented |
| macOS .app wrapper requested before v2 | Scope is explicit in BRD §6; redirect to follow-up issue OQ-1 |
| `winreg` crate version conflict | Check workspace deps before adding |
| `update-desktop-database` absent in CI container | Test with graceful-degradation path (tool not found) |
