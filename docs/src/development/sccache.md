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

Add the wrapper to your user-level `~/.cargo/config.toml`:

```toml
[build]
rustc-wrapper = "sccache"
```

Alternatively, export the environment variable:

```bash
export RUSTC_WRAPPER=sccache
```

> [!NOTE]
> The workspace does not ship `.cargo/config.toml` to avoid breaking CI jobs that don't install sccache. Configure it locally or via environment variable.

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
