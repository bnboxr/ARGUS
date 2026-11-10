#!/usr/bin/env bash
#
# ARGUS Phase I — boot the image under QEMU/OVMF, one command.
#
#   ./run.sh
#
# Boots dist/argus-disk.img on QEMU with the OVMF firmware, serial wired to
# stdio, no graphical window. The complete serial log is captured to
# artifacts/boot.log and also printed, with the wall-clock offset of every
# interesting line recorded to artifacts/timing.txt.
#
# THE RULE, stated because it has already misled one reader:
#   run.sh boots whatever is in dist/ RIGHT NOW. It does not build, and it does
#   not check that dist/ is up to date. If a build failed, the previous image is
#   still there and this script will happily boot it and exit 0 — correctly, for
#   the image it was given, and misleadingly for the change you just made.
#   Always: ./build.sh && ./run.sh, and look at build.sh's exit status first.
#
# The image is attached read-write on purpose: the loader writes \ARGUS\CONFIRM
# before handoff and may raise \ARGUS\FLOOR on a later boot. Booting a copy is
# what the tests do, so they never depend on the state a previous boot left.
#
# Exit status — this is the point of the script:
#   0   the kernel printed its own banner, verified the BootInfo checksum, and
#       reached its halt marker. The boot is proven, not assumed.
#   1   the kernel banner never appeared, the loader refused the image, or the
#       kernel refused BootInfo. Fails loudly; the log says where it stopped.
#   *   QEMU itself failed to start (bad firmware path, missing image) — that
#       status is passed through unchanged.
#
# Environment:
#   BOOT_TIMEOUT   seconds to let the VM run (default 12; the kernel reaches
#                  its halt marker at ~3.5 s, so this is pure margin)
#   OVMF_CODE      firmware code blob
#   OVMF_VARS      firmware variable store (copied before use, never modified)
#   QEMU           qemu binary (default qemu-system-x86_64)
#   QEMU_DRIVE_IF  QEMU -drive interface for the disk (default virtio; ide, sata,
#                  nvme and usb are the other values worth testing)
#   ARGUS_DIST     directory holding argus-disk.img (default ./dist). Used by
#                  the negative tests so they never touch the real image.
#   ARGUS_LOG      where to write the serial log (default artifacts/boot.log)

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

DIST="${ARGUS_DIST:-$ROOT/dist}"
IMG="$DIST/argus-disk.img"
ARTIFACTS="$ROOT/artifacts"
LOG="${ARGUS_LOG:-$ARTIFACTS/boot.log}"
TIMING="$ARTIFACTS/timing.txt"

QEMU="${QEMU:-qemu-system-x86_64}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-12}"
DRIVE_IF="${QEMU_DRIVE_IF:-virtio}"

OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"

