---
aliases:
  - Deep Link Technical Spec
tags:
  - spec
  - deep-link
  - technical
created: 2026-06-07
status: approved
spec_id: "066"
related:
  - "[[013-acp/spec]]"
  - "[[044-zeph-common/spec]]"
  - "[[001-system-invariants/spec]]"
  - "[[010-security/spec]]"
---

# Technical Spec: zeph:// Deep Link Scheme (066)

## 1. Module Placement

```
zeph-common
└── src/deep_link.rs          — URI parser + DeepLink struct (new module)
    └── parse_deep_link()     — pub fn, sync, pure, no zeph-* deps

src/cli.rs                    — UrlOpen + UrlScheme variants added to Command enum
src/runner.rs                 — handle_url_open() + handle_url_scheme() dispatch arms
src/url_scheme/
├── mod.rs                    — public re-exports
├── register.rs               — platform-specific registration logic
└── validate.rs               — cwd validation (calls parse_deep_link + fs ops)

zeph-config
└── src/types/deep_link.rs    — DeepLinkConfig struct with serde defaults
```

`parse_deep_link` lives in `zeph-common` (Layer-0a). This is consistent with `zeph-common`'s
existing posture: it already contains `net.rs`, `http_middleware.rs`, `sanitize.rs`,
`fs_secure.rs`, `spawner.rs`, and `task_supervisor.rs`. A pure-sync URI parser is a natural
fit. No INV-1 (Channel Contract) conflict — INV-1 is about `Box<dyn Channel>`, not layering.

## 2. Data Types

```rust
// zeph-common/src/deep_link.rs

/// Parsed representation of a zeph:// URI.
#[derive(Debug, Clone, PartialEq)]
pub enum DeepLink {
    NewSession(NewSessionParams),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewSessionParams {
    /// Absolute path, percent-decoded. Validated by caller via validate_deep_link_cwd().
    pub cwd: Option<PathBuf>,
    /// Percent-decoded prompt text, length-capped. Not yet sanitized.
    pub prompt: Option<String>,
    /// Named config profile alias. Validated against known profiles by caller.
    pub profile: Option<String>,
    /// Provider name from [[llm.providers]]. Validated by caller.
    pub model: Option<String>,
}

/// Parses a zeph:// URI string. Sync, panic-free, no I/O.
/// Returns Err for malformed URI; unknown hosts return DeepLinkError::UnknownHost.
pub fn parse_deep_link(uri: &str) -> Result<DeepLink, DeepLinkError> { ... }

#[derive(Debug, thiserror::Error)]
pub enum DeepLinkError {
    #[error("malformed URI: {0}")]
    Malformed(String),
    #[error("unknown scheme action '{0}'; try upgrading zeph")]
    UnknownHost(String),
    #[error("unsupported action '{0}' (reserved for a future version)")]
    DeferredHost(String),
    #[error("prompt too long: {0} bytes (limit: 8192)")]
    PromptTooLong(usize),
    #[error("cwd must be an absolute path")]
    CwdNotAbsolute,
}
```

## 3. CWD Validation Invariant (INV-CWD)

The following steps MUST be executed in exactly this order. Skipping or reordering steps
breaks the security model.

```
INV-CWD order:
  1. percent-decode raw param value
  2. assert path is absolute
  3. std::fs::canonicalize  (resolves symlinks, .., .)
  4. case-fold for comparison (lowercase on macOS/Windows; no-op on Linux)
  5. compare against hardcoded denylist (case-folded)
  6. if allowed_cwd_roots non-empty: assert starts_with at least one root
  7. assert metadata().is_dir()
```

Implementation location: `src/url_scheme/validate.rs`, `pub fn validate_deep_link_cwd`.

Hardcoded denylist roots (Unix):
- `/proc`, `/sys`, `/dev`
- `$HOME/.ssh`, `$HOME/.gnupg`, `$HOME/.aws`

Plus any roots present in `zeph-acp`'s `resolve_resource_link` denylist — check before
implementation and merge.

Accepted residual risk: TOCTOU window between step 3 and actual use. Documented with a
`// SAFETY: TOCTOU accepted` comment at the call site.

## 4. Self-Invocation Loop Prevention Invariant (INV-LOOP)

