#!/usr/bin/env bash
# Negative tests: each corrupted kernel must be rejected with its own code and
# the loader must NOT jump.
#
# STATUS: written, NOT yet run to completion — see README "STATUS". It is kept
# so the next session does not have to re-derive the corruption offsets.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
KERNEL="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-unknown-none/release/argus-kernel"
[ -f "$KERNEL" ] || { echo "build first: ./build.sh"; exit 2; }
WORK="$ROOT/artifacts/negative"; rm -rf "$WORK"; mkdir -p "$WORK"
python3 tests/corrupt.py "$KERNEL" "$WORK"

fail=0
run_case() { # file expected_code
    local f="$1" code="$2"
    local img="$WORK/case.img"
    cp dist/argus-disk.img "$img"
    MTOOLS_SKIP_CHECK=1 mcopy -o -i "$img@@1M" "$f" ::/ARGUSKRN.ELF
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$WORK/v.fd"
    timeout 20 qemu-system-x86_64 -machine q35 -m 128M -cpu max \
      -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
      -drive if=pflash,format=raw,unit=1,file="$WORK/v.fd" \
      -drive "file=$img,format=raw,if=virtio" \
      -serial file:"$WORK/$code.log" -display none -no-reboot >/dev/null 2>&1
    if ! grep -q "code=$code" "$WORK/$code.log"; then
        echo "FAIL $code: expected rejection code not found"; fail=1; return
    fi
    if grep -q "ARGUS-KERNEL.*event=banner" "$WORK/$code.log"; then
        echo "FAIL $code: kernel ran on a rejected image"; fail=1; return
    fi
    echo "PASS $code"
}
run_case "$WORK/bad-magic.elf"        E_MAGIC
run_case "$WORK/truncated-phdr.elf"   E_PHDR_TRUNCATED
run_case "$WORK/vaddr-unusable.elf"   E_VADDR_UNUSABLE
exit $fail
