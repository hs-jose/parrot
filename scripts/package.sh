#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="$(rustc -vV | sed -n 's|host: ||p')"
PACKAGE_NAME="parrot-$TARGET"
DIST_DIR="$PROJECT_ROOT/dist"
STAGE_DIR="$DIST_DIR/$PACKAGE_NAME"

echo "Building release binaries..."
cargo build --release --workspace --manifest-path "$PROJECT_ROOT/Cargo.toml"

echo "Staging distribution..."
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/config"

cp "$PROJECT_ROOT/target/release/parrot" "$STAGE_DIR/bin/"
cp "$PROJECT_ROOT/target/release/parrotd" "$STAGE_DIR/bin/"
cp "$PROJECT_ROOT/README.md" "$STAGE_DIR/"

USER_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
sed "s|{USER_DATA_DIR}|$USER_DATA_DIR|g" "$PROJECT_ROOT/scripts/parrot.toml.template" \
    > "$STAGE_DIR/config/parrot.toml"

TARBALL="$DIST_DIR/$PACKAGE_NAME.tar.gz"
rm -f "$TARBALL"
tar -czf "$TARBALL" -C "$DIST_DIR" "$PACKAGE_NAME"

echo "Done:"
echo "  Stage:  $STAGE_DIR"
echo "  Tarball: $TARBALL"
