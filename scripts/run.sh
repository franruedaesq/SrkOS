#!/usr/bin/env bash
# scripts/run.sh — SrkOS local emulation pipeline
#
# Invoked automatically by `cargo run` (or `cargo run-fw`) via the runner
# configured in .cargo/config.toml for the xtensa-esp32-none-elf target.
#
# Pipeline:
#   1. ELF  →  espflash save-image  →  4 MB flash image
#   2. flash image  →  qemu-system-xtensa (Espressif's ESP32 fork)
#
# ── Prerequisites ─────────────────────────────────────────────────────────────
#
#  1. Espressif Rust toolchain ("esp" channel, includes Xtensa LLVM backend):
#       cargo install espup
#       espup install
#       source ~/export-esp.sh   # add to your shell profile for persistence
#
#  2. espflash >= 3.0 (ELF → flash-image converter + validator):
#       cargo install espflash
#
#  3. Espressif QEMU fork with ESP32 machine support (qemu-system-xtensa):
#       Download a pre-built release for your OS from
#         https://github.com/espressif/qemu/releases
#       Extract it and add the bin/ directory to your PATH:
#         export PATH="/path/to/qemu-esp/bin:$PATH"
#
# ── Usage ─────────────────────────────────────────────────────────────────────
#
#   # Full pipeline (build → image → emulate):
#   cargo run-fw
#   # Equivalent long form:
#   cargo run --target xtensa-esp32-none-elf --features firmware --release
#
# Press Ctrl-A then X inside QEMU to quit the emulator.

set -euo pipefail

# ── Arguments ─────────────────────────────────────────────────────────────────

ELF="${1:?Usage: $0 <path-to-elf>}"
BIN_DIR="$(dirname "$ELF")"
FLASH_BIN="$BIN_DIR/flash.bin"

# ── Hardware constants (ESP32 WROOM-32) ───────────────────────────────────────

CHIP="esp32"
FLASH_SIZE="4mb"                  # 4 MB SPI flash
FLASH_BYTES=$((4 * 1024 * 1024))  # 4 194 304 bytes — full image size for QEMU

# ── Helpers ───────────────────────────────────────────────────────────────────

info()  { printf '[run.sh] %s\n'        "$*"; }
warn()  { printf '[run.sh] WARNING: %s\n' "$*" >&2; }
error() { printf '[run.sh] ERROR: %s\n'   "$*" >&2; }

require_tool() {
    local cmd="$1" hint="$2"
    if ! command -v "$cmd" &>/dev/null; then
        error "'$cmd' is not installed or not in PATH."
        error "  Install: $hint"
        exit 1
    fi
}

# ── Tool checks ───────────────────────────────────────────────────────────────

# 1. Espressif Rust toolchain (needed to compile the firmware).
if ! rustup toolchain list 2>/dev/null | grep -q '^esp'; then
    error "The Espressif 'esp' Rust toolchain is not installed."
    error "  Run:  cargo install espup && espup install"
    error "  Then: source ~/export-esp.sh   (or restart your shell)"
    exit 1
fi

# 2. espflash — converts the ELF into a flash image that includes the
#    ESP32 second-stage bootloader, partition table, and application.
require_tool espflash \
    "cargo install espflash   (requires espflash >= 3.0)"

# Warn if espflash is older than v3.
_ESPF_VER=$(espflash --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
_ESPF_MAJOR="${_ESPF_VER%%.*}"
if [[ "${_ESPF_MAJOR:-0}" -lt 3 ]]; then
    warn "espflash ${_ESPF_VER} detected; version >= 3.0 is recommended."
    warn "  Upgrade: cargo install espflash"
fi

# 3. qemu-system-xtensa — Espressif's QEMU fork with the ESP32 machine model.
require_tool qemu-system-xtensa \
    "Download from https://github.com/espressif/qemu/releases and add bin/ to PATH"

# ── Step 1: ELF → flash image ─────────────────────────────────────────────────
#
# espflash save-image merges:
#   • the built-in ESP32 second-stage bootloader (0x1000)
#   • a default partition table              (0x8000)
#   • the compiled application               (0x10000)
# into a single binary that mirrors the device's SPI flash.

info "Generating flash image from ELF: $(basename "$ELF")"
espflash save-image \
    --chip  "$CHIP"       \
    --flash-size "$FLASH_SIZE" \
    "$ELF"                \
    "$FLASH_BIN"

_IMG_SIZE=$(stat -c%s "$FLASH_BIN" 2>/dev/null || stat -f%z "$FLASH_BIN")
info "Flash image written: $_IMG_SIZE bytes  →  $FLASH_BIN"

# Pad the image to the full 4 MB with 0xFF (erased-sector value).
# QEMU's MTD driver needs the image to be exactly the declared flash size so
# that reads beyond the written regions return the correct erased-byte value.
if [[ "$_IMG_SIZE" -lt "$FLASH_BYTES" ]]; then
    info "Padding flash image: $_IMG_SIZE → $FLASH_BYTES bytes (0xFF fill)..."
    python3 - <<PYEOF
with open("$FLASH_BIN", "ab") as f:
    f.write(b"\xff" * ($FLASH_BYTES - $_IMG_SIZE))
PYEOF
    info "Padded flash image: $(stat -c%s "$FLASH_BIN" 2>/dev/null || stat -f%z "$FLASH_BIN") bytes"
fi

# ── Step 2: Boot in Espressif QEMU ────────────────────────────────────────────
#
# Machine:  esp32  (Xtensa LX6 dual-core, 240 MHz — matches WROOM-32)
# Flash:    raw MTD device fed with the padded flash image
#
# Serial output appears directly in your terminal.
# Exit the emulator with Ctrl-A then X.

info "Booting ESP32 in QEMU  (Ctrl-A X to quit)..."
info "---"
exec qemu-system-xtensa          \
    -nographic                    \
    -machine "$CHIP"              \
    -drive "file=$FLASH_BIN,if=mtd,format=raw"
