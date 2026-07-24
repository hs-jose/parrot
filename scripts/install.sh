#!/usr/bin/env bash
# Installs parrot into ~/.parrot/bin from a staged distribution.
# Usage:
#   ./scripts/install.sh [--target <triple>] [--install-dir <path>]
# Examples:
#   ./scripts/install.sh
#   ./scripts/install.sh --target aarch64-unknown-linux-gnu
#   PARROT_INSTALL_DIR=/opt/parrot ./scripts/install.sh
#
# Config (`parrot.toml`) is created from the template on first install only;
# re-running install.sh preserves the existing config (e.g. user's API key).
set -euo pipefail

TARGET=""
INSTALL_DIR="${PARROT_INSTALL_DIR:-$HOME/.parrot}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            TARGET="$2"; shift 2 ;;
        --install-dir)
            INSTALL_DIR="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--target <triple>] [--install-dir <path>]" >&2
            exit 1 ;;
    esac
done

BIN_DIR="$INSTALL_DIR/bin"
CONFIG_DIR="$INSTALL_DIR/config"
mkdir -p "$BIN_DIR" "$CONFIG_DIR"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV | sed -n 's|^host: ||p')"
fi
SOURCE_DIR="$SCRIPT_DIR/../dist/parrot-$TARGET"

if [[ ! -d "$SOURCE_DIR" ]]; then
    echo "Distribution not found at $SOURCE_DIR. Run scripts/package.sh --target $TARGET first." >&2
    exit 1
fi

PARROT_SRC="$SOURCE_DIR/bin/parrot"
PARROTD_SRC="$SOURCE_DIR/bin/parrotd"
[[ -f "$PARROT_SRC" ]]   || { echo "$PARROT_SRC not found" >&2; exit 1; }
[[ -f "$PARROTD_SRC" ]] || { echo "$PARROTD_SRC not found" >&2; exit 1; }

cp "$PARROT_SRC"   "$BIN_DIR/"
cp "$PARROTD_SRC" "$BIN_DIR/"

if [[ ! -f "$CONFIG_DIR/parrot.toml" ]]; then
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    USER_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
    sed "s|{USER_DATA_DIR}|$USER_DATA_DIR|g" "$PROJECT_ROOT/scripts/parrot.toml.template" \
        > "$CONFIG_DIR/parrot.toml"
    echo "Created default config at $CONFIG_DIR/parrot.toml"
fi

echo "Installed parrot to $BIN_DIR"
echo "Add the following directory to your PATH:"
echo "  $BIN_DIR"