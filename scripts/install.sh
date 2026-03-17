#!/bin/sh
# install.sh — one-liner installer for zeph
# Usage: curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
#        or: sh install.sh [--version v0.9.9] [--help]
set -eu
umask 022

REPO="bug-ops/zeph"
BINARY_NAME="zeph"
INSTALL_DIR="${ZEPH_INSTALL_DIR:-$HOME/.zeph/bin}"

VERSION=""
ZEPH_TMP=""

usage() {
    cat <<EOF
Usage: install.sh [OPTIONS]

Options:
  --version <tag>   Install a specific version (e.g. v0.9.9). Default: latest.
  --help            Show this help message.

Environment:
  ZEPH_INSTALL_DIR  Installation directory. Default: ~/.zeph/bin
EOF
}

cleanup() {
    if [ -n "$ZEPH_TMP" ] && [ -d "$ZEPH_TMP" ]; then
        rm -rf "$ZEPH_TMP"
    fi
}

trap cleanup EXIT

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                if [ $# -lt 2 ]; then
                    printf 'Error: --version requires a value\n' >&2
                    exit 1
                fi
                shift
                VERSION="$1"
                case "$VERSION" in
                    v[0-9]*.[0-9]*.[0-9]*) ;;
                    *)
                        printf 'Invalid version format: %s (expected vX.Y.Z)\n' "$VERSION" >&2
                        exit 1
                        ;;
                esac
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                printf 'Unknown option: %s\n' "$1" >&2
                usage >&2
                exit 1
                ;;
        esac
        shift
    done
}

detect_platform() {
    OS=$(uname -s)
    ARCH=$(uname -m)

    case "$OS" in
        Linux)
            case "$ARCH" in
                x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
                aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
                *)
                    printf 'Unsupported architecture: %s\n' "$ARCH" >&2
                    printf 'Supported: x86_64, aarch64\n' >&2
                    exit 1
                    ;;
            esac
            ;;
        Darwin)
            case "$ARCH" in
                x86_64)        TARGET="x86_64-apple-darwin" ;;
                arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
                *)
                    printf 'Unsupported architecture: %s\n' "$ARCH" >&2
                    printf 'Supported: x86_64, arm64\n' >&2
                    exit 1
                    ;;
            esac
            ;;
        *)
            printf 'Unsupported OS: %s\n' "$OS" >&2
            printf 'Supported: Linux, Darwin\n' >&2
            printf 'Windows users: download the zip from https://github.com/%s/releases\n' "$REPO" >&2
            exit 1
            ;;
    esac
}

resolve_url() {
    ARCHIVE="${BINARY_NAME}-${TARGET}.tar.gz"
    CHECKSUM="${ARCHIVE}.sha256"

    if [ -n "$VERSION" ]; then
        BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
    else
        BASE_URL="https://github.com/${REPO}/releases/latest/download"
    fi

    ARCHIVE_URL="${BASE_URL}/${ARCHIVE}"
    CHECKSUM_URL="${BASE_URL}/${CHECKSUM}"
}

download_file() {
    URL="$1"
    DEST="$2"

    if command -v curl > /dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$DEST" "$URL"
    elif command -v wget > /dev/null 2>&1; then
        wget -q -O "$DEST" "$URL"
    else
        printf 'Neither curl nor wget found. Install one of them and retry.\n' >&2
        exit 1
    fi
}

verify_checksum() {
    ARCHIVE_PATH="$1"
    CHECKSUM_PATH="$2"

    EXPECTED=$(awk '{print $1}' "$CHECKSUM_PATH")
    BASENAME=$(basename "$ARCHIVE_PATH")

    if command -v shasum > /dev/null 2>&1; then
        ACTUAL=$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')
    elif command -v sha256sum > /dev/null 2>&1; then
        ACTUAL=$(sha256sum "$ARCHIVE_PATH" | awk '{print $1}')
    else
        printf 'Neither shasum nor sha256sum found. Cannot verify checksum.\n' >&2
        exit 1
    fi

    if [ "$EXPECTED" != "$ACTUAL" ]; then
        printf 'Checksum mismatch for %s\n' "$BASENAME" >&2
        printf '  expected: %s\n' "$EXPECTED" >&2
        printf '  actual:   %s\n' "$ACTUAL" >&2
        printf 'Aborting installation for security reasons.\n' >&2
        exit 1
    fi
}

extract_and_install() {
    mkdir -p "$INSTALL_DIR"
    tar xzf "$ZEPH_TMP/$ARCHIVE" -C "$ZEPH_TMP"
    chmod 0755 "$ZEPH_TMP/$BINARY_NAME"
    mv "$ZEPH_TMP/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
}

# Append a line to a file only if it is not already present.
append_if_absent() {
    FILE="$1"
    LINE="$2"
    if [ -f "$FILE" ] && grep -qF "$LINE" "$FILE"; then
        return 0
    fi
    printf '\n%s\n' "$LINE" >> "$FILE"
}

configure_path() {
    EXPORT_LINE="export PATH=\"${INSTALL_DIR}:\$PATH\""
    FISH_LINE="set -gx PATH \"${INSTALL_DIR}\" \$PATH"

    # bash
    for RC in "$HOME/.bashrc" "$HOME/.bash_profile"; do
        if [ -f "$RC" ]; then
            append_if_absent "$RC" "$EXPORT_LINE"
        fi
    done

    # zsh
    if [ -f "$HOME/.zshrc" ]; then
        append_if_absent "$HOME/.zshrc" "$EXPORT_LINE"
    fi

    # fish
    FISH_CONF_DIR="$HOME/.config/fish/conf.d"
    if [ -d "$FISH_CONF_DIR" ]; then
        FISH_FILE="$FISH_CONF_DIR/zeph.fish"
        if ! grep -qF "$FISH_LINE" "$FISH_FILE" 2>/dev/null; then
            printf '%s\n' "$FISH_LINE" > "$FISH_FILE"
        fi
    fi
}

print_success() {
    printf '\n'
    printf 'zeph installed to %s/%s\n' "$INSTALL_DIR" "$BINARY_NAME"
    printf '\n'
    printf 'Add it to your PATH if not already active:\n'
    # shellcheck disable=SC2016
    printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    printf '\n'
    printf 'Get started:\n'
    printf '  zeph init\n'
    printf '\n'
}

main() {
    parse_args "$@"
    detect_platform
    resolve_url

    ZEPH_TMP=$(mktemp -d)

    printf 'Downloading %s ...\n' "$ARCHIVE_URL"
    download_file "$ARCHIVE_URL" "$ZEPH_TMP/$ARCHIVE"

    printf 'Verifying checksum ...\n'
    download_file "$CHECKSUM_URL" "$ZEPH_TMP/$CHECKSUM"
    verify_checksum "$ZEPH_TMP/$ARCHIVE" "$ZEPH_TMP/$CHECKSUM"

    printf 'Installing to %s ...\n' "$INSTALL_DIR"
    extract_and_install

    configure_path
    print_success
}

main "$@"
