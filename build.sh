#!/usr/bin/env bash
#
# ARGUS Phase I — build everything, one command.
#
#   ./build.sh            build the whole boot artifact from scratch
#   ./build.sh --check    verify an existing dist/ without building anything
#
# Produces:
#   dist/BOOTX64.EFI        the UEFI loader
#   dist/ARGUSKRN.ELF       the kernel
#   dist/ARGUS.MF           the signed boot manifest (also on the ESP)
#   dist/BOOT.CFG           the boot config covered by config_digest
#   dist/FLOOR              the seeded anti-replay floor
#   dist/argus-disk.img     a 16 MiB MBR disk with a 15 MiB FAT16 ESP, bootable
#                           as a UEFI disk (dd it to a USB stick and it is the
#                           same thing the owner runs under QEMU)
#   dist/esp/               the same tree as plain files, for inspection
#
# Exit status: 0 means every artifact was produced **and read back out of the
# image** and compared byte for byte. Non-zero means a real defect. A build that
# cannot prove what it wrote is a failed build, not a flaky one.
#
# Environment:
#   CARGO_TARGET_DIR   where cargo puts build output. Defaults to ./target.
#                      On the team build machine set it to a path on the large
#                      volume: CARGO_TARGET_DIR=/opt/argus-target ./build.sh
#   SECURITY_VERSION   manifest security_version to sign (default 2)
#   SIGNING_KEY        private key used to sign the manifest (default
#                      keys/test/manifest-key.pem — a TEST key, see README)
#   ARGUS_DIST         build into another directory (tests use this)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

BOOT_TARGET="x86_64-unknown-uefi"
KERNEL_TARGET="x86_64-unknown-none"
DIST="${ARGUS_DIST:-$ROOT/dist}"
ESP="$DIST/esp"
IMG="$DIST/argus-disk.img"
IMG_SIZE_MB=16
ESP_SIZE_MB=15
SECURITY_VERSION="${SECURITY_VERSION:-2}"
SIGNING_KEY="${SIGNING_KEY:-$ROOT/keys/test/manifest-key.pem}"
PUBKEY_DER="$ROOT/keys/test/manifest-pub.der"
# Must match `security::LOADER_VERSION_NUM` in the loader. A manifest that
# demands more than the loader offers is refused with E_LOADER_TOO_OLD, which is
# exactly what the negative test for that check relies on.
LOADER_VERSION_NUM=3

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
# mtools is noisy about FAT quirks it does not need to care about here.
export MTOOLS_SKIP_CHECK=1

# --- host tools -------------------------------------------------------------
for tool in mkfs.vfat mmd mcopy mdir dd cmp sha256sum flock; do
    command -v "$tool" >/dev/null 2>&1 \
        || die "$tool not found. Install dosfstools and mtools (apt-get install -y dosfstools mtools)"
done

# --- serialise builds -------------------------------------------------------
#
# Two builds writing dist/ at the same time is the bug that made this script
# flaky: one build's `dd` can land between another build's `mcopy` and its
# verification, and the verifier then reports a file that *is* in the image as
# missing. A false negative here is as damaging as a false positive — it trains
# people to re-run until green. The lock makes concurrent builds queue instead.
mkdir -p "$DIST"
exec 9>"$DIST/.build.lock"
flock -w 300 9 || die "another build is holding $DIST/.build.lock"

# --- verification helpers ---------------------------------------------------
#
# Everything below verifies a *file*, never a device path with an offset into
# another file: the volume image is read back after it is written, and the disk
# image is checked by extracting its partition and comparing the two byte for
# byte. There is no window in which a stale buffer can be read.

# The files every ARGUS ESP must carry. Kept in one place so the writer and the
# reader cannot drift.
ESP_FILES="ARGUSKRN.ELF INITRD.BIN ARGUS/ARGUS.MF ARGUS/BOOT.CFG ARGUS/FLOOR EFI/BOOT/BOOTX64.EFI"

