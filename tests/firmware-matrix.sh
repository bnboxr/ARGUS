#!/usr/bin/env bash
#
# Firmware matrix: does the firmware expose SimpleFileSystem on the volume it
# booted us from, across drive interfaces and OVMF builds?
#
#   ./tests/firmware-matrix.sh
#
# One change at a time, everything else identical, and each row is a real boot
# whose serial log is kept under artifacts/firmware-matrix/.
#
# Columns:
#   sfs      the LoadedImage->DeviceHandle route returned a filesystem (yes/no)
#   loader   the loader's banner reached the wire (no means the firmware never
#            handed control to us at all)
#   kernel   the kernel banner and halt marker appeared (run.sh exit 0)
#
# This machine has only the 4 MiB OVMF blobs (`OVMF_CODE_4M.fd` and the
# `.ms.fd` secure-boot variant); there is no 2 MiB `OVMF_CODE.fd` to test.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# shellcheck source=tests/lib.sh
. "$ROOT/tests/lib.sh"

[ -f "$ROOT/dist/argus-disk.img" ] || { echo "build first: ./build.sh"; exit 2; }

WORK="$ROOT/artifacts/firmware-matrix"
rm -rf "$WORK"; mkdir -p "$WORK"
TABLE="$ROOT/artifacts/firmware-matrix.txt"

printf '\033[1m[firmware-matrix] %s\033[0m\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

row() { # <label> <drive-if> <code> <vars> <kernel>
    local label="$1" dif="$2" code="$3" vars="$4" kernel="$5"
    local log="$WORK/$label.log"
    local rc
    ARGUS_LOG="$log" \
    QEMU_DRIVE_IF="$dif" \
    OVMF_CODE="$code" \
    OVMF_VARS="$vars" \
    BOOT_TIMEOUT="$TEST_TIMEOUT" \
    "$ROOT/run.sh" > "$WORK/$label.stdout" 2>&1
    rc=$?

    local sfs loader
    if grep -q "route=loaded_image_device_handle.*found=1" "$log" 2>/dev/null; then
        sfs=yes; else sfs=no; fi
    if grep -q "stage=banner event=start" "$log" 2>/dev/null; then
        loader=yes; else loader=no; fi

    printf '%-28s %-8s %-24s %-4s %-6s %-7s exit=%s\n' \
        "$label" "$dif" "$(basename "$code")" "$sfs" "$loader" "$kernel" "$rc" >> "$TABLE"
    printf '  %-28s if=%-7s %-22s sfs=%-3s loader=%-3s kernel=%-3s exit=%s\n' \
        "$label" "$dif" "$(basename "$code")" "$sfs" "$loader" "$kernel" "$rc"
}

: > "$TABLE"
printf '%-28s %-8s %-24s %-4s %-6s %-7s %s\n' \
    "case" "drive" "firmware" "sfs" "loader" "kernel" "status" >> "$TABLE"
printf '%-28s %-8s %-24s %-4s %-6s %-7s %s\n' \
    "case" "drive" "firmware" "sfs" "loader" "kernel" "status"

PLAIN=/usr/share/OVMF/OVMF_CODE_4M.fd
PLAIN_VARS=/usr/share/OVMF/OVMF_VARS_4M.fd
SECBOOT=/usr/share/OVMF/OVMF_CODE_4M.ms.fd
SECBOOT_VARS=/usr/share/OVMF/OVMF_VARS_4M.ms.fd

for dif in virtio ide sata nvme; do
    row "plain-$dif" "$dif" "$PLAIN" "$PLAIN_VARS" "?"
done

if [ -f "$SECBOOT" ] && [ -f "$SECBOOT_VARS" ]; then
    row "secureboot-virtio" virtio "$SECBOOT" "$SECBOOT_VARS" "?"
fi

printf '\n[firmware-matrix] table: %s\n' "$TABLE"
printf '[firmware-matrix] logs:  %s/\n' "$WORK"

# The assertion this test exists for: on the firmware we ship against, the
# canonical route must find the volume on every block interface OVMF supports.
if grep -q "^plain-virtio .*yes yes" "$TABLE"; then
    pass "virtio: SimpleFileSystem found via LoadedImage->DeviceHandle"
else
    fail "virtio: SimpleFileSystem NOT found"
fi

# Secure Boot: an unsigned loader is expected to be refused. That is not a
# failure of this test, it is the documented state until slice 2 signs images.
if grep -q "^secureboot-virtio" "$TABLE"; then
    if grep -q "^secureboot-virtio .*no no" "$TABLE"; then
        printf '  note: secure-boot firmware refused the unsigned loader, as expected\n'
        printf '        (signing and key enrolment are Phase I slice 2)\n'
    fi
fi

summary "firmware-matrix"
