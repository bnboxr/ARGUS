#!/usr/bin/env bash
#
# The security chain of Phase I slice 2a, attacked one case at a time.
#
# Every case copies the good image, changes exactly one thing, boots it with the
# real run.sh, and asserts three things:
#   * run.sh exits non-zero            (a refused boot is never "success")
#   * the serial log names the code    (E_MANIFEST_ABSENT, E_SIG_INVALID, ...)
#   * the kernel never printed its banner (the refusal happened before handoff)
#
# The last one is the point: a loader that refuses *after* handing control to the
# kernel has already lost.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tests/lib.sh"

SIGN="${CARGO_TARGET_DIR:-$ROOT/target}/release/sign-manifest"
[ -x "$SIGN" ] || { printf 'FAIL: %s not built; run ./build.sh first\n' "$SIGN"; exit 1; }

KERNEL="$ROOT/dist/esp/ARGUSKRN.ELF"
CONFIG="$ROOT/dist/esp/ARGUS/BOOT.CFG"
INITRD="$ROOT/dist/esp/INITRD.BIN"
KEY="$ROOT/keys/test/manifest-key.pem"
OTHER_KEY="$ROOT/keys/test/other-key.pem"

WORK="$WORK_ROOT/manifest"
rm -rf "$WORK"
mkdir -p "$WORK"

# case <name> <expected-code>
# Runs in a directory prepared by the caller: $WORK/<name>/dist/argus-disk.img
case_run() {
    local name="$1" code="$2" dir="$WORK/$1"
    local status
    status="$(boot "$dir" "$name")"
    printf '\n\033[1m-- %s\033[0m\n' "$name"
    expect_nonzero "$status" "$name"
    expect_contains "$dir/$name.log" "code=$code" "$name"
    expect_absent "$dir/$name.log" "[ARGUS-KERNEL] stage=entry event=banner" "$name (kernel never ran)"
    show_evidence "$dir/$name.log" "stage=(manifest|floor|kernel-digest|config|initrd-digest|handoff).*(REFUSED|enforcing|enforced)" "$name"
    drop_image "$dir"
}

# fresh <name> -> prepares a scratch image, echoes the dir
fresh() {
    local dir="$WORK/$1"
    new_image "$dir" >/dev/null
    printf '%s\n' "$dir"
}

img() { printf '%s/dist/argus-disk.img\n' "$1"; }

# resign <out> [extra sign args...] — sign the real kernel/config with the test key
resign() {
    local out="$1"; shift
    "$SIGN" sign --kernel "$KERNEL" --config "$CONFIG" --initrd "$INITRD" \
        --slot A --security-version 2 --slot-generation 1 --manifest-version 1 \
        --min-loader-version 3 --issued-counter 1 \
        --key "$KEY" --out "$out" "$@" >/dev/null 2>&1
}

printf '\033[1m[manifest-tests] %s\033[0m\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# --- 1. manifest absent -----------------------------------------------------
d="$(fresh absent)"
mdel -i "$(img "$d")@@1M" ::/ARGUS/ARGUS.MF
case_run absent E_MANIFEST_ABSENT

# --- 2. bad magic -----------------------------------------------------------
d="$(fresh badmagic)"
cp "$ROOT/dist/esp/ARGUS/ARGUS.MF" "$WORK/badmagic.mf"
printf 'X' | dd of="$WORK/badmagic.mf" bs=1 seek=0 conv=notrunc status=none
put_file "$(img "$d")" "$WORK/badmagic.mf" ::/ARGUS/ARGUS.MF
case_run badmagic E_MANIFEST_MAGIC

# --- 3. tampered payload_digest --------------------------------------------
# One bit flipped in the field that covers every other field. The signature is
# untouched and still valid over the *old* payload_digest, which is exactly why
# the loader checks the payload digest before it spends time on RSA.
d="$(fresh payloaddigest)"
cp "$ROOT/dist/esp/ARGUS/ARGUS.MF" "$WORK/tampered.mf"
python3 - "$WORK/tampered.mf" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, 'rb').read())
b[181] ^= 0x01          # first byte of payload_digest
open(p, 'wb').write(b)
PY
put_file "$(img "$d")" "$WORK/tampered.mf" ::/ARGUS/ARGUS.MF
case_run payloaddigest E_MANIFEST_DIGEST

# --- 4. signature by a different key ---------------------------------------
# The manifest claims the *real* key id but is signed by the other test key, so
# the key-id check passes and the signature check is what has to catch it.
d="$(fresh wrongkey)"
resign "$WORK/wrongkey.mf" --key "$OTHER_KEY" --key-id-from "$KEY"
put_file "$(img "$d")" "$WORK/wrongkey.mf" ::/ARGUS/ARGUS.MF
case_run wrongkey E_SIG_INVALID

