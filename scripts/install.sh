#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${PARROT_INSTALL_DIR:-$HOME/.parrot}"
BIN_DIR="$INSTALL_DIR/bin"
CONFIG_DIR="$INSTALL_DIR/config"

mkdir -p "$BIN_DIR" "$CONFIG_DIR"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="$(rustc -vV | sed -n 's|host: ||p')"
SOURCE_DIR="$SCRIPT_DIR/../dist/parrot-$TARGET"

if [ ! -d "$SOURCE_DIR" ]; then
    echo "Distribution not found at $SOURCE_DIR. Run scripts/package.sh first." >&2
    exit 1
fi

cp "$SOURCE_DIR/bin/parrot" "$BIN_DIR/"
cp "$SOURCE_DIR/bin/parrotd" "$BIN_DIR/"

if [ ! -f "$CONFIG_DIR/parrot.toml" ]; then
    cp "$SOURCE_DIR/config/parrot.toml" "$CONFIG_DIR/parrot.toml"
    echo "Created default config at $CONFIG_DIR/parrot.toml"
fi

echo "Installed parrot to $BIN_DIR"
echo "Add the following directory to your PATH:"
echo "  $BIN_DIR"
