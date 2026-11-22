#!/bin/sh
# Installs the latest containerbee release. Usage:
#   curl -fsSL https://raw.githubusercontent.com/mhxnahid/cotainerbee/main/install.sh | sh
set -eu

REPO="mhxnahid/cotainerbee"
BIN="containerbee"
DIR="${CONTAINERBEE_INSTALL_DIR:-}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)          TARGET="x86_64-unknown-linux-musl" ;;
  Linux-aarch64|Linux-arm64) TARGET="aarch64-unknown-linux-musl" ;;
  Darwin-arm64)          TARGET="aarch64-apple-darwin" ;;
  Darwin-x86_64)         TARGET="x86_64-apple-darwin" ;;
  *) echo "No prebuilt binary for $(uname -s) $(uname -m). Build from source: cargo install --git https://github.com/$REPO" >&2; exit 1 ;;
esac

if [ -z "$DIR" ]; then
  if [ -w /usr/local/bin ]; then DIR=/usr/local/bin; else DIR="$HOME/.local/bin"; fi
fi
mkdir -p "$DIR"

URL="https://github.com/$REPO/releases/latest/download/$BIN-$TARGET.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $BIN for $TARGET..."
curl -fsSL "$URL" | tar xz -C "$TMP"
install -m 755 "$TMP/$BIN" "$DIR/$BIN"

echo "Installed $DIR/$BIN"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "Note: $DIR is not in your PATH." ;;
esac