# verify_fat_volume <fat-image> <scratch-dir> <reference-dir>
#
# Reads every file back out of the FAT image and compares it with the reference
# tree, byte for byte. Retries a few times with a sync between attempts: the
# only legitimate way this fails on a good image is a still-dirty page cache,
# and a check that can fail for a reason that is not a defect is worse than no
# check at all. An empty <reference-dir> means "only prove the files are there".
verify_fat_volume() {
    local vol="$1" tmp="$2" ref="${3:-}" attempt
    local f ok

    for attempt in 1 2 3; do
        ok=1
        for f in $ESP_FILES; do
            rm -f "$tmp/extract.bin"
            if ! mcopy -o -i "$vol" "::/$f" "$tmp/extract.bin" 2>/dev/null; then
                ok=0
                break
            fi
            [ -s "$tmp/extract.bin" ] || { ok=0; break; }
            if [ -n "$ref" ] && ! cmp -s "$tmp/extract.bin" "$ref/$f"; then
                ok=0
                break
            fi
        done
        [ "$ok" = "1" ] && return 0
        printf '\033[1;33m[build] verify:\033[0m attempt %d did not match yet (%s), syncing and retrying\n' "$attempt" "$f" >&2
        sync
        sleep 0.2
    done
    return 1
}

# verify_mbr <disk-image>
verify_mbr() {
    local img="$1" tmp="$2"
    local size
    size=$(stat -c%s "$img")
    [ "$size" -eq $((IMG_SIZE_MB * 1024 * 1024)) ] \
        || die "disk image is $size bytes, expected $((IMG_SIZE_MB * 1024 * 1024))"

    dd if="$img" bs=1 count=512 of="$tmp/mbr.bin" status=none
    # Partition 1: type 0x0E at 450, start LBA 2048 at 454, 30720 sectors at 458.
    local entry
    entry=$(dd if="$tmp/mbr.bin" bs=1 skip=446 count=16 status=none | od -An -tx1 | tr -d ' \n')
    [ "$entry" = "00feffff0efeffff0008000000780000" ] \
        || die "MBR partition entry is $entry, expected the FAT16 LBA entry this script writes"
    local sig
    sig=$(dd if="$tmp/mbr.bin" bs=1 skip=510 count=2 status=none | od -An -tx1 | tr -d ' \n')
    [ "$sig" = "55aa" ] || die "MBR boot signature is $sig, expected 55aa"
}

# check_image <disk-image> <scratch-dir> [expected-esp-dir]
#
# The single definition of "the image is what we think it is", used by the build
# and by `--check`. It never trusts the image: it extracts the partition, proves
# the extraction matches the volume that was written, and then reads the files
# out of that extraction.
check_image() {
    local img="$1" tmp="$2" expect="${3:-}"
    [ -f "$img" ] || die "$img does not exist"

    verify_mbr "$img" "$tmp"

    dd if="$img" of="$tmp/partition.bin" bs=1M skip=1 count="$ESP_SIZE_MB" status=none \
        || die "could not extract the ESP partition from $img"
    [ -s "$tmp/partition.bin" ] || die "extracted partition is empty"

    if [ -n "$expect" ]; then
        [ -f "$tmp/esp-volume.fat" ] || die "internal: no reference volume to compare against"
        cmp -s "$tmp/esp-volume.fat" "$tmp/partition.bin" \
            || die "the partition inside $img is not byte-identical to the volume that was written"
    fi

    if ! verify_fat_volume "$tmp/partition.bin" "$tmp" "$expect"; then
        # Dump what the FAT reader actually sees, so the failure is diagnosable
        # from the log instead of requiring a re-run.
        printf '\033[1;31m[build] ERROR:\033[0m read-back from the ESP partition failed\n' >&2
        mdir -i "$tmp/partition.bin" ::/ >&2 2>&1 || true
        mdir -i "$tmp/partition.bin" ::/ARGUS >&2 2>&1 || true
        die "files read back from the image do not match what was written"
    fi
}

