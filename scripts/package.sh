#!/usr/bin/env bash
# Builds release binaries and stages a distributable archive under dist/.
# Usage:
#   scripts/package.sh [--target <triple>] [--archive tar.gz|zip]
# Examples:
#   ./scripts/package.sh                                  # build for current host
#   ./scripts/package.sh --target aarch64-unknown-linux-gnu
#   ./scripts/package.sh --target x86_64-unknown-linux-musl --archive zip
#
# When `--target` is omitted the host triple (from `rustc -vV`) is used and no
# `--target` flag is passed to cargo, so cargo falls back to the default
# `target/release/` output directory. When `--target` is given cargo builds
# into `target/<target>/release/`.
set -euo pipefail

TARGET=""
ARCHIVE="tar.gz"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            TARGET="$2"; shift 2 ;;
        --archive)
            ARCHIVE="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: $0 [--target <triple>] [--archive tar.gz|zip]" >&2
            exit 1 ;;
    esac
done

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV | sed -n 's|^host: ||p')"
fi

PACKAGE_NAME="parrot-$TARGET"
DIST_DIR="$PROJECT_ROOT/dist"
STAGE_DIR="$DIST_DIR/$PACKAGE_NAME"

echo "Target: $TARGET"
echo "Building release binaries..."
if [[ -n "$TARGET" ]]; then
    cargo build --release --workspace --manifest-path "$PROJECT_ROOT/Cargo.toml" --target "$TARGET"
    RELEASE_DIR="$PROJECT_ROOT/target/$TARGET/release"
else
    cargo build --release --workspace --manifest-path "$PROJECT_ROOT/Cargo.toml"
    RELEASE_DIR="$PROJECT_ROOT/target/release"
fi

PARROT_BIN="$RELEASE_DIR/parrot"
PARROTD_BIN="$RELEASE_DIR/parrotd"
[[ -f "$PARROT_BIN" ]]   || { echo "parrot not found at $PARROT_BIN" >&2; exit 1; }
[[ -f "$PARROTD_BIN" ]] || { echo "parrotd not found at $PARROTD_BIN" >&2; exit 1; }

echo "Staging distribution..."
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/bin" "$STAGE_DIR/config"

cp "$PARROT_BIN"   "$STAGE_DIR/bin/"
cp "$PARROTD_BIN" "$STAGE_DIR/bin/"
cp "$PROJECT_ROOT/README.md" "$STAGE_DIR/"

USER_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
sed "s|{USER_DATA_DIR}|$USER_DATA_DIR|g" "$PROJECT_ROOT/scripts/parrot.toml.template" \
    > "$STAGE_DIR/config/parrot.toml"

ARCHIVE_PATH="$DIST_DIR/$PACKAGE_NAME.$ARCHIVE"
rm -f "$ARCHIVE_PATH"
case "$ARCHIVE" in
    tar.gz)
        tar -czf "$ARCHIVE_PATH" -C "$DIST_DIR" "$PACKAGE_NAME" ;;
    zip)
        (cd "$DIST_DIR" && zip -qr "$ARCHIVE_PATH" "$PACKAGE_NAME") ;;
    *)
        echo "Unknown archive type: $ARCHIVE" >&2; exit 1 ;;
esac

echo "Done:"
echo "  Target:  $TARGET"
echo "  Stage:   $STAGE_DIR"
echo "  Archive: $ARCHIVE_PATH"