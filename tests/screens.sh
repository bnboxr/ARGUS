#!/usr/bin/env bash
#
# tests/screens.sh — prove the on-screen boot output, in pictures.
#
#   ./tests/screens.sh
#
# Boots QEMU three times with a monitor socket attached, waits for a marker in
# the serial log, and asks QEMU for a `screendump`. The PNGs land in
# artifacts/screens/ and are the evidence that a boot is visible without a
# serial port:
#
#   boot-1920x1080.png            a successful boot: loader stages + kernel alive
#   boot-640x480.png              the same, on a surface that only offers 640x480
#   refusal-missing-manifest-1920x1080.png
#                                 \ARGUS\ARGUS.MF deleted: E_MANIFEST_ABSENT, big
#
# The resolution in every file name is the one the loader reported on serial for
# that boot, and the test fails if the PNG header disagrees with it. Nothing is
# ever named after the mode the test *asked* for.
#
# The 640x480 case asks for the small mode through QEMU's fw_cfg
# (`-fw_cfg name=opt/argus/screen,string=640x480`) because QEMU's default VGA
# offers every mode up to 2560x1600 and the loader would otherwise pick 1920x1080
# on all three boots. The image itself is untouched: same bytes, different
# machine, exactly like a laptop with a 640x480 panel.
#
# QEMU 8.2 writes a binary PPM for `screendump` here even when the filename ends
# in .png, so the PPM is converted to a real PNG with python's zlib. That keeps
# the artifact small (a few tens of KB instead of 6 MB) and viewable anywhere.
#
# Each case boots its own copy of the image under /opt (not /home, which is a
# small volume) and never touches dist/.
#
# Exit status: 0 when all three PNGs exist and each is the size its case asked
# for; 1 otherwise.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="$ROOT/artifacts/screens"
WORK="${ARGUS_SCREEN_WORK:-/opt/argus-test/screens}"

QEMU="${QEMU:-qemu-system-x86_64}"
OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"
IMG="$ROOT/dist/argus-disk.img"

say() { printf '\033[1m[screens]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[screens] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v "$QEMU" >/dev/null 2>&1 || die "$QEMU not found"
[ -f "$IMG" ] || die "$IMG not found — run ./build.sh first"
[ -f "$OVMF_CODE" ] || die "OVMF code blob not found at $OVMF_CODE"
[ -f "$OVMF_VARS" ] || die "OVMF vars blob not found at $OVMF_VARS"

mkdir -p "$OUT" "$WORK"
# A PNG from an older naming scheme would sit next to the honest ones and be
# read as current evidence, so the artifacts directory starts empty of them.
rm -f "$OUT"/boot-*.png "$OUT"/refusal-*.png
FAILED=0
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; FAILED=1; }
pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }

# new_image <dir> — a fresh copy of the boot disk, boot state reset.
#
# The floor and the confirmation marker are state a previous boot leaves behind.
# A screenshot case must start from the image as built, or it inherits a floor of
# 2 and refuses for the wrong reason.
new_image() {
    local dir="$1"
    mkdir -p "$dir/dist"
    cp "$IMG" "$dir/dist/argus-disk.img"
    cp "$ROOT/dist/build-timing.txt" "$dir/dist/" 2>/dev/null || true
    MTOOLS_SKIP_CHECK=1 mdel -i "$dir/dist/argus-disk.img@@1M" ::/ARGUS/CONFIRM 2>/dev/null || true
    cp "$ROOT/dist/esp/ARGUS/FLOOR" "$dir/dist/floor.seed"
    MTOOLS_SKIP_CHECK=1 mcopy -o -i "$dir/dist/argus-disk.img@@1M" "$dir/dist/floor.seed" ::/ARGUS/FLOOR
    printf '%s\n' "$dir/dist/argus-disk.img"
}

# ppm_to_png <in.ppm> <out.png>
ppm_to_png() {
    python3 - "$1" "$2" <<'PY'
import struct, sys, zlib

raw, out = sys.argv[1], sys.argv[2]
data = open(raw, "rb").read()
if not data.startswith(b"P6"):
    sys.exit("not a P6 PPM: %r" % data[:8])
parts = data.split(b"\n", 3)
width, height = (int(x) for x in parts[1].split())
pixels = parts[3]
if len(pixels) != width * height * 3:
    sys.exit("short PPM: %d bytes for %dx%d" % (len(pixels), width, height))

rows = bytearray()
stride = width * 3
for y in range(height):
    rows.append(0)                      # PNG filter type 0 (none)
    rows += pixels[y * stride:(y + 1) * stride]

def chunk(kind, payload):
    return (struct.pack(">I", len(payload)) + kind + payload
            + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF))

png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
       + chunk(b"IEND", b""))
open(out, "wb").write(png)
print("%dx%d" % (width, height))
PY
}

