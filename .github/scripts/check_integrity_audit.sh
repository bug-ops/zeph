#!/usr/bin/env bash
# Issue #6360 (transcript-integrity) CI completeness gate.
#
# Enumerates every source file that plausibly reads a trusted-history-replay surface
# (sub-agent transcript JSONL, session event-log JSONL, or durable journal) and treats its
# content as legitimate prior context. Every match must appear in .github/integrity_audited.txt
# with a verdict; an unaudited match fails CI, and a self-test (below) fails CI if the pattern
# stops matching a file it is required to keep matching (pattern rot).
#
# History: the originally proposed pattern (`read_all\(|ReplayEngine::replay|...`) was empirically
# broken — it missed crates/zeph-core/src/session_resume.rs (uses ReplayEngine::fold, not
# ::replay) and over-matched 40+ unrelated crates/zeph-memory semantic-store files (a different
# trust domain: memory retrieval, not conversation-history replay-trust) when run over the whole
# workspace. This version scopes the scan to only the crates where a trust-bearing reader could
# plausibly live, per the critic's rev3 audit (.local/handoff/2026-07-18T04-58-40-critic-rev3.md).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

ALLOWLIST=".github/integrity_audited.txt"
PATTERN='ReplayEngine::(replay|fold)|TranscriptReader::(load|load_strict)|SessionEventLog::(open|open_exclusive)|read_chunked\(|ReplayCursor|open_execution|read_execution_range'

# Scope: crates that could plausibly hold a trusted-history reader. Deliberately excludes
# crates/zeph-memory (semantic/episodic memory store — a different trust domain), zeph-bench,
# zeph-llm, zeph-tools, zeph-mcp, and other crates with no conversation-history replay surface.
SCAN_PATHS=(
  crates/zeph-core
  crates/zeph-session
  crates/zeph-subagent
  crates/zeph-agent-persistence
  crates/zeph-acp
  crates/zeph-orchestration
  crates/zeph-a2a
  crates/zeph-commands
  crates/zeph-durable
  src
)

# `mapfile`/`readarray` requires bash >= 4 (macOS ships bash 3.2 by default) — use a portable
# while-read loop instead so this script also runs on a contributor's local macOS shell, not
# just CI's Linux runners.
HITS=()
while IFS= read -r line; do
  [[ -n "$line" ]] && HITS+=("$line")
done < <(
  grep -rlE "$PATTERN" "${SCAN_PATHS[@]}" 2>/dev/null \
    | grep -v -E '/tests/|tests\.rs$|\.md$|/benches/' \
    | sort -u
)

if [[ ${#HITS[@]} -eq 0 ]]; then
  echo "FATAL: the integrity-audit scan found zero matches — the pattern is broken (pattern rot)." >&2
  exit 1
fi

# Extract just the path column (first whitespace-separated field) from non-comment,
# non-blank allowlist lines, so a `.` or other regex-special character in a path never needs
# escaping and can't produce a false match.
AUDITED_PATHS=()
while IFS= read -r line; do
  [[ -n "$line" ]] && AUDITED_PATHS+=("$line")
done < <(awk 'NF && $1 !~ /^#/ {print $1}' "$ALLOWLIST")

unaudited=()
for f in "${HITS[@]}"; do
  found=false
  for audited in "${AUDITED_PATHS[@]}"; do
    if [[ "$audited" == "$f" ]]; then
      found=true
      break
    fi
  done
  if [[ "$found" == false ]]; then
    unaudited+=("$f")
  fi
done

if [[ ${#unaudited[@]} -gt 0 ]]; then
  echo "FAIL: the following trusted-history readers are not in $ALLOWLIST:" >&2
  printf '  %s\n' "${unaudited[@]}" >&2
  echo "" >&2
  echo "Add each with a verdict (VERIFIED/DISPLAY_ONLY/CALLER_CONTEXT/FOLLOW_UP) — see the" >&2
  echo "allowlist file's header for definitions — before merging (issue #6360)." >&2
  exit 1
fi

# Self-test (S-new-3): assert the pattern still matches a curated set of files it MUST match —
# if any of these stop appearing in $HITS, the pattern has silently rotted (e.g. an API rename)
# and the gate is providing false assurance. This is what caught the originally proposed
# pattern's session_resume.rs miss during implementation.
REQUIRED_MATCHES=(
  "crates/zeph-subagent/src/transcript.rs"
  "crates/zeph-subagent/src/manager/collect.rs"
  "crates/zeph-session/src/log.rs"
  "crates/zeph-session/src/replay.rs"
  "crates/zeph-session/src/fork.rs"
  "crates/zeph-agent-persistence/src/hydrate.rs"
  "crates/zeph-durable/src/backend/local.rs"
  "crates/zeph-durable/src/replay.rs"
  "src/commands/sessions.rs"
  # crates/zeph-core/src/session_resume.rs is the specific file the original (broken) pattern
  # missed — it matches only via `ReplayEngine::fold` in a doc comment, not `::replay`. Keeping
  # it in this list is what actually catches a regression to that exact bug (narrowing
  # `ReplayEngine::(replay|fold)` back down to `replay` only) — tester-found: without this
  # entry the self-test "passed" even when reproducing the original miss.
  "crates/zeph-core/src/session_resume.rs"
)

missing=()
for required in "${REQUIRED_MATCHES[@]}"; do
  found=false
  for hit in "${HITS[@]}"; do
    if [[ "$hit" == "$required" ]]; then
      found=true
      break
    fi
  done
  if [[ "$found" == false ]]; then
    missing+=("$required")
  fi
done

if [[ ${#missing[@]} -gt 0 ]]; then
  echo "FAIL (self-test): the integrity-audit pattern no longer matches known readers:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo "" >&2
  echo "This means the grep PATTERN in this script has rotted (e.g. an API was renamed) and" >&2
  echo "is silently losing coverage — fix the pattern, do not just update this list." >&2
  exit 1
fi

echo "OK: ${#HITS[@]} trusted-history readers scanned, all present in $ALLOWLIST; self-test passed."
