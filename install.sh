#!/bin/sh
# churl installer — POSIX sh, no bashisms.
# Usage:
#   curl -fsSL https://github.com/AlsubaieAli/churl/releases/latest/download/install.sh | sh
#
# Options:
#   --to DIR      Install to DIR instead of ~/.local/bin
#   --tag TAG     Install a specific release (e.g. v0.2.0-beta.1) instead of latest
#   --force       Overwrite an existing churl binary
#   --dry-run     Print resolved URL/target and exit without downloading
#
# Supports: macOS (arm64, x86_64), Linux (x86_64, aarch64) musl static
# Windows users: see README for the pre-built binary table.

set -eu

REPO="AlsubaieAli/churl"
BIN="churl"
DEFAULT_INSTALL_DIR="${HOME}/.local/bin"

# --- option parsing (both --opt VALUE and --opt=VALUE forms) ---
TO_DIR=""
TAG=""
FORCE=0
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --to=*) TO_DIR="${1#--to=}" ;;
    --to)
      [ $# -ge 2 ] || { printf 'error: --to requires a value\n' >&2; exit 1; }
      TO_DIR="$2"; shift
      ;;
    --tag=*) TAG="${1#--tag=}" ;;
    --tag)
      [ $# -ge 2 ] || { printf 'error: --tag requires a value\n' >&2; exit 1; }
      TAG="$2"; shift
      ;;
    --force) FORCE=1 ;;
    --dry-run) DRY_RUN=1 ;;
    *)
      printf 'error: unknown option %s\n' "$1" >&2
      exit 1
      ;;
  esac
  shift
done

INSTALL_DIR="${TO_DIR:-$DEFAULT_INSTALL_DIR}"

# --- detect OS + arch, map to release target triple ---
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64)        TARGET="x86_64-apple-darwin" ;;
      *)
        printf 'error: unsupported macOS arch: %s\n' "$ARCH" >&2
        exit 1
        ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
      x86_64)        TARGET="x86_64-unknown-linux-musl" ;;
      *)
        printf 'error: unsupported Linux arch: %s\n' "$ARCH" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    printf 'error: unsupported OS: %s\n' "$OS" >&2
    printf 'Windows users: download the pre-built binary from https://github.com/%s/releases\n' "$REPO" >&2
    exit 1
    ;;
esac

ARCHIVE="${BIN}-${TARGET}.tar.gz"
# `latest` never resolves to a prerelease — betas are only reachable via --tag.
if [ -n "$TAG" ]; then
  BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
else
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
fi
ARCHIVE_URL="${BASE_URL}/${ARCHIVE}"
CHECKSUM_URL="${BASE_URL}/${ARCHIVE}.sha256"

if [ "$DRY_RUN" = "1" ]; then
  printf 'dry-run: target  = %s\n' "$TARGET"
  printf 'dry-run: archive = %s\n' "$ARCHIVE_URL"
  printf 'dry-run: install = %s/%s\n' "$INSTALL_DIR" "$BIN"
  exit 0
fi

# --- check for existing binary ---
DEST="${INSTALL_DIR}/${BIN}"
if [ -e "$DEST" ] && [ "$FORCE" = "0" ]; then
  printf 'error: %s already exists — use --force to overwrite\n' "$DEST" >&2
  exit 1
fi

# --- download into a temp directory ---
TMP_DIR="$(mktemp -d)"
# shellcheck disable=SC2064
trap "rm -rf '$TMP_DIR'" EXIT INT TERM

printf 'Downloading %s ...\n' "$ARCHIVE_URL"

if command -v curl > /dev/null 2>&1; then
  curl -fsSL "$ARCHIVE_URL"  -o "${TMP_DIR}/${ARCHIVE}"
  curl -fsSL "$CHECKSUM_URL" -o "${TMP_DIR}/${ARCHIVE}.sha256"
elif command -v wget > /dev/null 2>&1; then
  wget -q "$ARCHIVE_URL"  -O "${TMP_DIR}/${ARCHIVE}"
  wget -q "$CHECKSUM_URL" -O "${TMP_DIR}/${ARCHIVE}.sha256"
else
  printf 'error: neither curl nor wget is available\n' >&2
  exit 1
fi

# --- verify sha256 checksum ---
printf 'Verifying checksum ...\n'
cd "$TMP_DIR"
if command -v sha256sum > /dev/null 2>&1; then
  sha256sum -c "${ARCHIVE}.sha256"
elif command -v shasum > /dev/null 2>&1; then
  shasum -a 256 -c "${ARCHIVE}.sha256"
else
  printf 'warning: no sha256 tool found, skipping checksum verification\n' >&2
fi
cd - > /dev/null

# --- extract + install ---
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$TMP_DIR"
mkdir -p "$INSTALL_DIR"
mv "${TMP_DIR}/${BIN}" "$DEST"
chmod +x "$DEST"

printf 'Installed %s to %s\n' "$BIN" "$DEST"

# --- PATH hint ---
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    printf '\nNOTE: %s is not in your PATH.\n' "$INSTALL_DIR"
    printf 'Add the following to your shell profile (.bashrc, .zshrc, etc.):\n'
    # The $PATH below must appear literally in the printed hint.
    # shellcheck disable=SC2016
    printf '  export PATH="$PATH:%s"\n' "$INSTALL_DIR"
    ;;
esac