say() { printf '\033[1m[run]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[run] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v "$QEMU" >/dev/null 2>&1 || die "$QEMU not found. Install qemu-system-x86."
[ -f "$IMG" ] || die "$IMG not found. Run ./build.sh first."
[ -f "$OVMF_CODE" ] || die "OVMF code blob not found at $OVMF_CODE (set OVMF_CODE=...)"
[ -f "$OVMF_VARS" ] || die "OVMF vars blob not found at $OVMF_VARS (set OVMF_VARS=...)"

mkdir -p "$ARTIFACTS" "$(dirname "$LOG")"

# OVMF writes to its variable store, so give it a throwaway copy each run.
# The system file is never touched, which keeps runs reproducible.
VARS_COPY="$ARTIFACTS/OVMF_VARS.run.fd"
cp "$OVMF_VARS" "$VARS_COPY"

say "firmware: $OVMF_CODE"
say "vars:     $OVMF_VARS -> $VARS_COPY"
say "image:    $IMG  (mtime $(date -u -r "$IMG" +%Y-%m-%dT%H:%M:%SZ), built, not rebuilt)"
if [ -f "$DIST/ARGUS.MF" ]; then
    say "manifest: $DIST/ARGUS.MF  ($(stat -c%s "$DIST/ARGUS.MF") bytes, signed on the host)"
fi
say "drive:    if=$DRIVE_IF"
say "log:      $LOG"
say "timeout:  ${BOOT_TIMEOUT}s"

QEMU_CMD=(
    "$QEMU"
    -machine q35
    -m 128M
    -cpu max
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,unit=1,file=$VARS_COPY"
    -drive "file=$IMG,format=raw,if=$DRIVE_IF"
    -serial stdio
    -display none
    -no-reboot
    -no-shutdown
)

# Print the exact command, so the README and the report cannot drift from it.
printf '\033[1m[run]\033[0m %s\n' "${QEMU_CMD[*]}"

# --- run, stamping every line as it arrives ---------------------------------
#
# Boot time is measured, not estimated: each serial line is timestamped against
# the moment QEMU was launched, so "time to kernel banner" is the offset of the
# line the kernel itself printed. A clock read after the fact would only tell us
# how long the timeout was.
MARKERS="$(mktemp)"
trap 'rm -f "$MARKERS"' EXIT

: > "$LOG"
START_NS=$(date +%s%N)

timeout --foreground "$BOOT_TIMEOUT" "${QEMU_CMD[@]}" 2>&1 | \
while IFS= read -r line; do
    printf '%s\n' "$line"
    printf '%s\n' "$line" >> "$LOG"
    now=$(date +%s%N)
    ms=$(( (now - START_NS) / 1000000 ))
    case "$line" in
        *"[ARGUS-LOADER] stage=banner event=start"*)
            printf 'loader_banner_ms=%s\n' "$ms" >> "$MARKERS" ;;
        *"stage=exit-boot-services result=EFI_SUCCESS"*)
            printf 'exit_boot_services_ms=%s\n' "$ms" >> "$MARKERS" ;;
        *"stage=handoff event=jump"*)
            printf 'handoff_ms=%s\n' "$ms" >> "$MARKERS" ;;
        *"[ARGUS-KERNEL] stage=entry event=banner"*)
            printf 'kernel_banner_ms=%s\n' "$ms" >> "$MARKERS" ;;
        *"[ARGUS-KERNEL] stage=halt event=boot_proven"*)
            printf 'kernel_halt_ms=%s\n' "$ms" >> "$MARKERS" ;;
    esac
done
QEMU_STATUS="${PIPESTATUS[0]}"

# 124 is the timeout firing, which is the *expected* outcome: the kernel halts
# in `hlt` forever by design, so QEMU only stops when we stop it.
if [ "$QEMU_STATUS" -ne 0 ] && [ "$QEMU_STATUS" -ne 124 ]; then
    printf '\n'
    die "QEMU exited with status $QEMU_STATUS — the VM did not boot. See $LOG"
fi

# --- verdict ----------------------------------------------------------------
missing=0
require() {
    if ! grep -qF "$1" "$LOG"; then
        printf '\033[1;31m[run] MISSING:\033[0m %s\n' "$1" >&2
        missing=1
    fi
}
forbid() {
    if grep -qF "$1" "$LOG"; then
        printf '\033[1;31m[run] FORBIDDEN:\033[0m %s\n' "$1" >&2
        missing=1
    fi
}
# require_one <label> <literal>... — at least one of the literals must appear.
#
# For a stage that has exactly one of several legitimate outcomes. The floor
# stage is the case that forced this: it ends in `event=stable` when there is
# nothing to raise, and in `event=raised` when the previous boot's confirmation
# marker lets it go up. Demanding only `stable` made the *second* boot of a good
# image fail `./run.sh` with a missing stage, even though the loader had done
# everything right — measured on 2026-11-08, see manifest-tests "floor rises".
# The two outcomes stay mutually exclusive in the loader, and which one is
# correct for a given boot is asserted by the test, not guessed here.
require_one() {
    local label="$1"; shift
    local lit
    for lit in "$@"; do
        if grep -qF "$lit" "$LOG"; then
            return 0
        fi
    done
    printf '\033[1;31m[run] MISSING:\033[0m %s (none of: %s)\n' "$label" "$*" >&2
    missing=1
}

# The security chain, in the order the loader enforces it. These are not
# decoration: each one is a check that has a matching negative test.
require "[ARGUS-LOADER] stage=volume event=found"
require "[ARGUS-LOADER] stage=manifest event=key"
require "[ARGUS-LOADER] stage=manifest event=signature result=VALID"
require "[ARGUS-LOADER] stage=manifest event=verified result=ACCEPT"
require "[ARGUS-LOADER] stage=floor event=enforced"
require "[ARGUS-LOADER] stage=confirm event="
require "[ARGUS-LOADER] stage=config result=MATCH"
require "[ARGUS-LOADER] stage=kernel-digest result=MATCH"
require "[ARGUS-LOADER] stage=initrd-digest result=MATCH"
require_one "[ARGUS-LOADER] stage=floor outcome" \
    "[ARGUS-LOADER] stage=floor event=stable" \
    "[ARGUS-LOADER] stage=floor event=raised"