# capture <name> <marker> <expected WxH> [extra qemu args...]
#
# Boot, wait for <marker> in the serial log, screendump through the monitor, then
# quit QEMU. If <name>'s image was already prepared by the caller, use it as is.
capture() {
    local case="$1" base="$2" marker="$3" want="$4"
    shift 4
    local dir="$WORK/$case"
    local img
    if [ -f "$dir/dist/argus-disk.img" ]; then
        mkdir -p "$dir"
        img="$dir/dist/argus-disk.img"
    else
        rm -rf "$dir"
        mkdir -p "$dir"
        img="$(new_image "$dir")"
    fi
    local sock="$dir/monitor.sock"
    local log="$dir/serial.log"
    rm -f "$sock" "$log"
    cp "$OVMF_VARS" "$dir/vars.fd"

    say "booting $case (waiting for \"$marker\")"
    "$QEMU" \
        -machine q35 -m 128M -cpu max \
        -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,unit=1,file=$dir/vars.fd" \
        -drive "file=$img,format=raw,if=virtio" \
        -serial "file:$log" \
        -monitor "unix:$sock,server=on,wait=off" \
        -display none -no-reboot -no-shutdown \
        "$@" >"$dir/qemu.out" 2>&1 &
    local qpid=$!

    # Wait for the marker, then let the picture settle so the last line drawn is
    # on the panel before it is photographed.
    local waited=0
    while [ "$waited" -lt 250 ]; do
        if [ -f "$log" ] && grep -qF "$marker" "$log" 2>/dev/null; then
            break
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
    if ! grep -qF "$marker" "$log" 2>/dev/null; then
        fail "$case: marker \"$marker\" never appeared; see $log"
        kill "$qpid" 2>/dev/null
        wait "$qpid" 2>/dev/null
        return 1
    fi
    sleep 0.8

    # The name of the file comes from the mode the loader says it drew into, not
    # from what the test asked for. A screenshot labelled with a resolution the
    # image does not have is worse than no screenshot, so the label is read back
    # out of the serial log and then checked against the PNG header below.
    local reported stride
    reported="$(grep -oE '^\[ARGUS-LOADER\] stage=screen result=READY width=[0-9]+ height=[0-9]+ stride=[0-9]+' "$log" \
        | tail -1 | sed -E 's/.*width=([0-9]+) height=([0-9]+).*/\1x\2/')"
    stride="$(grep -oE '^\[ARGUS-LOADER\] stage=screen result=READY width=[0-9]+ height=[0-9]+ stride=[0-9]+' "$log" \
        | tail -1 | sed -E 's/.*stride=([0-9]+)/\1/')"
    if [ -z "$reported" ]; then
        fail "$case: the loader never reported a screen mode; see $log"
        kill "$qpid" 2>/dev/null
        wait "$qpid" 2>/dev/null
        return 1
    fi
    if [ "$want" != "any" ] && [ "$reported" != "$want" ]; then
        fail "$case: asked the firmware for $want, the loader drew into $reported"
        printf '        the mode request was not honoured; nothing was captured, because a\n'
        printf '        %s file would be a lie\n' "$want"
        kill "$qpid" 2>/dev/null
        wait "$qpid" 2>/dev/null
        return 1
    fi

    local png="$OUT/$base-$reported.png"
    local ppm="${png%.png}.ppm"
    rm -f "$png" "$ppm"

    python3 - "$sock" "$ppm" <<'PY'
import socket, sys, time
sock_path, out = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sock_path)
time.sleep(0.2)
try:
    s.recv(65536)                       # the monitor's greeting
except socket.timeout:
    pass
s.sendall(("screendump %s\n" % out).encode())
time.sleep(1.0)
try:
    s.recv(65536)
except socket.timeout:
    pass
s.sendall(b"quit\n")
time.sleep(0.2)
s.close()
PY

    kill "$qpid" 2>/dev/null
    wait "$qpid" 2>/dev/null

    [ -s "$ppm" ] || { fail "$case: no screendump written"; return 1; }
    local got
    got="$(ppm_to_png "$ppm" "$png")" || { fail "$case: could not convert the screendump"; return 1; }
    rm -f "$ppm"
    if [ "$got" = "$reported" ]; then
        pass "$(basename "$png") is $got, the mode the loader reported (stride $stride)"
    else
        fail "$(basename "$png") is $got but the loader reported $reported — the file name would be a lie"
    fi
    printf '      from the serial log:\n'
    grep -E "stage=gop event=(selected|hint)|stage=screen result" "$log" | sed 's/^/        /' | head -4
    rm -f "$dir/dist/argus-disk.img"
}

# --- 1. successful boot at the firmware's own preferred mode ----------------
capture boot-default boot "stage=halt event=boot_proven" any

# --- 2. the same boot, forced down to 640x480 through QEMU's fw_cfg --------
# The loader is asked for a small mode; if the firmware will not give it, the
# case fails instead of writing a file with the wrong resolution in its name.
capture boot-low boot "stage=halt event=boot_proven" 640x480 \
    -fw_cfg name=opt/argus/screen,string=640x480

# --- 3. refusal: no manifest ------------------------------------------------
# Delete \ARGUS\ARGUS.MF from a copy. The loader must refuse with a named code,
# draw it large and centred, and leave it on screen.
REFUSAL_DIR="$WORK/refusal-missing-manifest"
rm -rf "$REFUSAL_DIR"
mkdir -p "$REFUSAL_DIR"
REFUSAL_IMG="$(new_image "$REFUSAL_DIR")"
MTOOLS_SKIP_CHECK=1 mdel -i "$REFUSAL_IMG@@1M" ::/ARGUS/ARGUS.MF || die "could not delete the manifest"
capture refusal-missing-manifest refusal-missing-manifest "code=E_MANIFEST_ABSENT" any

printf '\n'
if [ "$FAILED" -ne 0 ]; then
    printf '\033[1;31m[screens] FAILED\033[0m — see the logs above\n' >&2
    exit 1
fi
printf '\033[1;32m[screens] PASSED\033[0m — three PNGs in %s\n' "$OUT"
ls -l "$OUT"/*.png | sed 's/^/      /'
