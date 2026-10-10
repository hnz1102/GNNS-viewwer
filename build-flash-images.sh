#!/bin/bash
# Build merged flash images from the cfg.toml.tmp defaults (Wi-Fi credentials
# and Maps API key left unset), suitable for distribution. Two variants:
#   flash_satellite-viewer.bin        quake_enable = "false"  (no earthquake/EEW feed)
#   flash_satellite-viewer-quake.bin  quake_enable = "true"   (P2P地震情報 earthquake list + EEW)
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR"
OUTPUT_DIR="$SCRIPT_DIR/flash_images"
OUTPUT_OFF="$OUTPUT_DIR/flash_satellite-viewer.bin"
OUTPUT_ON="$OUTPUT_DIR/flash_satellite-viewer-quake.bin"

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

# Build one variant: $1 = quake_enable value ("true"/"false"), $2 = output image.
build_variant() {
    local quake="$1" out="$2"

    echo "========================================"
    echo " Building from cfg.toml.tmp (quake_enable=$quake)"
    echo "========================================"

    cp "$PROJECT_DIR/cfg.toml.tmp" "$PROJECT_DIR/cfg.toml"
    sed -i -E "s/^quake_enable[[:space:]]*=.*/quake_enable = \"$quake\"/" "$PROJECT_DIR/cfg.toml"
    if ! grep -q "^quake_enable = \"$quake\"" "$PROJECT_DIR/cfg.toml"; then
        echo "ERROR: could not set quake_enable=$quake in cfg.toml (missing from cfg.toml.tmp?)" >&2
        exit 1
    fi

    # cfg.toml is read by a macro at compile time and cargo does not track it,
    # so force the crate to be recompiled or the second variant would reuse the
    # first one's settings.
    touch "$PROJECT_DIR/src/main.rs"

    (cd "$PROJECT_DIR" && cargo build --release)
    (cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --merge "$out")
}

build_variant false "$OUTPUT_OFF"
build_variant true  "$OUTPUT_ON"

echo "========================================"
echo " Build complete"
ls -lh "$OUTPUT_OFF" "$OUTPUT_ON"
echo "========================================"
