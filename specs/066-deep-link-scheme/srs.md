---
aliases:
  - Deep Link SRS
tags:
  - srs
  - deep-link
  - requirements
created: 2026-06-07
status: approved
spec_id: "066"
standard: "ISO/IEC/IEEE 29148:2018"
---

# SRS: zeph:// Deep Link Scheme

Standard: ISO/IEC/IEEE 29148:2018. EARS notation is used for functional requirements.

## 1. URI Scheme Definition

**FR-1** (→ BR-1, BR-2)
WHEN a `zeph://` URI is passed to `zeph url-open`, the system SHALL parse it according to the
grammar in §2 and reject malformed URIs with a structured error before any process launch.

**FR-2** (→ BR-1)
The system SHALL support exactly one URI host in v1: `new-session`. All other hosts SHALL be
handled per FR-10.

**FR-3** (→ BR-2)
WHEN the `new-session` host is parsed, the system SHALL accept the following optional query
parameters:

| Parameter | Type | Constraint |
|---|---|---|
| `cwd` | Absolute path (percent-encoded) | Validated per FR-6 |
| `prompt` | String (percent-encoded) | Length cap per FR-7 |
| `profile` | Named config profile alias | Validated per FR-8 |
| `model` | Provider name from `[[llm.providers]]` | Validated per FR-9 |

**FR-4** (→ BR-2)
The system SHALL NOT accept a `?auto=true` or any autonomy-escalation parameter. If such a
parameter appears, it SHALL be silently dropped (not treated as an error) and its presence SHALL
be logged at WARN level.

### 2. URI Grammar (ABNF)

```abnf
zeph-uri    = "zeph://" host [ "?" query ]
host        = "new-session"
query       = param *( "&" param )
param       = cwd-param / prompt-param / profile-param / model-param / ignored-param
cwd-param   = "cwd=" pct-encoded-path
prompt-param = "prompt=" pct-encoded-text
profile-param = "profile=" profile-name
model-param = "model=" provider-name
ignored-param = token "=" *pchar   ; unknown params: log WARN, ignore
pct-encoded-path = 1*( pchar / pct-encoded )
pct-encoded-text = *( pchar / pct-encoded )
profile-name = 1*( ALPHA / DIGIT / "-" / "_" )
provider-name = 1*( ALPHA / DIGIT / "-" / "_" )
```

Percent-decoding of ALL parameters MUST occur before any validation.

## 3. CLI Integration

**FR-5** (→ BR-1, BR-3)
The system SHALL add two top-level subcommand groups to the `Command` enum:
- `zeph url-open <uri>` — dispatch a `zeph://` URI (the OS handler invocation target)
- `zeph url-scheme {register | unregister | status}` — manage OS-level registration

**FR-5a** — `zeph url-open`
WHEN invoked, the command SHALL:
1. Parse and validate the URI (FR-1 through FR-9).
2. Map the parsed `DeepLink` struct onto the existing bootstrap path (no parallel runtime).
3. Apply the `cwd`, `profile`, and `model` parameters to the agent configuration.
4. If `confirm_before_prompt = true` (default) and a TTY is available, display the prompt to
   the user and require explicit confirmation before injecting it as the first turn.
5. If `confirm_before_prompt = true` and no TTY is available, reject the prompt parameter,
   log a WARN, and start a blank session (FR-12).
6. Launch the interactive surface: TUI if `desktop` feature is enabled and a terminal is
   detected, else CLI.

**FR-5b** — `zeph url-scheme register`
WHEN invoked, the command SHALL write platform-specific registration artefacts (per §4) using
only user-scoped locations (no sudo, no system directories). Registration SHALL be idempotent.
On failure to find a required tool (e.g., `update-desktop-database` absent), the command SHALL
print manual instructions and exit 0 (not hard-fail).

**FR-5c** — `zeph url-scheme unregister`
WHEN invoked, the command SHALL remove all artefacts written by `register`.

**FR-5d** — `zeph url-scheme status`
WHEN invoked, the command SHALL report: whether `zeph://` is currently registered for this
binary, the registered exe path, and whether it matches the running binary's path.

## 4. OS Registration

**FR-6-OS** (→ BR-3)
Registration SHALL write only to user-scoped locations. No elevation (sudo, UAC prompt, admin
rights) SHALL be required.

### 4.1 Linux (v1 — systemd distros)

**FR-Linux-1**: Write `~/.local/share/applications/zeph-url.desktop` with:
```ini
[Desktop Entry]
Type=Application
Name=Zeph URL Handler
Exec=zeph url-open %u
MimeType=x-scheme-handler/zeph;
NoDisplay=true
```

