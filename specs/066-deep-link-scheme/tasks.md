---
aliases:
  - Deep Link Tasks
tags:
  - tasks
  - deep-link
created: 2026-06-07
status: approved
spec_id: "066"
---

# Developer Tasks: zeph:// Deep Link Scheme (066)

Each task corresponds to one implementable unit of work. Tasks within the same phase are
ordered by dependency. A developer should be able to implement one task per PR review cycle.

Traceability: each task references the FR/NFR/INV it satisfies.

---

## Phase 1: Foundation

### TASK-1 — Add `deep-link` feature flag and `[deep_link]` config section

**Files**: `Cargo.toml` (root + `zeph-config/Cargo.toml`), `zeph-config/src/types/deep_link.rs`,
`zeph-config/src/types/mod.rs`, `crates/zeph-core/src/config/types/mod.rs`

**What**:
1. Add `deep-link` Cargo feature in root `Cargo.toml`; include it in the `desktop` bundle.
2. Create `DeepLinkConfig` struct (`confirm_before_prompt: bool = true`,
   `allowed_cwd_roots: Vec<PathBuf> = []`, `prefer_acp: AcpPreference = Never`).
3. Create `AcpPreference` enum (`Never` default; `Auto`/`Always` parsed but treated as `Never`).
4. Add `deep_link: DeepLinkConfig` field to the top-level `Config` struct with `#[serde(default)]`.
5. Add `--migrate-config` migration step: inject `[deep_link]` with defaults when absent.

**Acceptance** (FR-13, FR-13b, NFR-7.2):
- Config without `[deep_link]` loads without error.
- Config with `prefer_acp = "auto"` deserialises to `AcpPreference::Auto` (not an error).
- Migration step is idempotent.

---

### TASK-2 — Implement `parse_deep_link` in `zeph-common`

**Files**: `crates/zeph-common/src/deep_link.rs`, `crates/zeph-common/src/lib.rs`

**What**:
1. Add module `pub mod deep_link;` in `lib.rs`.
2. Implement `DeepLink`, `NewSessionParams`, `DeepLinkError` (exact signatures from spec §2).
3. Implement `parse_deep_link(uri: &str) -> Result<DeepLink, DeepLinkError>`:
   - Reject non-`zeph://` schemes.
   - Dispatch on host: `new-session` → parse query params; deferred hosts → `DeferredHost`;
     others → `UnknownHost`.
   - Percent-decode ALL query param values before validation.
   - `prompt`: measure byte length post-decode; return `PromptTooLong` if > 8192.
   - `cwd`: store as raw `PathBuf` (no I/O here — validation is in TASK-3).
   - `auto`/`-y` params: drop silently (do not error).
   - Unknown params: drop silently.
4. Add `proptest` / `quickcheck` fuzz test: arbitrary `&str` must never panic.

**Acceptance** (FR-1, FR-2, FR-3, FR-4, FR-7, FR-10, INV-SYNC, NFR-2.1, NFR-4.1):
- `parse_deep_link("zeph://new-session")` returns `Ok(DeepLink::NewSession(..))`.
- `parse_deep_link("zeph://resume")` returns `Err(DeepLinkError::DeferredHost("resume"))`.
- `parse_deep_link("zeph://foo")` returns `Err(DeepLinkError::UnknownHost("foo"))`.
- `parse_deep_link("zeph://new-session?prompt=" + "a".repeat(8193))` returns
  `Err(DeepLinkError::PromptTooLong(8193))`.
- `parse_deep_link("not-a-uri")` returns `Err(DeepLinkError::Malformed(..))`.
- Fuzz test: 100k random inputs, no panics.
- `cargo check -p zeph-common` with no `deep-link` feature: compiles (parser is always-on).

---

### TASK-3 — Implement `validate_deep_link_cwd`

**Files**: `src/url_scheme/validate.rs`, `src/url_scheme/mod.rs`

