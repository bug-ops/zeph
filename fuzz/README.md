# Zeph coverage-guided fuzzing

This directory is a detached [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) workspace.
It is **not** a member of the root Cargo workspace (`fuzz/Cargo.toml` declares its own empty
`[workspace]` table), so `cargo build` / `cargo test` / `cargo clippy --workspace` at the repo
root never touch it and never require a nightly toolchain.

Fuzzing here targets panics, logic bugs, and algorithmic-complexity denial-of-service in
parser-like components — **not** memory-safety undefined behavior. None of the fuzzed code
paths use `unsafe` (the workspace denies it via `unsafe_code = "deny"`), so there is no UB to
hunt; the value is in finding malformed inputs that panic, hang, or silently corrupt state.

## Prerequisites

cargo-fuzz requires the **nightly** toolchain for sanitizer/coverage instrumentation and
`#![no_main]` harnesses:

```bash
cargo install cargo-fuzz
rustup toolchain install nightly
```

## Targets

| Target | Fuzzes | Input |
|--------|--------|-------|
| `skill_frontmatter` | `zeph_skills::loader::load_skill_meta_from_str` — transitively exercises the private `split_frontmatter`/`parse_frontmatter` hand-rolled parsers | raw `&str` |
| `skill_extensions` | `zeph_skills::extensions::parse_extensions` — the `serde_norway` YAML sub-block deserializer | raw `&str` |
| `chunk_file` | `zeph_index::chunker::chunk_file` across all 9 `Lang` variants (tree-sitter parse + chunk boundary logic) | structured `Input { lang_selector: u8, source: String }` |
| `config_toml` | `toml::from_str::<zeph_config::Config>` — Zeph's config deserialization graph (defaults, validation), not the TOML tokenizer itself (already OSS-Fuzzed upstream) | raw `&str` |

## Running a target locally

```bash
cd fuzz
cargo +nightly fuzz run skill_frontmatter
```

Stop with Ctrl-C at any time; libFuzzer runs until interrupted or a crash is found. To bound a
run (as CI does), pass `-max_total_time=<seconds>`:

```bash
cargo +nightly fuzz run skill_frontmatter -- -max_total_time=300
```

## Reproducing a crash

A crashing input is written to `fuzz/artifacts/<target>/crash-<hash>`. Replay it directly:

```bash
cargo +nightly fuzz run skill_frontmatter fuzz/artifacts/skill_frontmatter/crash-<hash>
```

Minimize a crash to the smallest input that still triggers it:

```bash
cargo +nightly fuzz tmin skill_frontmatter fuzz/artifacts/skill_frontmatter/crash-<hash>
```

## Seed corpora

Each target has a **mandatory**, committed seed corpus under `fuzz/corpus/<target>/`. An empty
corpus is a defect for `skill_frontmatter`, `skill_extensions`, and `chunk_file` — without seeds,
libFuzzer's raw byte mutation almost never produces a well-formed input past the target's first
structural gate (see below), so the fuzzer would spend its entire CI time budget doing nothing
useful.

- **`skill_frontmatter`**: copied from the real `.zeph/skills/*/SKILL.md` files in this repo.
  `load_skill_meta_from_str` early-returns unless the input starts with `---` and has a closing
  `---` delimiter — seeds give the fuzzer a valid envelope to mutate from.
- **`skill_extensions`**: hand-authored YAML blocks, each containing an `extensions:` key with
  indented `ui:`/`keybindings:`/`monitors:` children. `parse_extensions` returns `None`
  immediately if the `extensions:` key is absent, so seeds without it never reach the
  `serde_norway` deserializer.
- **`chunk_file`**: generated from real source files in this repo — see byte layout below.
- **`config_toml`**: copied from `crates/zeph-config/config/default.toml` and
  `crates/zeph-config/tests/fixtures/acp_pr4_v0_19.toml`.

### `chunk_file` seed byte layout

The harness input is:

```rust
#[derive(Arbitrary, Debug)]
struct Input {
    lang_selector: u8,
    source: String,
}
```