# --- 5. unknown key id ------------------------------------------------------
d="$(fresh unknownkey)"
resign "$WORK/unknownkey.mf" --key "$OTHER_KEY"
put_file "$(img "$d")" "$WORK/unknownkey.mf" ::/ARGUS/ARGUS.MF
case_run unknownkey E_KEY_UNKNOWN

# --- 6. slot mismatch -------------------------------------------------------
# A manifest signed for slot B is structurally invalid for a slot-A boot,
# however valid its signature is.
d="$(fresh slotmismatch)"
resign "$WORK/slotb.mf" --slot B
put_file "$(img "$d")" "$WORK/slotb.mf" ::/ARGUS/ARGUS.MF
case_run slotmismatch E_SLOT_MISMATCH

# --- 7. security_version below the floor -----------------------------------
d="$(fresh rollback)"
resign "$WORK/old.mf" --security-version 1
put_file "$(img "$d")" "$WORK/old.mf" ::/ARGUS/ARGUS.MF
"$SIGN" floor --floor 5 --seq 2 --out "$WORK/floor5.bin" >/dev/null
put_file "$(img "$d")" "$WORK/floor5.bin" ::/ARGUS/FLOOR
case_run rollback E_MANIFEST_ROLLBACK

# --- 8. kernel_digest mismatch ---------------------------------------------
# The kernel is signed, then one byte in the middle of it is flipped. The
# manifest is untouched, so this is the case the digest exists for.
d="$(fresh kerneldigest)"
cp "$KERNEL" "$WORK/kernel-flipped.elf"
python3 - "$WORK/kernel-flipped.elf" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, 'rb').read())
b[len(b) // 2] ^= 0x01
open(p, 'wb').write(b)
PY
put_file "$(img "$d")" "$WORK/kernel-flipped.elf" ::/ARGUSKRN.ELF
case_run kerneldigest E_KERNEL_DIGEST

# --- 9. min_loader_version too high ----------------------------------------
d="$(fresh loaderold)"
resign "$WORK/future.mf" --min-loader-version 99
put_file "$(img "$d")" "$WORK/future.mf" ::/ARGUS/ARGUS.MF
case_run loaderold E_LOADER_TOO_OLD

# --- 10. config tampered ----------------------------------------------------
d="$(fresh configdigest)"
cp "$CONFIG" "$WORK/boot-tampered.cfg"
printf 'slot=B\n' >> "$WORK/boot-tampered.cfg"
put_file "$(img "$d")" "$WORK/boot-tampered.cfg" ::/ARGUS/BOOT.CFG
case_run configdigest E_CONFIG_DIGEST

# --- 11. a corrupt floor refuses rather than guessing -----------------------
d="$(fresh floorcorrupt)"
python3 - "$WORK/badfloor.bin" <<'PY'
import sys
# Our magic, bad CRC in both copies: the file exists but its value is unknown.
b = bytearray(4096)
b[0:8] = b"ARGFLR01"
b[2048:2056] = b"ARGFLR01"
open(sys.argv[1], 'wb').write(b)
PY
put_file "$(img "$d")" "$WORK/badfloor.bin" ::/ARGUS/FLOOR
case_run floorcorrupt E_FLOOR_CORRUPT

# --- 12. the floor rises after a confirmed boot ----------------------------
# Two boots of the same image: the first writes \ARGUS\CONFIRM, the second
# reads it and raises \ARGUS\FLOOR. This is the anti-replay mechanism doing its
# job, not a simulation of it.
printf '\n\033[1m-- floor rises after a confirmed boot\033[0m\n'
d="$WORK/floorrise"
new_image "$d" >/dev/null
status1="$(boot "$d" first)"
status2="$(boot "$d" second)"
if [ "$status1" = "0" ] && [ "$status2" = "0" ]; then
    pass "two consecutive boots of the same image both exit 0"
else
    fail "boot 1 exited $status1, boot 2 exited $status2"
fi
expect_contains "$d/first.log" "stage=floor event=stable floor=1" "first boot: nothing to raise yet"
expect_contains "$d/first.log" "stage=confirm event=pending seq=1" "first boot writes the confirmation marker"
expect_contains "$d/second.log" "stage=confirm event=found" "second boot finds the marker"
expect_contains "$d/second.log" "stage=floor event=raised from=1 to=2" "second boot raises the floor 1 -> 2"
show_evidence "$d/second.log" "stage=(floor|confirm) event=" "floor rise"
drop_image "$d"

summary "manifest-tests"