**What**:
Implement `pub fn validate_deep_link_cwd(raw: &Path, config: &DeepLinkConfig) -> Result<PathBuf, CwdError>`:
1. Assert path is absolute.
2. `std::fs::canonicalize` (I/O).
3. Case-fold for comparison (lowercase on macOS/Windows; no-op on Linux).
4. Compare against hardcoded denylist (resolve `$HOME` at call time, not at compile time).
5. If `allowed_cwd_roots` non-empty: assert `starts_with` at least one root (case-folded).
6. Assert `metadata().is_dir()`.
Add a `// SAFETY: TOCTOU accepted` comment at the canonicalize call.

Review `crates/zeph-acp/src/agent/mod.rs` `resolve_resource_link` for additional denylist
roots and merge them.

**Acceptance** (FR-6, FR-6a, FR-6b, INV-CWD, NFR-3.1):
- `/proc/self` → rejected (denylist).
- `$HOME/.ssh` (exact) → rejected.
- `$HOME/.SSH` → rejected (case-fold on macOS).
- `%2Fetc` decoded to `/etc` → accepted (not in denylist).
- Path resolving to `/proc/self` → rejected (denylist, same as `/proc/self` test above).
- Symlink pointing into `~/.ssh` → rejected after canonicalize (symlink escape via canonicalize).
- Non-existent path → rejected (canonicalize fails).
- Non-directory path → rejected (is_dir = false).
- `allowed_cwd_roots = ["/home/user/projects"]`, path `/home/user/other` → rejected.
- Unit tests for each case above.

---

## Phase 2: CLI Dispatch + Bootstrap Integration

### TASK-4 — Add `UrlOpen` and `UrlScheme` variants to `Command` enum

**Files**: `src/cli.rs`

**What**:
Add behind `#[cfg(feature = "deep-link")]`:
```rust
/// Handle a zeph:// URI dispatched by the OS scheme handler.
UrlOpen { uri: String },
/// Manage OS-level zeph:// scheme registration.
UrlScheme { #[command(subcommand)] command: UrlSchemeCommand },
```
Add `UrlSchemeCommand` enum: `Register`, `Unregister`, `Status`.

**Acceptance** (FR-5, NFR-7.1):
- `cargo run --features deep-link -- url-open --help` shows expected help.
- `cargo run -- url-open --help` (no feature): subcommand absent from help.

---

### TASK-5 — Implement `handle_url_open` in `runner.rs`

**Files**: `src/runner.rs`

**What**:
1. In the `Command::UrlOpen { uri }` arm:
   a. Check `ZEPH_URL_OPEN_DEPTH` env var; if `"1"`, print loop detection message and exit(1).
   b. Set `ZEPH_URL_OPEN_DEPTH=1` via `std::env::set_var` (before any child process launch).
   c. Call `parse_deep_link(&uri)`; on error, print friendly message and exit(1).
   d. If `cwd` present: call `validate_deep_link_cwd`; on error, print message and exit(1).
   e. If `profile` present: look up in `config.profiles` map; on not found, exit(1).
   f. If `model` present: look up in `config.llm.providers`; on not found, exit(1).
   g. If `prompt` present: apply confirmation gate (FR-5a steps 4–5, FR-12).
   h. Map parameters onto existing bootstrap (set cwd, override provider, enqueue prompt as
      `QueuedMessage` with `trust_level = Untrusted`).
   i. Continue into normal agent bootstrap path.

**Acceptance** (FR-5a, INV-LOOP, INV-TRUST, INV-NOTTY, NFR-8.1, NFR-3.3):
- Unit test: `ZEPH_URL_OPEN_DEPTH=1` → exits with "loop detected" message.
- Unit test: unknown model name → exits with error listing available models.
- Unit test: prompt queued with `TrustLevel::Untrusted`.
- Integration test (no network): `url-open "zeph://new-session"` starts blank session.

