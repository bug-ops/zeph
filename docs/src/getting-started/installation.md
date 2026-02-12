# Installation

## From Source

```bash
git clone https://github.com/bug-ops/zeph
cd zeph
cargo build --release
```

The binary is produced at `target/release/zeph`.

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
docker pull ghcr.io/bug-ops/zeph:v0.9.0
```

Images are scanned with [Trivy](https://trivy.dev/) in CI/CD and use Oracle Linux 9 Slim base with **0 HIGH/CRITICAL CVEs**. Multi-platform: linux/amd64, linux/arm64.

See [Docker Deployment](../guide/docker.md) for full deployment options including GPU support and age vault.