**FR-Linux-2**: Run `xdg-mime default zeph-url.desktop x-scheme-handler/zeph` and
`update-desktop-database ~/.local/share/applications`.

**FR-Linux-3**: If `update-desktop-database` is not found, print manual fallback instructions
and exit 0.

**Note**: NixOS, Flatpak, Snap, Alpine/busybox are out of scope for v1 automated registration.
Manual instructions SHALL be documented in the user guide.

### 4.2 Windows (v1)

**FR-Win-1**: Write to `HKCU\Software\Classes\zeph` with:
- Default value: `URL:Zeph Protocol`
- `URL Protocol` (empty string)
- `shell\open\command\(Default)` = `"<exe_path>" url-open "%1"`

Where `<exe_path>` is the absolute path from `std::env::current_exe()`.

**FR-Win-2**: `unregister` SHALL delete `HKCU\Software\Classes\zeph` and all subkeys.

### 4.3 macOS (v1 — dispatch only)

macOS v1 ships dispatch only. `zeph url-open` MUST work when invoked manually or from a
`.app` wrapper created by the user. Auto-generation of a `.app` wrapper is deferred to a
follow-up issue (see Open Questions §9, OQ-1).

`zeph url-scheme register` on macOS SHALL print instructions for manual registration and
the path to the running binary, then exit 0.

## 5. Security Requirements

### 5.1 CWD Validation (FR-6)

**FR-6** (→ BR-4)
WHEN a `cwd` parameter is present, the system SHALL validate it in the following order (all
steps are mandatory; failure at any step terminates validation with an error):

1. Percent-decode the raw parameter value.
2. Confirm the decoded path is absolute (starts with `/` on Unix, drive letter on Windows).
3. Call `std::fs::canonicalize` to resolve symlinks and `.`/`..` segments.
4. Case-fold the canonicalized path on case-insensitive filesystems (macOS, Windows: convert
   to lowercase for comparison only; do NOT alter the actual path used as cwd).
5. Compare the case-folded path against the denylist (FR-6a).
6. If `[deep_link] allowed_cwd_roots` is non-empty, confirm the case-folded path starts with
   at least one allowlisted root (FR-6b).
7. Confirm the path is an existing directory (`metadata().is_dir()`).

This order is an **invariant** (INV-CWD in spec.md). Reordering or skipping steps is forbidden.

TOCTOU note: canonicalize-then-use leaves a window. This is accepted residual risk for a
human-launched action and SHALL be noted in the implementation doc comment.

**FR-6a — Denylist (hardcoded):**
The following roots and their subdirectories are always rejected, case-insensitively:
- `/proc`, `/sys`, `/dev` (Linux only)
- `~/.ssh` (`$HOME/.ssh`)
- `~/.gnupg` (`$HOME/.gnupg`)
- `~/.aws` (`$HOME/.aws`)

`zeph-acp`'s `resolve_resource_link` denylist SHALL be reviewed for parity and any additional
sensitive roots it covers SHALL be merged into this list before implementation.

**FR-6b — Allowlist:**
When `[deep_link] allowed_cwd_roots` is set (non-empty list), the cwd MUST match at least one
entry. When empty (default), any path not in the denylist is allowed.

The relationship between `[deep_link] allowed_cwd_roots` and ACP `[acp] additional_directories`:
these are **independent allowlists** serving different purposes. ACP's allowlist controls what
the ACP server exposes; the deep-link allowlist controls what `url-open` may use as cwd.
Operators who run both MUST configure both if they want to restrict cwd via either path.
This independence is documented explicitly.

### 5.2 Prompt Validation (FR-7)

**FR-7** (→ BR-4)
WHEN a `prompt` parameter is present:
1. Percent-decode the raw parameter value.
2. Measure the byte length of the decoded string. Reject if > 8192 bytes (post-decode cap).
3. Inject the prompt as a normal user turn with `trust_level = Untrusted` (per `trust_level.rs`
   in `zeph-common`). It MUST NOT be injected as a system/instruction message.
4. The existing sanitizer/IPI pipeline applies as for any user turn.
5. If `confirm_before_prompt = true` (default), display the decoded prompt to the user and
   require explicit `y/N` confirmation before injecting. Default response on timeout or
   non-interactive input is `N` (reject).

### 5.3 Profile and Model Validation (FR-8, FR-9)