`libfuzzer-sys`'s `fuzz_target!(|input: Input| ...)` decodes the raw bytes via
`Arbitrary::arbitrary_take_rest`. For a derived struct this calls plain `arbitrary()` on every
field except the last, and `arbitrary_take_rest()` on the last field:

- `lang_selector: u8` uses `u8::arbitrary`, which reads exactly **1 byte from the front** of the
  input via `fill_buffer`.
- `source: String`, being the **last** field, uses `String::arbitrary_take_rest`, which delegates
  to `<&str>::arbitrary_take_rest` — this takes **all remaining bytes** and keeps the longest
  valid UTF-8 prefix. There is **no length prefix** anywhere in this layout (unlike a `String`
  that appears as a non-last field, which would consume a length suffix from the *end* of the
  input via `arbitrary_len`).

So a valid seed is simply: **1 selector byte, followed by raw UTF-8 source bytes, verbatim.**
`lang_selector % 9` indexes into the `LANGS` array in `fuzz_targets/chunk_file.rs`, in this order:
`0=Rust 1=Python 2=JavaScript 3=TypeScript 4=Go 5=Bash 6=Toml 7=Json 8=Markdown`.

`fuzz/scripts/gen_chunk_seeds.sh` generates the committed seeds by prepending the selector byte
to a handful of real files already in this repo (Rust, Python, Bash, TOML, JSON, Markdown — Go
and TypeScript have no in-repo sample files, so those two langs rely on libFuzzer's own mutation
rather than a seed). Re-run it after adding new representative sample files:

```bash
./fuzz/scripts/gen_chunk_seeds.sh
```

## Adding a new target

1. Add a `[[bin]]` entry to `fuzz/Cargo.toml` with `test = false`, `doc = false`, `bench = false`.
2. Add `fuzz_targets/<name>.rs` with `#![no_main]` and a `fuzz_target!` macro invocation.
3. Add SPDX headers (`// SPDX-FileCopyrightText: ...` / `// SPDX-License-Identifier: ...`) to the
   new file — the repo-wide `./.github/scripts/add-spdx-headers.sh` script only scans `src/` and
   `crates/`, so add the header manually for files under `fuzz/`.
4. Seed `fuzz/corpus/<name>/` — check whether the target function has an early structural gate
   (a required prefix, a required key, a magic byte) that raw mutation is unlikely to satisfy; if
   so, seeding is mandatory, not optional.
5. Add the target to the matrix in `.github/workflows/fuzz.yml` with an appropriate
   `-max_total_time` budget.

## Follow-up: `plugin_manifest` target (not implemented here)

A strong candidate for a 5th target: `zeph_plugins::manifest::PluginManifest` (pub, `Deserialize`,
`crates/zeph-plugins/src/manifest.rs:37`), parsed via `toml::from_str::<PluginManifest>` at
`crates/zeph-plugins/src/manager/registry.rs:432,531,561` (also `PluginSource` at line 561),
`store.rs:49`, `install.rs:44`, and `security.rs:147`. Unlike `config_toml` (a trusted, user-owned
file), plugin manifests originate from **untrusted marketplace/registry sources** — the same
threat model as `skill_frontmatter` — making this a higher-value target than `config_toml`. Left
out of this PR to keep scope tight; tracked as a follow-up issue.

## Build-time note

The detached `fuzz/` workspace has its own `Cargo.lock` and pulls `zeph-index` (features =
`["sqlite"]`), which transitively compiles `sqlx`, `zeph-llm`, `zeph-memory`,
`zeph-db`, `zeph-tools`, and 5 tree-sitter grammars — all ASan-instrumented (cargo-fuzz's
default). A cold build of this graph is the dominant cost of a CI run, not the fuzzing itself;
`.github/workflows/fuzz.yml` uses `Swatinem/rust-cache` keyed on `fuzz/Cargo.lock` to amortize
this across scheduled runs. If build time becomes a recurring problem, a slimmer `zeph-index`
feature that excludes the DB/LLM stack is a possible future optimization (tracked as a
conditional follow-up issue, not implemented here).