---

### TASK-6 — Implement `handle_url_scheme status`

**Files**: `src/url_scheme/register.rs`

**What**:
`Status` arm: detect whether artefacts written by `register` exist and match the current binary path.
- Linux: check `~/.local/share/applications/zeph-url.desktop` exists; parse `Exec=` line.
- Windows: read `HKCU\Software\Classes\zeph\shell\open\command`.
- macOS: print "macOS: manual registration only in v1".
Print status to stdout; exit 0.

**Acceptance** (FR-5d): `status` before `register` prints "not registered"; after `register`,
prints registered path.

---

### TASK-7 — Prompt confirmation gate and no-TTY handling

**Files**: `src/runner.rs` (or extracted to `src/url_scheme/prompt.rs`)

**What**:
Implement `confirm_prompt(prompt: &str, config: &DeepLinkConfig) -> ConfirmResult`:
- If `confirm_before_prompt = false`: return `Accepted`.
- If no TTY (`atty::is(atty::Stream::Stdin)` = false): log WARN, return `Discarded`.
- Otherwise: print the decoded prompt, ask `Accept? [y/N]`, read one line; `y` → Accepted,
  anything else → Declined.

`ConfirmResult::Discarded` and `ConfirmResult::Declined` both result in a blank session start
with a WARN log entry.

**Acceptance** (FR-5a, FR-12, NFR-8.2):
- No-TTY + `confirm_before_prompt=true` → WARN logged, blank session.
- `confirm_before_prompt=false` → prompt injected without interaction.

---

### TASK-8 — TUI status message for deep-link sessions

**Files**: `crates/zeph-tui/src/` (relevant status/notification module)

**What**:
When session was initiated via `UrlOpen`, emit a one-shot system notification in the TUI:
`Opened via zeph://new-session` (or truncated URI up to 60 chars).
Pass a `deep_link_uri: Option<String>` through the bootstrap context to the TUI init path.

**Acceptance** (FR-14): TUI session opened via deep link shows the notification in the status
area within 1 s of launch.

---

### TASK-9 — `--init` wizard step for scheme registration

**Files**: `src/init.rs` (or wherever the `--init` wizard lives)

**What**:
Add a step: "Would you like to register the `zeph://` URL scheme so you can open sessions
from your browser? [y/N]". If yes, call `handle_url_scheme_register()`.
Gate behind `#[cfg(feature = "deep-link")]`.

**Acceptance** (FR-13a): `zeph --init` presents the step; accepting calls `register`.

---

## Phase 3: OS Registration

### TASK-10 — Linux `.desktop` registration

**Files**: `src/url_scheme/register.rs` (Linux arm)

**What**:
1. Write `~/.local/share/applications/zeph-url.desktop` (content per FR-Linux-1).
   Use `std::env::current_exe()` for the `Exec` path.
2. Run `xdg-mime default zeph-url.desktop x-scheme-handler/zeph`.
3. Run `update-desktop-database ~/.local/share/applications`.
4. If either tool is not found: print manual instructions (copy the .desktop content + xdg-mime
   command), return Ok (do not exit non-zero).
5. Print success message with the registered path.

**Acceptance** (FR-Linux-1, FR-Linux-2, FR-Linux-3, NFR-5.2):
- On system with both tools: .desktop written, tools run, exit 0.
- On system without `update-desktop-database`: instructions printed, exit 0.
- Running register twice: idempotent (NFR-2.2).

---

### TASK-11 — Linux `.desktop` unregister

**Files**: `src/url_scheme/register.rs` (Linux arm)

**What**:
1. Delete `~/.local/share/applications/zeph-url.desktop` if it exists.
2. Run `update-desktop-database ~/.local/share/applications` (graceful on tool absent).
3. Print confirmation.

**Acceptance** (NFR-2.3): After unregister, `status` reports "not registered".

---

### TASK-12 — Windows HKCU registration

