# Skill Trust Levels

Zeph assigns a trust level to every loaded skill, controlling which tools it can invoke. This prevents untrusted or tampered skills from executing dangerous operations like shell commands or file writes.

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

## Anomaly Detection

An `AnomalyDetector` tracks tool execution outcomes in a sliding window (default: 10 events). If the error/blocked ratio exceeds configurable thresholds, an anomaly is reported:

| Threshold | Default | Severity |
|-----------|---------|----------|
| Warning | 50% | Logged as warning |
| Critical | 80% | May trigger auto-block |

The detector requires at least 3 events before producing a result.

## Self-Learning Gate

Skills with trust level below `Verified` are excluded from self-learning improvement. This prevents the LLM from generating improved versions of untrusted skill content.

## CLI Commands

| Command | Description |
|---------|-------------|
| `/skill trust` | List all skills with their trust level, source, and hash |
| `/skill trust <name>` | Show trust details for a specific skill |
| `/skill trust <name> <level>` | Set trust level (`trusted`, `verified`, `quarantined`, `blocked`) |
| `/skill block <name>` | Block a skill (all tool access denied) |
| `/skill unblock <name>` | Unblock a skill (reverts to `quarantined`) |

## Configuration

```toml
[skills.trust]
# Trust level for newly discovered skills
default_level = "quarantined"
# Trust level for local (built-in) skills
local_level = "trusted"
# Trust level assigned after BLAKE3 hash mismatch on hot-reload
hash_mismatch_level = "quarantined"
```

Environment variable overrides:

```bash
export ZEPH_SKILLS_TRUST_DEFAULT_LEVEL=quarantined
export ZEPH_SKILLS_TRUST_LOCAL_LEVEL=trusted
export ZEPH_SKILLS_TRUST_HASH_MISMATCH_LEVEL=quarantined
```
