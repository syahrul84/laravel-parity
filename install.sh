#!/bin/sh
# Install laravel-parity (macOS / Linux) and register it with Claude Code:
#   curl -fsSL https://raw.githubusercontent.com/syahrul84/laravel-parity/main/install.sh | sh
set -eu

REPO="syahrul84/laravel-parity"
BIN_DIR="${LARAVEL_PARITY_BIN:-$HOME/.local/bin}"
HOME_DIR="${LARAVEL_PARITY_HOME:-$HOME/.laravel-parity}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo "No ready-made build for $(uname -s) $(uname -m); build it with: cargo install --git https://github.com/$REPO" >&2; exit 1 ;;
esac

tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
[ -n "$tag" ] || { echo "Could not find the latest release of $REPO" >&2; exit 1; }
asset="laravel-parity-$target.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$asset" "https://github.com/$REPO/releases/download/$tag/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "https://github.com/$REPO/releases/download/$tag/SHA256SUMS"
expected=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/$asset" | cut -d' ' -f1); else actual=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ "$expected" = "$actual" ] || { echo "Checksum mismatch for $asset; nothing installed" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BIN_DIR" "$HOME_DIR/pairs"
install -m 755 "$tmp/laravel-parity" "$BIN_DIR/laravel-parity"
echo "Installed laravel-parity $tag in $BIN_DIR (pair files and runs in $HOME_DIR)"

if command -v claude >/dev/null; then
  claude mcp remove --scope user laravel-parity >/dev/null 2>&1 || true
  claude mcp add --scope user laravel-parity -e LARAVEL_PARITY_HOME="$HOME_DIR" -- "$BIN_DIR/laravel-parity" serve
  echo "Registered the MCP server with Claude Code (all projects)."
else
  echo "Claude Code not found; register later with:"
  echo "  claude mcp add --scope user laravel-parity -e LARAVEL_PARITY_HOME=$HOME_DIR -- $BIN_DIR/laravel-parity serve"
fi
echo "Next: laravel-parity guide"