`zeph url-open` MUST set `ZEPH_URL_OPEN_DEPTH=1` in its own environment before launching
the child session (or before exec'ing into bootstrap). At startup of `url-open`, check:
if `ZEPH_URL_OPEN_DEPTH` is already set to `"1"`, print
`"deep-link dispatch loop detected; exiting"` and exit(1).

This env var check is the sole mechanism. No other loop-prevention is needed because
the registration templates are fixed strings (`url-open "%u"` / `"%1"`) and cannot
themselves emit `zeph://` URIs.

## 5. Bootstrap Reuse

`handle_url_open` in `runner.rs` MUST NOT implement a parallel agent runtime.
It maps `NewSessionParams` onto the existing CLI bootstrap:

```
NewSessionParams { cwd, prompt, profile, model }
    │
    ├── profile → equivalent of --config <profile_path> (look up in [profiles] map)
    ├── model   → set active_provider by name before agent start
    ├── cwd     → set process working directory (after validate_deep_link_cwd)
    └── prompt  → enqueue as first QueuedMessage with trust_level = Untrusted
                  (after confirmation gate)
```

The existing `runner::run` code path is entered after this mapping. The `url-open` arm is
a thin pre-processor, not a separate agent loop.

## 6. Prompt Trust Invariant (INV-TRUST)

ANY prompt injected via a deep link MUST carry `trust_level = Untrusted`.
It MUST NOT enter the message queue as a system or instruction message.
The `TrustLevel` enum in `zeph-common/src/trust_level.rs` is the authoritative reference.

## 7. Configuration Schema

```toml
# Canonical position in config.toml (after [tools], before [scheduler])
[deep_link]
# true = show prompt to user and require y/N before injecting (default: true)
confirm_before_prompt = true

# Restrict cwd to these root directories. Empty = any non-denylisted path is accepted.
allowed_cwd_roots = []

# ACP attach preference. v1 only supports "never"; "auto" is reserved for v2.
prefer_acp = "never"
```

`DeepLinkConfig` in `zeph-config/src/types/deep_link.rs`:
```rust
// Do NOT use #[derive(Default)] here — bool::default() is false, but confirm_before_prompt
// must default to true (secure default). Use an explicit impl instead.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeepLinkConfig {
    pub confirm_before_prompt: bool,
    pub allowed_cwd_roots: Vec<PathBuf>,
    pub prefer_acp: AcpPreference,
}

impl Default for DeepLinkConfig {
    fn default() -> Self {
        Self {
            confirm_before_prompt: true,  // secure default: always confirm
            allowed_cwd_roots: Vec::new(),
            prefer_acp: AcpPreference::Never,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcpPreference {
    #[default]
    Never,
    // Auto and Always are reserved for v2 — parsed but treated as Never
    Auto,
    Always,
}
```

`DeepLinkConfig` is added to the top-level `Config` struct with `#[serde(default)]`.

## 8. Feature Flag

Cargo feature: `deep-link`. Added in root `Cargo.toml` `[features]`.
Included in the `desktop` bundle.
NOT in `default` (per NFR-7.1 and Feature Flag Contract §9).

All `deep-link`-specific code in `src/` is gated with `#[cfg(feature = "deep-link")]`.
`zeph-common/src/deep_link.rs` is always compiled (no feature gate on the parser itself,
which has no runtime cost) — only the CLI subcommands and OS registration code are gated.

## 9. TUI Status Message

When TUI mode is active and the session was initiated via deep link, emit a system status
entry (not a spinner, it's a one-shot notification):
```
Opened via zeph://new-session
```
This satisfies INV-12 of system invariants (background operations must have visible status).

## 10. Platform-Specific Registration

### Linux

Registration artefacts:
- `~/.local/share/applications/zeph-url.desktop` (content in FR-Linux-1)
- Tool invocations: `xdg-mime default zeph-url.desktop x-scheme-handler/zeph`,
  `update-desktop-database ~/.local/share/applications`

Unregister: delete the `.desktop` file, rerun `update-desktop-database`.

Detection of missing tools: `which update-desktop-database` (or `Command::new` probe);
on failure, print instructions and return Ok.

### Windows

Registration artefacts: `HKCU\Software\Classes\zeph` subtree (see FR-Win-1).
Use `winreg` crate (already a workspace dep or add as `[target.'cfg(windows)'.dependencies]`).

Unregister: `RegDeleteTree` equivalent on `HKCU\Software\Classes\zeph`.

### macOS

v1: `register` prints instructions and exits 0. `unregister` is a no-op with a message.
`url-open` works when invoked directly from a correctly configured `.app` wrapper.
Full registration support tracked in follow-up issue (OQ-1).

## 11. Key Invariants Summary

| ID | Invariant | Where enforced |
|---|---|---|
| INV-CWD | cwd validation order: decode → absolute → canonicalize → case-fold → denylist → allowlist → is_dir | `validate.rs` |
| INV-LOOP | url-open checks ZEPH_URL_OPEN_DEPTH before dispatching; sets it before launching child | `runner.rs` |
| INV-TRUST | prompt from deep link is always Untrusted | `runner.rs` enqueue path |
| INV-NOAUTO | auto-escalation params (`auto`, `-y`) are silently dropped + WARN logged | `parse_deep_link` |
| INV-SYNC | `parse_deep_link` is sync, panic-free, no I/O, no zeph-* deps | `deep_link.rs` |
| INV-NOTTY | if no TTY + confirm_before_prompt=true, prompt discarded, blank session starts | `runner.rs` |

## 12. Rejected Alternatives

- **Sniff argv for zeph:// URI instead of dedicated subcommand** — rejected: ambiguous, clap
  parses URI as unknown flags.
- **env-var URI carrier** — rejected: OS handlers use argv; env adds uncontrolled injection
  surface.
- **Auto-register at startup** — rejected: surprising side effect, macOS requires .app bundle.
- **ACP attach in v1** — rejected (critic S2): bearer token not discoverable by cold sibling
  process. Documented as v2 follow-up.
- **`?profile=` accepting a raw filesystem path** — rejected: config injection vector.

## 13. ACP Attach Follow-up Scope (v2 design sketch)

For the v2 spec: ACP HTTP attach requires:
1. A discovery mechanism (local pidfile / Unix socket) advertising the running ACP server's
   HTTP port and a one-time bearer token.
2. `url-open` reads the pidfile, validates it points to a live process (kill -0), reads the
   one-time token, issues `POST /sessions` with Authorization header.
3. Reconcile `[deep_link] allowed_cwd_roots` with ACP `additional_directories`: for the
   attach path, the cwd must pass BOTH allowlists; for the spawn path, only the deep-link
   allowlist applies.
4. Token rotation: the one-time token must be regenerated after each attach to prevent replay.

This is a separate spec and a separate PR.
