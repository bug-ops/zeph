---
aliases:
  - Deep Link BRD
tags:
  - brd
  - deep-link
  - ux
created: 2026-06-07
status: approved
spec_id: "066"
---

# BRD: zeph:// Deep Link Scheme

## 1. Business Problem

Zeph users cannot launch a contextualised session from outside the terminal — from a browser
link, an IDE action, a script, or an OS shortcut. Competitor Goose v1.35.0 already supports
`goose://new-session` with cwd and prompt seeding. The absence of an equivalent scheme reduces
Zeph's integration surface and makes workflow automation harder for power users.

## 2. Stakeholders

| Role | Concern |
|---|---|
| End user (developer) | One-click session launch from browser, docs, IDE |
| Power user / script author | Programmatic deep-link generation for workflow automation |
| Security-conscious operator | Guarantee that untrusted URI parameters cannot drive the agent without confirmation |
| Package maintainer | OS-level scheme registration must not require root/admin |

## 3. Business Requirements

**BR-1 — One-action launch.** A user must be able to open a ready-to-use Zeph session by
clicking or activating a `zeph://new-session` URI from any registered OS source (browser, file
manager, script).

**BR-2 — Context seeding.** The URI must carry optional context: working directory, initial
prompt, config profile, and model selection, so the launched session starts with the correct
project context without manual setup.

**BR-3 — Opt-in registration.** URI scheme registration must be explicit and reversible — the
user runs `zeph url-scheme register` once; the scheme is not registered automatically at install
or startup.

**BR-4 — Security without friction.** Deep-link parameters are untrusted. The product must
prevent a malicious link from silently driving the agent, while keeping the confirmation step
skippable for power users who opt out.

**BR-5 — Parity with Goose baseline.** v1 must match Goose's single-action `new-session`
baseline. Extended actions (`resume`, `run-skill`) are reserved for follow-up iterations.

## 4. Constraints

- No admin/root/sudo access required for registration or dispatch.
- macOS .app wrapper auto-generation is out of scope for v1 (registered as follow-up issue).
- ACP HTTP attach is out of scope for v1 (bearer token discovery not feasible for a cold
  sibling process — registered as follow-up issue).
- Registration must be idempotent and reversible.
- Must not change any existing CLI behaviour for users who do not opt in.

## 5. Success Criteria

| Criterion | Measurement |
|---|---|
| A `zeph://new-session` click opens a session in < 3 s on a warmed binary | Manual test on Linux + Windows |
| Registration succeeds without root on Linux (systemd distro) and Windows | Manual + CI test |
| An unrecognised URI host produces a clear, non-crashing error message | Unit test |
| A URI carrying a `prompt` parameter shows a confirmation before the first turn (default config) | Manual test |
| `cwd` outside the allowlist / in the denylist is rejected with a clear error | Unit test |
| `zeph url-scheme unregister` removes the registration cleanly | Manual test |

## 6. Out of Scope (v1)

- `zeph://resume`, `zeph://run-skill`, `zeph://open`, `zeph://config`
- ACP HTTP attach from a cold sibling process (requires token discovery)
- macOS .app wrapper auto-generation
- Single-instance focus / window raise when a running session exists
- Concurrent/duplicate `url-open` deduplication (spawn-only path creates independent sessions)
- NixOS / Flatpak / Snap / Alpine registration support (documented as manual steps)