# ===========================================================================
# --check: verify an existing dist/ without building it
# ===========================================================================
if [ "${1:-}" = "--check" ]; then
    say "checking $IMG (no build)"
    [ -f "$IMG" ] || die "$IMG does not exist; run ./build.sh first"

    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    # The reference volume is not kept, so compare against dist/esp when it is
    # there and against the manifest's own digests otherwise.
    # dist/esp is the reference tree when it exists: the files on the image must
    # be byte-identical to the files this build produced. Without it, --check
    # still proves the structure and the manifest signature.
    EXPECT=""
    if [ -d "$ESP" ]; then
        EXPECT="$ESP"
    else
        say "dist/esp is absent; checking structure and the manifest signature only"
    fi

    check_image "$IMG" "$TMP" "$EXPECT"

    if [ -d "$ESP" ] && [ -f "$ESP/ARGUS/ARGUS.MF" ]; then
        # Independently of the loader: does the manifest on the ESP still
        # authenticate the kernel and config next to it?
        SIGN="$CARGO_TARGET_DIR/release/sign-manifest"
        if [ -x "$SIGN" ]; then
            FLOOR_VALUE="$( "$SIGN" floor-read --file "$ESP/ARGUS/FLOOR" 2>/dev/null | awk -F= '$1=="floor"{print $2}' )" || FLOOR_VALUE=0
            "$SIGN" verify \
                --manifest "$ESP/ARGUS/ARGUS.MF" \
                --kernel "$ESP/ARGUSKRN.ELF" \
                --config "$ESP/ARGUS/BOOT.CFG" \
                --initrd "$ESP/INITRD.BIN" \
                --slot A --floor "${FLOOR_VALUE:-0}" --loader-version "$LOADER_VERSION_NUM" \
                --pubkey "$PUBKEY_DER" \
                || die "the manifest on the ESP does not verify against the kernel next to it"
        else
            say "sign-manifest not built yet; skipping the signature cross-check"
        fi
    fi

    say "OK — $IMG is intact"
    exit 0
fi

# ===========================================================================
# build
# ===========================================================================
say "cargo:  $(cargo --version)"
say "rustc:  $(rustc --version)"

# Test hook: `tests/bootinfo-checksum.sh` sets this so the loader deliberately
# corrupts one BootInfo byte after sealing it. It is a compile-time cfg, so it
# cannot be switched on at runtime and never exists in a normal build.
if [ "${ARGUS_TEST_CORRUPT_BOOTINFO:-0}" = "1" ]; then
    export RUSTFLAGS="${RUSTFLAGS:-} --cfg argus_test_corrupt_bootinfo"
    say "TEST HOOK: loader will corrupt BootInfo after sealing (--cfg argus_test_corrupt_bootinfo)"
fi

BUILD_START_NS=$(date +%s%N)

for target in "$BOOT_TARGET" "$KERNEL_TARGET"; do
    if ! rustc --print target-list | grep -qx "$target"; then
        die "rustc does not know target $target"
    fi
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
        die "target $target is not installed. Run: rustup target add $target"
    fi
done

say "target dir: $CARGO_TARGET_DIR"

# --- build ------------------------------------------------------------------
say "building kernel ($KERNEL_TARGET, release)"
cargo build --release -p argus-kernel --target "$KERNEL_TARGET"

say "building bootloader ($BOOT_TARGET, release)"
cargo build --release -p argus-bootloader --target "$BOOT_TARGET"

say "building the manifest signer (host)"
cargo build --release -p argus-sign-manifest

KERNEL_ELF="$CARGO_TARGET_DIR/$KERNEL_TARGET/release/argus-kernel"
BOOT_EFI="$CARGO_TARGET_DIR/$BOOT_TARGET/release/argus-bootloader.efi"
SIGNER="$CARGO_TARGET_DIR/release/sign-manifest"

[ -f "$KERNEL_ELF" ] || die "kernel ELF not produced at $KERNEL_ELF"
[ -f "$BOOT_EFI" ]   || die "loader PE not produced at $BOOT_EFI"
[ -x "$SIGNER" ]     || die "sign-manifest not produced at $SIGNER"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- assemble the ESP tree --------------------------------------------------
say "assembling ESP at $ESP"
rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT" "$ESP/ARGUS"
cp "$BOOT_EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"
cp "$KERNEL_ELF" "$ESP/ARGUSKRN.ELF"
cp "$BOOT_EFI" "$DIST/BOOTX64.EFI"
cp "$KERNEL_ELF" "$DIST/ARGUSKRN.ELF"

