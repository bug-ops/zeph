#!/usr/bin/env bash
# Issue #6515 CI guard: prevent AcpPermissionGate::new(_, None) in crates/zeph-acp/src.
#
# A `None` second argument makes the gate fall back to the developer's real
# `~/.config/zeph/acp-permissions.toml` (or platform equivalent) instead of an isolated
# per-test path, which silently pollutes that file across test runs (the #6512 flake,
# fixed for both terminal.rs and permission.rs by PR #6514). This script (#6515) is what
# keeps that fix from silently regressing. The sole production caller
# (crates/zeph-acp/src/agent/mod.rs) passes a variable, never a literal `None`, so this scan
# has zero legitimate matches on a clean tree.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

SCAN_DIR="crates/zeph-acp/src"
PATTERN='AcpPermissionGate::new\(.*,[[:space:]]*None[[:space:]]*\)'

# Known limitations (pragmatic line-based heuristic, same philosophy as
# check_integrity_audit.sh's documented limitations):
# 1. Multi-line calls (e.g. `new(\n    conn,\n    None,\n)`) are NOT detected — this is a
#    line-based scan. All current call sites are single-line and rustfmt keeps short calls
#    on one line; keep new AcpPermissionGate::new call sites single-line.
# 2. The greedy `.*` matches ANY `, None)` following `AcpPermissionGate::new(`, including a
#    literal `None` nested inside the first argument's own call, e.g.
#    `AcpPermissionGate::new(resolve(x, None), path)` would false-positive even though the
#    real second argument is `path`. Zero such sites exist today — this is a latent-only
#    false-positive class, not an active bug. No allowlist is added for this (YAGNI).
# 3. An intermediate variable (`let x = None; AcpPermissionGate::new(conn, x)`) is NOT
#    detected — this is a literal-token grep, not a data-flow analysis. Inherent to the
#    approach; not a realistic accidental-reintroduction vector (a developer copying an
#    existing test would copy the inline `None` form, not introduce an indirection).
scan() {
  local target="$1"
  # -H forces the filename prefix unconditionally: GNU grep (Linux CI) omits it on a
  # single-file scan (unlike BSD grep, which always includes it), which would otherwise make
  # the `:[0-9]+:` comment-filter anchor below fail to match on the self-test's one-line
  # fixture files below and silently disable the comment filter there.
  grep -rHnE "$PATTERN" "$target" 2>/dev/null | grep -v -E ':[0-9]+:[[:space:]]*//' || true
}

hits="$(scan "$SCAN_DIR")"

if [[ -n "$hits" ]]; then
  echo "FAIL: AcpPermissionGate::new(_, None) found in $SCAN_DIR." >&2
  echo "A None second argument falls back to the developer's real acp-permissions.toml and" >&2
  echo "can pollute it across test runs (issue #6515 / #6512). Use" >&2
  echo "Some(temp_perm_path()) with a per-test tempdir-backed path instead." >&2
  echo "" >&2
  echo "$hits" >&2
  exit 1
fi

# Method-existence sanity check: if AcpPermissionGate::new was renamed, the guard would
# silently stop flagging anything. Fail loudly instead of passing for the wrong reason.
if ! grep -rq 'AcpPermissionGate::new' "$SCAN_DIR" 2>/dev/null; then
  echo "FATAL: AcpPermissionGate::new not found in $SCAN_DIR — was it renamed or moved?" >&2
  echo "This guard is silently dead and must be updated." >&2
  exit 1
fi

# Anti-rot self-test: each fixture sample lives in its OWN isolated one-line temp file, so a
# regression in any single sample is actually caught rather than masked by another sample's
# match (a combined-file self-test could stay "non-empty" from the bad sample alone even if a
# good sample also started matching).
selftest_dir="$(mktemp -d)"
trap 'rm -rf "$selftest_dir"' EXIT

bad_file="$selftest_dir/bad.rs"
good_some_file="$selftest_dir/good_some.rs"
doc_comment_file="$selftest_dir/doc_comment.rs"
production_var_file="$selftest_dir/production_var.rs"

printf '    let (g, h) = AcpPermissionGate::new(conn, None);\n' >"$bad_file"
printf '    let (g, h) = AcpPermissionGate::new(conn, Some(path));\n' >"$good_some_file"
printf '    /// AcpPermissionGate::new(conn, None) falls back ...\n' >"$doc_comment_file"
printf '        AcpPermissionGate::new(Arc::clone(&conn), self.permission_file.clone());\n' >"$production_var_file"

if [[ -z "$(scan "$bad_file")" ]]; then
  echo "FAIL (self-test): the bad sample (literal None) was NOT flagged — regex rotted." >&2
  exit 1
fi

if [[ -n "$(scan "$good_some_file")" ]]; then
  echo "FAIL (self-test): the good sample (Some(path)) WAS flagged — regex rotted." >&2
  exit 1
fi

if [[ -n "$(scan "$doc_comment_file")" ]]; then
  echo "FAIL (self-test): the doc-comment sample WAS flagged — comment filter rotted." >&2
  exit 1
fi

if [[ -n "$(scan "$production_var_file")" ]]; then
  echo "FAIL (self-test): the production-variable sample WAS flagged — regex rotted." >&2
  exit 1
fi

echo "OK: no AcpPermissionGate::new(_, None) in zeph-acp; self-test passed."
