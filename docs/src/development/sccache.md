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
rustc-wrapper = "sccache"
```

If sccache is not installed, Cargo prints a warning and falls back to direct `rustc` invocation. CI jobs that don't need compilation override the wrapper with `RUSTC_WRAPPER=""` (env var takes priority over config file).

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
