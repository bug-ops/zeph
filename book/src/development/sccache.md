# sccache

[sccache](https://github.com/mozilla/sccache) caches compiled artifacts across builds, significantly reducing incremental and clean build times.

## Installation

```bash
cargo install sccache
```

Or via Homebrew on macOS:

```bash
brew install sccache
```

## Configuration

The workspace ships `.cargo/config.toml` with sccache pre-configured:

```toml
[build]
build-dir = "{cargo-cache-home}/build/{workspace-path-hash}"
rustc-wrapper = "sccache"
incremental = false
```

`build-dir` separates intermediate compilation artifacts from the final `target-dir` layout
(Cargo's build-dir/target-dir split, stable since 1.91). Combined with `rustc-wrapper`, this
means parallel builds across multiple git worktrees of this repo (e.g. agent teams working in
separate worktrees) do not duplicate dependency compilation: each worktree keeps its own
lock-free build-dir keyed by `{workspace-path-hash}`, but identical dependency builds still hit
the same sccache object cache instead of being recompiled per worktree. `incremental = false`
disables incremental compilation locally to match the CI profile — incremental caches are dead
weight for one-shot builds and would otherwise grow unboundedly per worktree.

If sccache is not installed, Cargo prints a warning and falls back to direct `rustc` invocation. CI jobs that don't need compilation override the wrapper with `RUSTC_WRAPPER=""` (env var takes priority over config file).

`SCCACHE_CACHE_SIZE` and `SCCACHE_DIR` are machine-level settings, not committed to this
project's config — set them in your own `~/.cargo/config.toml` `[env]` table (as
`{ value = "...", force = true }` entries) or shell profile. Do not add a plain-string
`SCCACHE_CACHE_SIZE` to the project's `.cargo/config.toml`: Cargo merges `[env]` tables
key-by-key across config files, and a plain string here conflicts with a table-form entry
in a user's global config, breaking every `cargo` invocation with a config-merge error.

## Verify

After building the project, check cache statistics:

```bash
sccache --show-stats
```

## CI Usage

In GitHub Actions, add sccache before `cargo build`:

```yaml
- name: Install sccache
  uses: mozilla-actions/sccache-action@v0.0.9

- name: Build
  run: cargo build --workspace
  env:
    RUSTC_WRAPPER: sccache
    SCCACHE_GHA_ENABLED: "true"
```

## Storage Backends

By default sccache uses a local disk cache at `~/.cache/sccache`. For shared caches across CI runners, configure a remote backend:

| Backend | Env Variable | Example |
|---------|-------------|---------|
| S3 | `SCCACHE_BUCKET` | `my-sccache-bucket` |
| GCS | `SCCACHE_GCS_BUCKET` | `my-sccache-bucket` |
| Redis | `SCCACHE_REDIS` | `redis://localhost` |

See the [sccache documentation](https://github.com/mozilla/sccache#storage-options) for full configuration options.

## macOS XProtect

On macOS 15+, XProtect scans every binary produced by the compiler. Add your terminal and sccache to **System Settings → Privacy & Security → Developer Tools** to avoid per-file scan overhead during builds.
