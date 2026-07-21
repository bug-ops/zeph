---
aliases:
  - Deep Link Scheme Spec
  - zeph:// URI Scheme
tags:
  - sdd
  - spec
  - ux
  - deep-link
created: 2026-06-07
status: approved
github_issue: 4687
related:
  - "[[013-acp/spec]]"
  - "[[043-zeph-common/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/spec]]"
---

# Spec 066: zeph:// Deep Link Scheme

Custom URI scheme allowing external callers (browser, OS launcher, scripts) to initiate a
fresh Zeph session with optional context parameters.

GitHub issue: #4687 — `feat(ux): deep link scheme for fresh session initiation (zeph://new-session)`

## Document Index

| Document | Purpose |
|---|---|
| `brd.md` | Business Requirements Document — business problem, personas, success criteria |
| `srs.md` | Software Requirements Specification (ISO/IEC/IEEE 29148) — functional requirements with EARS notation |
| `nfr.md` | Non-Functional Requirements (ISO/IEC 25010) — measurable quality targets |
| `spec.md` | Technical specification — design decisions, invariants, module breakdown |
| `plan.md` | Implementation plan — phases, milestones, deliverables |
| `tasks.md` | Task breakdown — implementable tasks for the developer |

## Scope Summary

**v1 ships:**
- URI scheme `zeph://new-session` with optional query params (`cwd`, `prompt`, `profile`, `model`)
- OS registration: Linux (xdg-mime / .desktop, systemd-distros only) + Windows (HKCU registry) — full
- macOS: dispatch only (`zeph url-open` works), no .app wrapper auto-generation
- CLI subcommands: `zeph url-open <uri>`, `zeph url-scheme {register,unregister,status}`
- Security model: untrusted-input, confirm-before-prompt, denylist/allowlist cwd
- Feature flag: `deep-link` (default off, included in `desktop` bundle)

**v1 explicitly excludes:**
- ACP HTTP attach path (token discovery not feasible for cold process — tracked as v2)
- macOS .app wrapper auto-generation (tracked as follow-up issue)
- URI hosts other than `new-session` (parser reserves forward-compat slots)

## Traceability Map

```
BR-1..BR-5 (BRD)
  └─ FR-1..FR-18 (SRS)
       ├─ NFR-1..NFR-8 (NFR)
       └─ INV-CWD, INV-LOOP, INV-TRUST, INV-NOAUTO, INV-SYNC, INV-NOTTY (spec.md)
            └─ TASK-1..TASK-18 (tasks.md)
```
