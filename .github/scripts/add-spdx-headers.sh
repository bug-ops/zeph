#!/usr/bin/env bash
# Add SPDX license headers to all project .rs files that don't already have them.
# Usage: .github/scripts/add-spdx-headers.sh [--check]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HEADER_LINE1="// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>"
HEADER_LINE2="// SPDX-License-Identifier: MIT OR Apache-2.0"

CHECK_ONLY=false
if [[ "${1:-}" == "--check" ]]; then
    CHECK_ONLY=true
fi

SEARCH_DIRS=("$REPO_ROOT/src" "$REPO_ROOT/crates")
for d in "${SEARCH_DIRS[@]}"; do
    if [[ ! -d "$d" ]]; then
        echo "error: source directory not found: $d" >&2
        exit 2
    fi
done

missing=()

while IFS= read -r -d '' file; do
    if ! head -1 "$file" | grep -q "SPDX-License-Identifier\|SPDX-FileCopyrightText"; then
        missing+=("$file")
    fi
done < <(find "${SEARCH_DIRS[@]}" -name '*.rs' -print0)

if [[ ${#missing[@]} -eq 0 ]]; then
    echo "All .rs files already have SPDX headers."
    exit 0
fi

if $CHECK_ONLY; then
    echo "Files missing SPDX headers (${#missing[@]}):"
    for f in "${missing[@]}"; do
        echo "  ${f#"$REPO_ROOT/"}"
    done
    exit 1
fi

for f in "${missing[@]}"; do
    tmp=$(mktemp)
    printf '%s\n%s\n\n' "$HEADER_LINE1" "$HEADER_LINE2" > "$tmp"
    cat "$f" >> "$tmp"
    mv "$tmp" "$f"
done

echo "Added SPDX headers to ${#missing[@]} files."
