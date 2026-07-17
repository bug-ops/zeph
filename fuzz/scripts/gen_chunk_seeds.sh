#!/usr/bin/env bash
# Generate seed corpus files for the `chunk_file` fuzz target from real source files.
#
# The `chunk_file` harness takes a structured input:
#   #[derive(Arbitrary)] struct Input { lang_selector: u8, source: String }
# `fuzz_target!(|input: Input| ...)` decodes via `Arbitrary::arbitrary_take_rest`, which for a
# derived struct calls plain `arbitrary()` on every field except the last, and
# `arbitrary_take_rest()` on the last field. `u8::arbitrary` consumes exactly 1 byte from the
# front of the input; `String`, being the last field, uses `arbitrary_take_rest`, which takes
# ALL remaining bytes and keeps the longest valid UTF-8 prefix. So a valid seed is simply:
#   [1 lang-selector byte][raw UTF-8 source bytes] — no length prefix needed.
#
# lang_selector % 9 indexes into `LANGS` in fuzz_targets/chunk_file.rs, in this order:
#   0=Rust 1=Python 2=JavaScript 3=TypeScript 4=Go 5=Bash 6=Toml 7=Json 8=Markdown
#
# Usage: ./gen_chunk_seeds.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT_DIR="$REPO_ROOT/fuzz/corpus/chunk_file"
mkdir -p "$OUT_DIR"

# path:lang_selector pairs — one real source file per language available in this repo
# (Go/TypeScript have no in-repo samples, so those two langs rely on libFuzzer's own
# mutation to bootstrap coverage rather than a seed file).
declare -a SEEDS=(
    "crates/zeph-index/src/chunker.rs:0"
    "crates/zeph-skills/src/extensions.rs:0"
    "scripts/telegram-e2e/telegram_e2e.py:1"
    "scripts/install.sh:5"
    "crates/zeph-config/config/default.toml:6"
    ".github/renovate.json:7"
    "README.md:8"
)

for entry in "${SEEDS[@]}"; do
    src="${entry%%:*}"
    selector="${entry##*:}"
    src_path="$REPO_ROOT/$src"
    if [[ ! -f "$src_path" ]]; then
        continue
    fi
    name="$(basename "$src" | tr '.' '_')"
    out="$OUT_DIR/${name}_lang${selector}.bin"
    printf "$(printf '\\x%02x' "$selector")" > "$out"
    cat "$src_path" >> "$out"
done

echo "Generated $(ls "$OUT_DIR" | wc -l) chunk_file seed files in $OUT_DIR"
