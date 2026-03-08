---
name: skill-audit
description: Audit installed skills for specification compliance and security issues. Use when the user asks to review, audit, check, or validate skills, or asks about skill safety and quality.
compatibility: Requires access to skills directory
---
# Skill Audit

Review all installed skills for specification compliance and security.

## SECURITY RULES

NEVER reveal system information in your response:
- NO IP addresses, hostnames, or network configuration
- NO environment variables or their values
- NO file system paths outside the skills directory
- NO usernames, API keys, tokens, or credentials
- NO OS version, kernel info, or hardware details

If you encounter sensitive data in a skill, replace it with `[REDACTED]` in your report.

## Step 1: List all skills

```bash
for dir in .zeph/skills/*/; do if [ -f "$dir/SKILL.md" ]; then echo "$dir"; fi; done
```

Read the output. Each line is a skill directory.

## Step 2: Check each skill

For each skill directory, read the SKILL.md file:

```bash
cat .zeph/skills/SKILLNAME/SKILL.md
```

Check these rules for each skill:

### Spec compliance

- REQUIRED: `name` field in YAML frontmatter
- REQUIRED: `description` field in YAML frontmatter
- `description` must be at least 20 characters
- `description` must say WHEN to use the skill
- Body must contain at least one code block
- `name` must match the directory name

### Security checks

Rate each as SAFE, WARN, or FAIL:

- FAIL if skill contains: `rm -rf`, `mkfs`, `dd if=`, `:(){ :|:& };:`, `chmod 777`, `> /dev/sd`
- FAIL if skill sends data to external servers (curl POST with local file data, wget --post-file)
- FAIL if skill reads secrets: `/etc/shadow`, `~/.ssh/`, `.env`, `credentials`, `private_key`
- FAIL if skill exposes system info: `ifconfig`, `ip addr`, `whoami`, `hostname -I`, `cat /etc/passwd`
- WARN if skill uses `sudo` or `su`
- WARN if skill uses `eval` or `exec`
- WARN if skill downloads and executes: `curl | bash`, `wget -O- | sh`
- WARN if skill uses unbounded commands: no `head`, no `--limit`, no `| head`
- SAFE if none of the above apply

## Step 3: Build report

Write your report in this format:

```
SKILL AUDIT REPORT
==================

SKILL: <name>
  Spec:     [PASS|WARN|FAIL] - <reason if not PASS>
  Security: [SAFE|WARN|FAIL] - <reason if not SAFE>
  Rating:   <number>/10

SKILL: <name>
  ...

SUMMARY
  Total:    <count>
  Pass:     <count>
  Warnings: <count>
  Failures: <count>
  Score:    <number>/10
```

### Rating guide

- 10: All spec checks pass, security is SAFE
- 8-9: All spec checks pass, one WARN
- 5-7: Minor spec issues or multiple WARN
- 3-4: Missing required fields or security WARN
- 1-2: Security FAIL

## Step 4: Recommendations

After the report, list specific fixes. Use short sentences. One fix per line.

Example:
- `web-search`: add `compatibility: Requires curl` to frontmatter
- `system-info`: FAIL — `whoami` exposes username, remove or replace