**FR-8** — `profile` parameter:
The value MUST be one of the named config profile aliases known at startup (e.g., entries in
`[profiles]` config section). Raw filesystem paths are rejected. Unknown profile names are
rejected with a clear error (not silently ignored).

**FR-9** — `model` parameter:
The value MUST match a provider `name` from `[[llm.providers]]`. Unknown names are rejected
with a clear error listing available provider names.

### 5.4 Unknown URI Hosts (FR-10)

**FR-10** (→ BR-5, critic M2)
WHEN the URI host is not `new-session`, the system SHALL:
- Emit a friendly, non-crashing error message naming the unrecognised action and suggesting
  a binary upgrade.
- For hosts in the reserved-but-deferred list (`resume`, `run-skill`, `open`, `config`), emit
  a distinct "not supported in this version" message.
- For entirely unknown hosts, emit "unrecognised scheme action".
- In both cases, exit with a non-zero exit code.

### 5.5 Self-invocation Loop Prevention (FR-11)

**FR-11** (→ critic M1)
`zeph url-open` MUST NOT itself emit or dispatch a `zeph://` URL. The OS registration
artefacts write a fixed `url-open "%u"` / `"%1"` template and nothing else. This is stated
as a code invariant (INV-LOOP in spec.md).

### 5.6 No-TTY Behavior (FR-12)

**FR-12** (→ critic M4)
WHEN `confirm_before_prompt = true` and the process has no controlling TTY:
- The `prompt` parameter SHALL be discarded.
- A WARN log entry SHALL be written: `deep-link: prompt discarded (no TTY, confirm required)`.
- A blank session SHALL be started with the remaining parameters (cwd, profile, model).

## 6. Configuration (FR-13)

**FR-13** (→ BR-3, BR-4)
A `[deep_link]` section SHALL be added to `zeph-config` with `#[serde(default)]`:

```toml
[deep_link]
# Require confirmation before injecting a prompt from a deep link.
# When no TTY is available, prompt is always discarded regardless of this setting.
confirm_before_prompt = true

# Restrict the cwd parameter to these root directories.
# Empty = any non-denylisted directory is accepted.
allowed_cwd_roots = []

# Prefer ACP HTTP attach over spawning a fresh process.
# Reserved for v2; always treated as "never" in v1.
prefer_acp = "never"
```

**FR-13a** — `--init` wizard:
WHEN `zeph --init` runs, it SHALL offer a step that invokes `zeph url-scheme register` on
behalf of the user after explaining what it does.

**FR-13b** — `--migrate-config`:
WHEN the `[deep_link]` section is absent from an existing config, `--migrate-config` SHALL
inject the section with default values.

## 7. TUI Integration (FR-14)

**FR-14** (→ CLAUDE.md TUI Rules §1: background operations must have visible status indicators)
WHEN a session is opened via a deep link in TUI mode, a status line SHALL be displayed:
`Opened via zeph://new-session` (or the full URI, truncated to 60 chars).

## 8. Feature Flag (FR-15)

**FR-15** (→ INV-9 Feature Flag Contract)
The `deep-link` capability SHALL be gated behind a Cargo feature flag `deep-link`.
Default: off. Included in the `desktop` feature bundle.
The flag gates: the `url-open` and `url-scheme` CLI subcommands, the `[deep_link]` config
section parsing, and all OS registration code.

## 9. Open Questions

| ID | Question | Owner | Decision needed by |
|---|---|---|---|
| OQ-1 | macOS .app wrapper auto-generation: implement in a follow-up issue? | team-lead | Before v2 scoping |
| OQ-2 | ACP HTTP attach token discovery: design in a follow-up issue? | architect | Before v2 scoping |
| OQ-3 | `deep-link` feature: include in `full` bundle? | team-lead | Before PR merge |
| OQ-4 | Binary-upgrade path durability: should `zeph doctor` detect stale registrations? | team-lead | Before v1 release |
| OQ-5 | Concurrent `url-open` invocations (user clicks link twice): acceptable to spawn two independent sessions in v1? | team-lead | Before v1 release |

**OQ-2 context (for the follow-up spec):**
ACP attach requires a `zeph url-open` sibling process to authenticate to `POST /sessions`
(protected by `BearerAuthLayer` when `auth_bearer_token` is configured). Token discovery options
for v2: (a) read the same vault key the ACP server uses, gated on loopback bind detection;
(b) a local pidfile advertising the HTTP bind and a one-time session token. Neither is trivial.
The v2 spec must also reconcile `[deep_link] allowed_cwd_roots` with ACP `additional_directories`
for the attach-with-cwd case.
