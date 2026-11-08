#!/usr/bin/env bash
#
# ARGUS Phase I — build everything, one command.
#
#   ./build.sh
#
# Produces:
#   dist/BOOTX64.EFI        the UEFI loader
#   dist/ARGUSKRN.ELF       the kernel
#   dist/argus-disk.img     a 16 MiB MBR disk with a 15 MiB FAT16 ESP, bootable as a
#                           UEFI disk (dd it to a USB stick and it is the same
#                           thing the owner runs under QEMU)
#   dist/esp/               the same tree as plain files, for inspection
#
# Exits non-zero if any step fails. Nothing here is optional or best-effort.
#
# Environment:
#   CARGO_TARGET_DIR   where cargo puts build output. Defaults to ./target.
#                      On the team build machine set it to a path on the large
#                      volume: CARGO_TARGET_DIR=/opt/argus-target ./build.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

BOOT_TARGET="x86_64-unknown-uefi"
KERNEL_TARGET="x86_64-unknown-none"
DIST="$ROOT/dist"
ESP="$DIST/esp"
IMG="$DIST/argus-disk.img"
IMG_SIZE_MB=16
ESP_SIZE_MB=15

say() { printf '\033[1m[build]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[build] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# --- toolchain --------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x /opt/rust/cargo/bin/cargo ]; then
        export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}"
        export CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"
        export PATH="/opt/rust/cargo/bin:$PATH"
    else
        die "cargo not found. Install Rust: https://rustup.rs"
    fi
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

say "cargo:  $(cargo --version)"
say "rustc:  $(rustc --version)"

for target in "$BOOT_TARGET" "$KERNEL_TARGET"; do
    if ! rustc --print target-list | grep -qx "$target"; then
        die "rustc does not know target $target"
    fi
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
        die "target $target is not installed. Run: rustup target add $target"
    fi
done

# --- host tools for the FAT image -------------------------------------------
for tool in mkfs.vfat mmd mcopy; do
    command -v "$tool" >/dev/null 2>&1 \
        || die "$tool not found. Install dosfstools and mtools (apt-get install -y dosfstools mtools)"
done

say "target dir: $CARGO_TARGET_DIR"

# --- build ------------------------------------------------------------------
say "building kernel ($KERNEL_TARGET, release)"
cargo build --release -p argus-kernel --target "$KERNEL_TARGET"

say "building bootloader ($BOOT_TARGET, release)"
cargo build --release -p argus-bootloader --target "$BOOT_TARGET"

KERNEL_ELF="$CARGO_TARGET_DIR/$KERNEL_TARGET/release/argus-kernel"
BOOT_EFI="$CARGO_TARGET_DIR/$BOOT_TARGET/release/argus-bootloader.efi"

[ -f "$KERNEL_ELF" ] || die "kernel ELF not produced at $KERNEL_ELF"
[ -f "$BOOT_EFI" ]   || die "loader PE not produced at $BOOT_EFI"

# --- assemble the ESP -------------------------------------------------------
say "assembling ESP at $ESP"
rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT"
cp "$BOOT_EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"
cp "$KERNEL_ELF" "$ESP/ARGUSKRN.ELF"
cp "$BOOT_EFI" "$DIST/BOOTX64.EFI"
cp "$KERNEL_ELF" "$DIST/ARGUSKRN.ELF"

# --- disk image -------------------------------------------------------------
#
# The image is a real, bootable disk: an MBR partition table with one FAT16
# EFI System Partition starting at LBA 2048 (1 MiB). This matters. A bare
# FAT filesystem with no partition table ("superfloppy") is read by OVMF's
# BDS, but PartitionDxe never creates a partition child handle for it, and
# the resulting handle topology leaves the loader unable to find a volume —
# measured, see README "Firmware notes". With a partition table the chain
# VirtioBlk -> Partition -> Fat binds normally.
#
# 16 MiB total: 1 MiB of MBR/reserved + 15 MiB FAT16 ESP.

say "creating FAT16 ESP volume (${ESP_SIZE_MB} MiB)"
ESP_FAT="$DIST/esp-volume.fat"
rm -f "$ESP_FAT"
dd if=/dev/zero of="$ESP_FAT" bs=1M count="$ESP_SIZE_MB" status=none
mkfs.vfat -F 16 -n ARGUSESP "$ESP_FAT" >/dev/null
export MTOOLS_SKIP_CHECK=1
mmd -i "$ESP_FAT" ::/EFI
mmd -i "$ESP_FAT" ::/EFI/BOOT
mcopy -i "$ESP_FAT" "$ESP/EFI/BOOT/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$ESP_FAT" "$ESP/ARGUSKRN.ELF" ::/ARGUSKRN.ELF

say "creating MBR disk image $IMG (${IMG_SIZE_MB} MiB)"
rm -f "$IMG"
dd if=/dev/zero of="$IMG" bs=1M count="$IMG_SIZE_MB" status=none

# One MBR partition entry: type 0x0E (FAT16 LBA), start LBA 2048, 30720
# sectors (15 MiB). CHS fields are set to the "use LBA" sentinel 0xFEFFFE,
# which is what every modern partitioner writes.
{
    dd if=/dev/zero bs=1 count=446 status=none
    printf '\x00\xfe\xff\xff\x0e\xfe\xff\xff'
    printf '\x00\x08\x00\x00'   # start LBA 2048, little-endian
    printf '\x00\x78\x00\x00'   # 30720 sectors, little-endian
    dd if=/dev/zero bs=1 count=48 status=none
    printf '\x55\xaa'
} > "$DIST/mbr.bin"
dd if="$DIST/mbr.bin" of="$IMG" conv=notrunc status=none
dd if="$ESP_FAT" of="$IMG" bs=1M seek=1 conv=notrunc status=none
rm -f "$DIST/mbr.bin" "$ESP_FAT"

# --- verify the image really contains what we think -------------------------
say "verifying image contents"
# `mdir -b` prints full 8.3 names ("::/EFI/BOOT/BOOTX64.EFI"); the default
# format pads the extension with spaces and would not match a literal grep.
echo "$(mdir -b -i "$IMG@@1M" ::/EFI/BOOT 2>/dev/null)" | grep -q "BOOTX64.EFI" \
    || die "BOOTX64.EFI missing from the ESP"
echo "$(mdir -b -i "$IMG@@1M" ::/ 2>/dev/null)" | grep -q "ARGUSKRN.ELF" \
    || die "ARGUSKRN.ELF missing from the ESP root"

say "OK"
printf '\n'
printf '  loader   %s  (%s bytes)\n' "$DIST/BOOTX64.EFI" "$(stat -c%s "$DIST/BOOTX64.EFI")"
printf '  kernel   %s  (%s bytes)\n' "$DIST/ARGUSKRN.ELF" "$(stat -c%s "$DIST/ARGUSKRN.ELF")"
printf '  image    %s  (%s bytes)\n' "$IMG" "$(stat -c%s "$IMG")"
printf '\n  next: ./run.sh\n\n'
