# Skill Trust Levels

Zeph assigns a trust level to every loaded skill, controlling which tools it can invoke. This prevents untrusted or tampered skills from executing dangerous operations like shell commands or file writes.

> **Crate ownership:** `TrustLevel` is defined in `zeph-tools::trust_level` and re-exported by `zeph-skills` for convenience. `TrustGateExecutor`, which enforces the trust policy at execution time, also lives in `zeph-tools`. This keeps `zeph-tools` independent of `zeph-skills` while sharing the common type.

## Trust Tiers

| Level | Tool Access | Description |
|-------|-------------|-------------|
| **Trusted** | Full | Built-in or user-audited skills. No restrictions. |
| **Verified** | Full | Hash-verified skills. Default tool access applies. |
| **Quarantined** | Restricted | Newly imported or hash-mismatch skills. `bash`, `file_write`, and `web_scrape` are denied. |
| **Blocked** | None | Explicitly disabled. All tool calls are rejected. |

The default trust level for newly discovered skills is `quarantined`. Local (built-in) skills default to `trusted`.

## Integrity Verification

Each skill's `SKILL.md` content is hashed with BLAKE3 on load. The hash is stored in SQLite alongside the skill's trust level and source metadata. On hot-reload, the new hash is compared against the stored value. If a mismatch is detected, the skill is downgraded to the configured `hash_mismatch_level` (default: `quarantined`).

## Quarantine Enforcement

When a quarantined skill is active, `TrustGateExecutor` intercepts tool calls and blocks access to `bash`, `file_write`, and `web_scrape`. Other tools (e.g., `file_read`) remain subject to the normal permission policy.

Quarantined skill bodies are also wrapped with a structural prefix in the system prompt, making the LLM aware of the restriction:

```
[QUARANTINED SKILL: <name>] The following skill is quarantined.
It has restricted tool access (no bash, file_write, web_scrape).
```

## Body Sanitization

Skill bodies from non-`Trusted` sources are sanitized before prompt injection. XML-like structural tags (e.g., `</skill>`, `</system>`) are escaped to prevent prompt boundary confusion. This is applied automatically — no configuration required.

## Anomaly Detection

An `AnomalyDetector` tracks tool execution outcomes in a sliding window (default: 10 events). If the error/blocked ratio exceeds configurable thresholds, an anomaly is reported:

| Threshold | Default | Severity |
|-----------|---------|----------|
| Warning | 50% | Logged as warning |
| Critical | 80% | May trigger auto-block |

The detector requires at least 3 events before producing a result.

## Self-Learning Gate

Skills with trust level below `Verified` are excluded from self-learning improvement. This prevents the LLM from generating improved versions of untrusted skill content.

## Hash Verification on Trust Promotion

When promoting a skill's trust level via `zeph skill trust <name> trusted` or `zeph skill trust <name> verified`, the SkillManager recomputes the BLAKE3 hash of the current `SKILL.md` content and compares it against the stored hash. If the hashes diverge, the promotion is rejected and the skill remains at its current level. This prevents promoting a skill that has been modified since last verification.

Run `zeph skill verify <name>` to check integrity without changing trust level.

## Per-Invocation Integrity Re-Check

The promotion-time hash check above runs once, when you set a skill to `trusted`/`verified`. It
does not protect against `SKILL.md` being modified on disk *after* promotion — a tampered file
stays `trusted` until someone happens to run `zeph skill verify` again.

Setting the `requires_trust_check` flag on a skill closes that gap: with the flag armed, the
BLAKE3 hash is recomputed and compared against the stored hash on *every* dispatch (`load_skill`,
`invoke_skill`, and `zeph skill invoke`), not just at promotion time. A mismatch aborts the
invocation and demotes the skill to `quarantined` immediately, before its body is ever returned.

### Automatic activation on promotion (default)

Promoting a skill to `trusted` or `verified` — via `zeph skill trust <name> trusted|verified` or
`/skill trust <name> trusted|verified` — arms `requires_trust_check` **automatically** unless
disabled, per `[skills.trust] require_integrity_check_on_promote` (default `true`). This closes
the "operator forgot to pass `--require-check`" gap: Trusted/Verified is the trust tier whose body
is dispatched verbatim (no sanitization), so it is the choke point where a tampered `SKILL.md`
matters most.

