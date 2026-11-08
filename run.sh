#!/usr/bin/env bash
#
# ARGUS Phase I — boot the image under QEMU/OVMF, one command.
#
#   ./run.sh
#
# Boots dist/argus-disk.img on QEMU with the OVMF firmware, serial wired to
# stdio, no graphical window. The complete serial log is captured to
# artifacts/boot.log and also printed.
#
# Exit status — this is the point of the script:
#   0   the kernel printed its own banner and reached its halt marker.
#       The boot is proven, not assumed.
#   1   the kernel banner never appeared, or the loader refused the image.
#       Fails loudly; the log says where it stopped.
#   *   QEMU itself failed to start (bad firmware path, missing image) — that
#       status is passed through unchanged.
#
# Environment:
#   BOOT_TIMEOUT   seconds to let the VM run (default 25)
#   OVMF_CODE      firmware code blob
#   OVMF_VARS      firmware variable store (copied before use, never modified)
#   QEMU           qemu binary (default qemu-system-x86_64)

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

IMG="$ROOT/dist/argus-disk.img"
ARTIFACTS="$ROOT/artifacts"
LOG="$ARTIFACTS/boot.log"

QEMU="${QEMU:-qemu-system-x86_64}"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-25}"

OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"

say() { printf '\033[1m[run]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[run] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v "$QEMU" >/dev/null 2>&1 || die "$QEMU not found. Install qemu-system-x86."
[ -f "$IMG" ] || die "$IMG not found. Run ./build.sh first."
[ -f "$OVMF_CODE" ] || die "OVMF code blob not found at $OVMF_CODE (set OVMF_CODE=...)"
[ -f "$OVMF_VARS" ] || die "OVMF vars blob not found at $OVMF_VARS (set OVMF_VARS=...)"

mkdir -p "$ARTIFACTS"

# OVMF writes to its variable store, so give it a throwaway copy each run.
# The system file is never touched, which keeps runs reproducible.
VARS_COPY="$ARTIFACTS/OVMF_VARS.run.fd"
cp "$OVMF_VARS" "$VARS_COPY"

say "firmware: $OVMF_CODE"
say "vars:     $OVMF_VARS -> $VARS_COPY"
say "image:    $IMG"
say "log:      $LOG"
say "timeout:  ${BOOT_TIMEOUT}s"

QEMU_CMD=(
    "$QEMU"
    -machine q35
    -m 128M
    -cpu max
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE"
    -drive "if=pflash,format=raw,unit=1,file=$VARS_COPY"
    -drive "file=$IMG,format=raw,if=virtio"
    -serial stdio
    -display none
    -no-reboot
    -no-shutdown
)

# Print the exact command, so the README and the report cannot drift from it.
printf '\033[1m[run]\033[0m %s\n' "${QEMU_CMD[*]}"

: > "$LOG"
timeout --foreground "$BOOT_TIMEOUT" "${QEMU_CMD[@]}" 2>&1 | tee -a "$LOG"
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

require "[ARGUS-LOADER] stage=banner event=start"
require "[ARGUS-LOADER] stage=memmap event=summary"
require "[ARGUS-LOADER] stage=elf-validate result=ACCEPT"
require "[ARGUS-LOADER] stage=exit-boot-services result=EFI_SUCCESS"
require "[ARGUS-LOADER] stage=handoff event=jump"
require "[ARGUS-KERNEL] stage=entry event=banner"
require "[ARGUS-KERNEL] stage=halt event=boot_proven"

printf '\n'
if [ "$missing" -ne 0 ]; then
    printf '\033[1;31m[run] FAIL:\033[0m the kernel banner or a loader stage is missing from the serial log.\n' >&2
    printf '      The boot is NOT proven. Full log: %s\n' "$LOG" >&2
    exit 1
fi

printf '\033[1;32m[run] PASS:\033[0m loader handed control to the kernel and the kernel printed its banner.\n'
printf '      Serial log: %s\n' "$LOG"
exit 0