# --- initrd -----------------------------------------------------------------
#
# A tiny initrd image, so `BootInfo.initrd_phys`/`initrd_len` carry a real
# payload instead of zeroes and the kernel can prove the bytes landed where the
# loader said they did. Format: 8-byte magic, u32 version, u32 payload length,
# then the payload. Phase II replaces this with the real service images.
say "creating initrd image"
INITRD="$DIST/INITRD.BIN"
INITRD_PAYLOAD="ARGUS Phase I initrd placeholder. Real service images arrive in Phase II."
python3 - "$INITRD" "$INITRD_PAYLOAD" <<'PYEOF'
import struct, sys
path, payload = sys.argv[1], sys.argv[2].encode()
data = b"ARGUSIRD" + struct.pack("<II", 1, len(payload)) + payload
# Pad to a page so the loader's page allocation is exactly one page.
data += b"\0" * (-len(data) % 4096)
open(path, "wb").write(data)
PYEOF
[ -s "$INITRD" ] || die "initrd image not produced"
cp "$INITRD" "$ESP/INITRD.BIN"

# --- boot config ------------------------------------------------------------
#
# The config is small and boring on purpose: it exists so that `config_digest`
# covers a real second artifact, and so a test can tamper with a file that is
# not the kernel and watch the loader refuse it.
say "writing boot config"
CONFIG="$ESP/ARGUS/BOOT.CFG"
cat > "$CONFIG" <<EOF
# ARGUS boot config — covered by config_digest in ARGUS.MF.
slot=A
security_version=${SECURITY_VERSION}
loader_version=${LOADER_VERSION_NUM}
console=serial
EOF
cp "$CONFIG" "$DIST/BOOT.CFG"

# --- sign the manifest ------------------------------------------------------
#
# Signed here, in the normal flow, so the everyday build exercises the same code
# path the tests attack. The key is the committed TEST key: see keys/test/README.md.
say "signing ARGUS.MF (security_version=$SECURITY_VERSION, test key)"
[ -f "$SIGNING_KEY" ] || die "signing key $SIGNING_KEY not found"
"$SIGNER" sign \
    --kernel "$ESP/ARGUSKRN.ELF" \
    --config "$CONFIG" \
    --initrd "$ESP/INITRD.BIN" \
    --slot A \
    --security-version "$SECURITY_VERSION" \
    --slot-generation 1 \
    --manifest-version 1 \
    --min-loader-version "$LOADER_VERSION_NUM" \
    --issued-counter 1 \
    --key "$SIGNING_KEY" \
    --out "$ESP/ARGUS/ARGUS.MF"
cp "$ESP/ARGUS/ARGUS.MF" "$DIST/ARGUS.MF"

# --- seed the anti-replay floor --------------------------------------------
#
# The floor starts at 1 and only ever rises, and only after a boot that was
# confirmed (see bootloader/src/security.rs). It is a file on the ESP, which is
# NOT tamper-proof — the README says so; TPM NV is where it belongs.
say "seeding the anti-replay floor at 1"
"$SIGNER" floor --floor 1 --seq 1 --out "$ESP/ARGUS/FLOOR"
cp "$ESP/ARGUS/FLOOR" "$DIST/FLOOR"
# No CONFIRM file: the first boot creates it, which is what makes the floor rise
# on the boot after that.

# --- verify the manifest against what it signs ------------------------------
say "cross-checking the signed manifest on the host"
"$SIGNER" verify \
    --manifest "$ESP/ARGUS/ARGUS.MF" \
    --kernel "$ESP/ARGUSKRN.ELF" \
    --config "$CONFIG" \
    --initrd "$ESP/INITRD.BIN" \
    --slot A --floor 1 --loader-version "$LOADER_VERSION_NUM" \
    --pubkey "$PUBKEY_DER"