**Files**: `src/url_scheme/register.rs` (Windows arm), `Cargo.toml` (winreg dep)

**What**:
1. Add `winreg = "..."` to workspace deps under `[target.'cfg(windows)'.dependencies]`.
2. Implement `register_windows()`: write `HKCU\Software\Classes\zeph` subtree per FR-Win-1.
   Use `std::env::current_exe()` for the command path.
3. Implement `unregister_windows()`: delete the `zeph` key and all subkeys.

**Acceptance** (FR-Win-1, FR-Win-2, NFR-3.2):
- Registration writes exactly the expected keys; no HKLM writes.
- Unregister removes all keys.
- `status` reads and verifies the registered exe path.

---

### TASK-13 — macOS stub

**Files**: `src/url_scheme/register.rs` (macOS arm)

**What**:
`register_macos()`: print the current binary path and instructions for manually wrapping it
in a `.app` bundle. Exit 0.
`unregister_macos()`: print instructions for manual removal. Exit 0.

**Acceptance**: `zeph url-scheme register` on macOS exits 0 with instructions.

---

### TASK-14 — Testing playbook and coverage-status row

**Files**:
- `/Users/rabax/Dev/zeph/.local/testing/playbooks/deep-link.md` (main repo, not worktree)
- `/Users/rabax/Dev/zeph/.local/testing/coverage-status.md` (main repo)

**What**:
1. Write the deep-link playbook with concrete test scenarios:
   - Launch from browser (Linux + Windows).
   - `url-open` with each param combination.
   - CWD denylist rejection.
   - Prompt confirmation (accept / decline / no-TTY).
   - `register` / `unregister` / `status` on each platform.
   - Unknown host graceful error.
2. Add a row to `coverage-status.md` for the `deep-link` feature block with status `Untested`.

**Acceptance**: Playbook has at least 12 numbered scenarios with expected outcomes.

---

## Phase 1+2+3 Cross-Cutting

### TASK-15 — Doc comments on all public API items

**Files**: `zeph-common/src/deep_link.rs`, `zeph-config/src/types/deep_link.rs`,
`src/url_scheme/validate.rs`

**What**:
Every `pub` type, function, and enum variant must have a `///` doc comment explaining what
and why. `parse_deep_link` and `validate_deep_link_cwd` must include `# Examples` with
`no_run` doc-tests (they perform I/O or need a binary).

**Acceptance** (CLAUDE.md API docs rules):
`RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps -p zeph-common --features deep-link` passes with zero warnings.

---

### TASK-16 — CHANGELOG.md update

**Files**: `CHANGELOG.md`

**What**:
Add to the `[Unreleased]` section:
```
### Added
- `zeph://new-session` URI scheme for OS-level session initiation (#4687)
- `zeph url-open <uri>` CLI subcommand for deep-link dispatch
- `zeph url-scheme {register,unregister,status}` for scheme registration management
- `[deep_link]` config section (`confirm_before_prompt`, `allowed_cwd_roots`)
- `deep-link` Cargo feature (included in `desktop` bundle)
```

---

### TASK-17 — Register spec 066 in `specs/README.md`

**Files**: `specs/README.md`

**What**:
Add the spec-066 entry to the Feature Docs table:
```
| `066-deep-link-scheme/spec.md` | zeph:// URI scheme — `url-open`, `url-scheme {register,unregister,status}`, OS registration, security model | `zeph-common`, `zeph-config`, binary |
```

---

### TASK-18 — File follow-up issues

**What** (to be done by team-lead after TASK-1..17 are merged):
1. macOS .app wrapper auto-generation (OQ-1): file GitHub issue, reference spec §4.3.
2. ACP HTTP attach / token discovery (OQ-2): file GitHub issue, reference spec §13.
3. `zeph doctor` stale registration detection (OQ-4): file as P3 enhancement.

These issues must not block the v1 PR merge.