Promoting to `quarantined` or `blocked` never touches `requires_trust_check` — a previously armed
flag survives a temporary demotion and re-promotion.

Override per command:

```bash
# CLI — force the check on even if the config default is false
zeph skill trust my-skill trusted --require-check

# CLI — skip arming even though the config default is true
zeph skill trust my-skill trusted --no-require-check
```

```text
# In-session — same two overrides
/skill trust my-skill trusted --require-check
/skill trust my-skill trusted --no-require-check
```

`--require-check` and `--no-require-check` are mutually exclusive and always win over the config
default. With neither flag, the command falls back to
`[skills.trust] require_integrity_check_on_promote`.

`/skill trust <name>` (no level argument) shows the flag's current state as
`requires_trust_check=true|false`. There is no command to clear it once armed — the only way is
to manually update the `requires_trust_check` column in the `skill_trust` SQLite table directly.
This is a known usability gap, not a security one (the flag only ever makes enforcement
stricter).

> **Note:** `zeph skill invoke <name>` shares its trust pipeline with `load_skill`/`invoke_skill`.
> A skill with no trust record at all resolves to `trusted` ("never classified", not "known
> untrusted") across all three, not `quarantined` — this differs from the "newly discovered skill"
> default in [Trust Tiers](#trust-tiers) above, which applies once a skill *has* been installed
> and given a trust row.

## Managed Skills Directory

External skills installed via `zeph skill install` are stored in `~/.config/zeph/skills/`. This directory is automatically appended to `skills.paths` at startup — no manual configuration required. Skills in this directory follow the same structure as local skills (`<name>/SKILL.md`).

## CLI Commands

| Command | Description |
|---------|-------------|
| `/skill trust` | List all skills with their trust level, source, and hash |
| `/skill trust <name>` | Show trust details for a specific skill |
| `/skill trust <name> <level>` | Set trust level (`trusted`, `verified`, `quarantined`, `blocked`). Promotion to `trusted`/`verified` arms the [per-invocation integrity re-check](#per-invocation-integrity-re-check) by default; append `--require-check`/`--no-require-check` to force it on/off |
| `/skill block <name>` | Block a skill (all tool access denied) |
| `/skill unblock <name>` | Unblock a skill (reverts to `quarantined`) |
| `/skill install <url\|path>` | Install an external skill (git URL or local path) with hot reload |
| `/skill remove <name>` | Remove an installed skill with hot reload |

## Skill Source Tracking

Every skill trust record stores a `source_kind` value that describes where the skill originated. This is used when determining default trust levels and in audit output.

| Value | Meaning |
|-------|---------|
| `local` | Skill shipped with the binary or found in a configured `skills.paths` directory |
| `hub` | Installed via `zeph skill install` from a remote URL (git or HTTP) |
| `file` | Imported directly from a local file path outside the managed skills directory |

Local skills default to the `local_level` trust tier. Hub and file-sourced skills default to the `default_level` tier (typically `quarantined`).

## Configuration

```toml
[skills.trust]
# Trust level for newly discovered skills
default_level = "quarantined"
# Trust level for local (built-in) skills
local_level = "trusted"
# Trust level assigned after BLAKE3 hash mismatch on hot-reload
hash_mismatch_level = "quarantined"
# Arm the per-invocation integrity re-check by default on promotion to trusted/verified
# (see "Automatic activation on promotion" above)
require_integrity_check_on_promote = true
```

Environment variable overrides:

```bash
export ZEPH_SKILLS_TRUST_DEFAULT_LEVEL=quarantined
export ZEPH_SKILLS_TRUST_LOCAL_LEVEL=trusted
export ZEPH_SKILLS_TRUST_HASH_MISMATCH_LEVEL=quarantined
export ZEPH_SKILLS_TRUST_REQUIRE_INTEGRITY_CHECK_ON_PROMOTE=true
```
