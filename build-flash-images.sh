#!/bin/bash
# Build a merged flash image from the cfg.toml.tmp defaults (Wi-Fi credentials
# and Maps API key left unset), suitable for distribution.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR"
OUTPUT_DIR="$SCRIPT_DIR/flash_images"
OUTPUT="$OUTPUT_DIR/flash_satellite-viewer.bin"

# Source ESP toolchain environment
source "$SCRIPT_DIR/../export-esp.sh"

mkdir -p "$OUTPUT_DIR"

# The build reads cfg.toml, so the template is copied over it for the build.
# Keep the user's own cfg.toml (credentials, API key) and put it back afterwards.
CFG_BACKUP=""
if [ -f "$PROJECT_DIR/cfg.toml" ]; then
    CFG_BACKUP="$(mktemp)"
    cp "$PROJECT_DIR/cfg.toml" "$CFG_BACKUP"
fi
restore_cfg() {
    if [ -n "$CFG_BACKUP" ]; then
        cp "$CFG_BACKUP" "$PROJECT_DIR/cfg.toml"
        rm -f "$CFG_BACKUP"
    else
        rm -f "$PROJECT_DIR/cfg.toml"
    fi
}
trap restore_cfg EXIT

echo "========================================"
echo " Building from cfg.toml.tmp"
echo "========================================"

cp "$PROJECT_DIR/cfg.toml.tmp" "$PROJECT_DIR/cfg.toml"

(cd "$PROJECT_DIR" && cargo build --release)
(cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --merge "$OUTPUT")

echo "========================================"
echo " Build complete"
ls -lh "$OUTPUT"
echo "========================================"