require "[ARGUS-LOADER] stage=confirm event=pending"

# The ordered trace the loader is responsible for.
require "[ARGUS-LOADER] stage=banner event=start"
require "[ARGUS-LOADER] stage=memmap event=summary"
require "[ARGUS-LOADER] stage=acpi event="
require "[ARGUS-LOADER] stage=gop event="
require "[ARGUS-LOADER] stage=kernel-read event=ok"
require "[ARGUS-LOADER] stage=elf-validate result=ACCEPT"
require "[ARGUS-LOADER] stage=initrd event="
require "[ARGUS-LOADER] stage=bootinfo event=sealed"
require "[ARGUS-LOADER] stage=exit-boot-services result=EFI_SUCCESS"
require "[ARGUS-LOADER] stage=handoff event=jump"

# The kernel's own proof, which the loader cannot fake.
require "[ARGUS-KERNEL] stage=entry event=banner"
require "[ARGUS-KERNEL] stage=bootinfo event=checksum_ok"

# Every BootInfo v1 field group must be visible in the kernel's dump.
require "[ARGUS-KERNEL] stage=bootinfo field=abi_version"
require "[ARGUS-KERNEL] stage=bootinfo field=abi_size"
require "[ARGUS-KERNEL] stage=bootinfo field=flags"
require "[ARGUS-KERNEL] stage=bootinfo field=checksum"
require "[ARGUS-KERNEL] stage=bootinfo field=memory_map_meta"
require "[ARGUS-KERNEL] stage=bootinfo field=memory_regions[0]"
require "[ARGUS-KERNEL] stage=bootinfo field=acpi"
require "[ARGUS-KERNEL] stage=bootinfo field=framebuffer"
require "[ARGUS-KERNEL] stage=bootinfo field=initrd"
require "[ARGUS-KERNEL] stage=bootinfo field=kernel_phys"
require "[ARGUS-KERNEL] stage=bootinfo field=loader_reserved[0]"
require "[ARGUS-KERNEL] stage=bootinfo field=boot_slot"
require "[ARGUS-KERNEL] stage=bootinfo field=debug_serial"
require "[ARGUS-KERNEL] stage=halt event=boot_proven"

# A refusal anywhere in the security chain is a failed run, whatever else the
# log contains. This is the single line that makes a refused boot impossible to
# mistake for a good one.
forbid "result=REFUSED"

# A kernel that refused BootInfo must never be reported as a proven boot.
forbid "[ARGUS-KERNEL] stage=bootinfo event=fail"
forbid "[ARGUS-KERNEL] stage=halt event=refused"

# --- timing -----------------------------------------------------------------
g() { awk -F= -v k="$1" '$1==k{print $2}' "$MARKERS" | head -1; }
BUILD_S="$(awk -F= '$1=="build_seconds"{print $2}' "$DIST/build-timing.txt" 2>/dev/null || true)"
{
    printf 'build_seconds=%s\n' "${BUILD_S:-unmeasured}"
    printf 'qemu_to_loader_banner_ms=%s\n' "$(g loader_banner_ms)"
    printf 'qemu_to_exit_boot_services_ms=%s\n' "$(g exit_boot_services_ms)"
    printf 'qemu_to_handoff_ms=%s\n' "$(g handoff_ms)"
    printf 'qemu_to_kernel_banner_ms=%s\n' "$(g kernel_banner_ms)"
    printf 'qemu_to_kernel_halt_ms=%s\n' "$(g kernel_halt_ms)"
    printf 'drive_interface=%s\n' "$DRIVE_IF"
    printf 'firmware=%s\n' "$OVMF_CODE"
    printf 'run_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$TIMING"

printf '\n'
say "timing:"
sed 's/^/      /' "$TIMING"

printf '\n'
if [ "$missing" -ne 0 ]; then
    printf '\033[1;31m[run] FAIL:\033[0m the kernel banner, a BootInfo field, or a loader stage is missing from the serial log.\n' >&2
    printf '      The boot is NOT proven. Full log: %s\n' "$LOG" >&2
    exit 1
fi

printf '\033[1;32m[run] PASS:\033[0m loader handed control to the kernel; the kernel printed its banner,\n'
printf '      verified the BootInfo checksum and reached its halt marker.\n'
printf '      Serial log: %s\n' "$LOG"
exit 0