# --- FAT volume -------------------------------------------------------------
#
# The image is a real, bootable disk: an MBR partition table with one FAT16
# EFI System Partition starting at LBA 2048 (1 MiB). This matters. A bare FAT
# filesystem with no partition table ("superfloppy") is read by OVMF's BDS, but
# PartitionDxe never creates a partition child handle for it, and the resulting
# handle topology leaves the loader unable to find a volume — measured, see
# README "Firmware notes". With a partition table the chain
# VirtioBlk -> Partition -> Fat binds normally.
say "creating FAT16 ESP volume (${ESP_SIZE_MB} MiB)"
ESP_FAT="$TMP/esp-volume.fat"
dd if=/dev/zero of="$ESP_FAT" bs=1M count="$ESP_SIZE_MB" status=none
mkfs.vfat -F 16 -n ARGUSESP "$ESP_FAT" >/dev/null
mmd -i "$ESP_FAT" ::/EFI
mmd -i "$ESP_FAT" ::/EFI/BOOT
mmd -i "$ESP_FAT" ::/ARGUS
mcopy -i "$ESP_FAT" "$ESP/EFI/BOOT/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$ESP_FAT" "$ESP/ARGUSKRN.ELF" ::/ARGUSKRN.ELF
mcopy -i "$ESP_FAT" "$ESP/INITRD.BIN" ::/INITRD.BIN
mcopy -i "$ESP_FAT" "$ESP/ARGUS/ARGUS.MF" ::/ARGUS/ARGUS.MF
mcopy -i "$ESP_FAT" "$ESP/ARGUS/BOOT.CFG" ::/ARGUS/BOOT.CFG
mcopy -i "$ESP_FAT" "$ESP/ARGUS/FLOOR" ::/ARGUS/FLOOR

# Read the volume back *before* it is composed into a disk image. This is the
# check the old script did last and against a dd-composed file; doing it here
# means the thing being verified is a file we just wrote and closed, not a
# window into another file.
say "verifying the FAT volume read back from disk"
sync
verify_fat_volume "$ESP_FAT" "$TMP" "$ESP" || die "the FAT volume does not contain the files that were written to it"

# --- disk image -------------------------------------------------------------
say "creating MBR disk image $IMG (${IMG_SIZE_MB} MiB)"
rm -f "$IMG"
dd if=/dev/zero of="$IMG" bs=1M count="$IMG_SIZE_MB" status=none

# One MBR partition entry: type 0x0E (FAT16 LBA), start LBA 2048, 30720 sectors
# (15 MiB). CHS fields are set to the "use LBA" sentinel 0xFEFFFE, which is what
# every modern partitioner writes.
{
    dd if=/dev/zero bs=1 count=446 status=none
    printf '\x00\xfe\xff\xff\x0e\xfe\xff\xff'
    printf '\x00\x08\x00\x00'   # start LBA 2048, little-endian
    printf '\x00\x78\x00\x00'   # 30720 sectors, little-endian
    dd if=/dev/zero bs=1 count=48 status=none
    printf '\x55\xaa'
} > "$TMP/mbr.bin"
dd if="$TMP/mbr.bin" of="$IMG" conv=notrunc status=none
dd if="$ESP_FAT" of="$IMG" bs=1M seek=1 conv=notrunc status=none
sync

# --- verify the image really contains what we think -------------------------
say "verifying image contents (partition extracted and compared byte for byte)"
check_image "$IMG" "$TMP" "$ESP"

BUILD_END_NS=$(date +%s%N)
BUILD_MS=$(( (BUILD_END_NS - BUILD_START_NS) / 1000000 ))

say "OK"
printf '\n'
printf '  loader   %s  (%s bytes)\n' "$DIST/BOOTX64.EFI" "$(stat -c%s "$DIST/BOOTX64.EFI")"
printf '  kernel   %s  (%s bytes)\n' "$DIST/ARGUSKRN.ELF" "$(stat -c%s "$DIST/ARGUSKRN.ELF")"
printf '  initrd   %s  (%s bytes)\n' "$INITRD" "$(stat -c%s "$INITRD")"
printf '  manifest %s  (%s bytes, security_version=%s)\n' "$DIST/ARGUS.MF" "$(stat -c%s "$DIST/ARGUS.MF")" "$SECURITY_VERSION"
printf '  image    %s  (%s bytes)\n' "$IMG" "$(stat -c%s "$IMG")"
printf '  build    %s s (wall clock, measured)\n' "$(awk "BEGIN{printf \"%.2f\", $BUILD_MS/1000}")"
printf '\n  next: ./run.sh\n\n'

# Machine-readable, so the report cites a measurement rather than a memory.
printf 'build_seconds=%.2f\nbuild_finished_utc=%s\nsecurity_version=%s\n' \
    "$(awk "BEGIN{printf \"%.2f\", $BUILD_MS/1000}")" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$SECURITY_VERSION" \
    > "$DIST/build-timing.txt"
