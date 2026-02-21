# Installation

Install Zeph from source, the install script, pre-built binaries, or Docker.

## Install Script (recommended)

Run the one-liner to download and install the latest release:

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
```

The script detects your OS and architecture, downloads the binary to `~/.zeph/bin/zeph`, and adds it to your `PATH`. Override the install directory with `ZEPH_INSTALL_DIR`:

```bash
ZEPH_INSTALL_DIR=/usr/local/bin curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
```

Install a specific version:

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh -s -- --version v0.11.4
```

After installation, run the configuration wizard:

```bash
zeph init
```

## From crates.io

```bash
cargo install zeph
```

With optional features:

```bash
cargo install zeph --features tui,a2a
```

## From Source

```bash
git clone https://github.com/bug-ops/zeph
cd zeph
cargo build --release
```

The binary is produced at `target/release/zeph`. Run `zeph init` to generate a config file.

## Pre-built Binaries

Download from [GitHub Releases](https://github.com/bug-ops/zeph/releases/latest):

| Platform | Architecture | Download |
|----------|-------------|----------|
| Linux | x86_64 | `zeph-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `zeph-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | x86_64 | `zeph-x86_64-apple-darwin.tar.gz` |
| macOS | aarch64 | `zeph-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `zeph-x86_64-pc-windows-msvc.zip` |

## Docker

Pull the latest image from GitHub Container Registry:

```bash
docker pull ghcr.io/bug-ops/zeph:latest
```

Or use a specific version:

```bash
docker pull ghcr.io/bug-ops/zeph:v0.9.8
```

Images are scanned with [Trivy](https://trivy.dev/) in CI/CD and use Oracle Linux 9 Slim base with **0 HIGH/CRITICAL CVEs**. Multi-platform: linux/amd64, linux/arm64.

See [Docker Deployment](../guides/docker.md) for full deployment options including GPU support and age vault.
