#!/usr/bin/env bash
#
# tests/bootmenu.sh — photograph the boot manager screen, in QEMU.
#
#   ./tests/bootmenu.sh
#
# Boots the built image with `\ARGUS\BOOTMENU.CFG` added to the ESP, waits for
# the loader to say it painted the menu, and asks QEMU for a `screendump` while
# the loader is still holding the screen (the menu's own 2000 ms timeout). The
# PNG lands in artifacts/screens/bootmenu-1920x1080.png.
#
# This is a separate script on purpose: it does not touch tests/screens.sh, and
# the boot it photographs is the same proven boot — the same manifest chain, the
# same BootInfo, the same handoff to the kernel. The only difference is the file
# on the ESP, which selects the second screen.
#
# The resolution in the file name is read back out of the loader's own serial
# line and then checked against the PNG's IHDR, so a file whose name says
# 1920x1080 can only exist if its bytes say 1920x1080.
#
# Exit status: 0 when the menu was painted, captured and is 1920x1080.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="$ROOT/artifacts/screens"
WORK="${ARGUS_SCREEN_WORK:-/opt/argus-test/bootmenu}"

QEMU="${QEMU:-qemu-system-x86_64}"
OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"
IMG="$ROOT/dist/argus-disk.img"

say() { printf '\033[1m[bootmenu]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[bootmenu] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }
pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*" >&2; FAILED=1; }
FAILED=0

command -v "$QEMU" >/dev/null 2>&1 || die "$QEMU not found"
[ -f "$IMG" ] || die "$IMG not found — run ./build.sh first"
[ -f "$OVMF_CODE" ] || die "OVMF code blob not found at $OVMF_CODE"
[ -f "$OVMF_VARS" ] || die "OVMF vars blob not found at $OVMF_VARS"

mkdir -p "$OUT" "$WORK"
rm -f "$OUT"/bootmenu-*.png

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

png_ihdr() {
    python3 - "$1" <<'PY'
import struct, sys
data = open(sys.argv[1], "rb").read()
if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
    sys.exit("no IHDR at offset 12")
width, height = struct.unpack(">II", data[16:24])
print("%dx%d" % (width, height))
PY
}

# --- the image: the built one, plus the one file that selects the menu -------
DIR="$WORK/case"
rm -rf "$DIR"
mkdir -p "$DIR/dist"
cp "$IMG" "$DIR/dist/argus-disk.img"
ESP="$DIR/dist/argus-disk.img@@1M"
# Boot state a previous run left behind would refuse for the wrong reason.
MTOOLS_SKIP_CHECK=1 mdel -i "$ESP" ::/ARGUS/CONFIRM 2>/dev/null || true
cp "$ROOT/dist/esp/ARGUS/FLOOR" "$DIR/dist/floor.seed"
MTOOLS_SKIP_CHECK=1 mcopy -o -i "$ESP" "$DIR/dist/floor.seed" ::/ARGUS/FLOOR

printf 'ARGUS boot manager screen.\n' > "$DIR/BOOTMENU.CFG"
MTOOLS_SKIP_CHECK=1 mcopy -o -i "$ESP" "$DIR/BOOTMENU.CFG" ::/ARGUS/BOOTMENU.CFG \
    || die "could not put \\ARGUS\\BOOTMENU.CFG on the ESP"

SOCK="$DIR/monitor.sock"
LOG="$DIR/serial.log"
rm -f "$SOCK" "$LOG"
cp "$OVMF_VARS" "$DIR/vars.fd"

say "booting with \\ARGUS\\BOOTMENU.CFG on the ESP"
"$QEMU" \
    -machine q35 -m 128M -cpu max \
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$OVMF_CODE" \
    -drive "if=pflash,format=raw,unit=1,file=$DIR/vars.fd" \
    -drive "file=$DIR/dist/argus-disk.img,format=raw,if=virtio" \
    -serial "file:$LOG" \
    -monitor "unix:$SOCK,server=on,wait=off" \
    -display none -no-reboot -no-shutdown \
    >"$DIR/qemu.out" 2>&1 &
QPID=$!

MARKER='stage=bootmenu event=painted'
waited=0
while [ "$waited" -lt 300 ]; do
    if [ -f "$LOG" ] && grep -qF "$MARKER" "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.1
    waited=$((waited + 1))
done
if ! grep -qF "$MARKER" "$LOG" 2>/dev/null; then
    fail "the loader never said it painted the boot menu; see $LOG"
    kill "$QPID" 2>/dev/null
    wait "$QPID" 2>/dev/null
    exit 1
fi

# The loader holds the menu for its own 2000 ms timeout after that line, so this
# sleep lands inside the hold and photographs the menu, not the kernel's block.
sleep 0.6

REPORTED="$(grep -oE 'stage=bootmenu event=painted path=[^ ]+ canvas=[0-9]+x[0-9]+' "$LOG" \
    | tail -1 | sed -E 's/.*canvas=([0-9]+x[0-9]+)/\1/')"
if [ -z "$REPORTED" ]; then
    fail "the loader painted the menu but never said at what size"
fi

PNG="$OUT/bootmenu-${REPORTED:-unknown}.png"
PPM="$DIR/bootmenu.ppm"
rm -f "$PNG" "$PPM"
python3 - "$SOCK" "$PPM" <<'PY'
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
s.close()
PY

# The menu's timeout expires and the loader carries on: the boot this screen
# belongs to still has to reach the handoff, and that is checked below. QEMU is
# left running until it does, or for ten seconds if it does not.
carried=0
while [ "$carried" -lt 100 ]; do
    if grep -qF 'stage=handoff event=jump' "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.1
    carried=$((carried + 1))
done
python3 - "$SOCK" <<'PY'
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
try:
    s.connect(sys.argv[1])
    time.sleep(0.1)
    try:
        s.recv(65536)
    except socket.timeout:
        pass
    s.sendall(b"quit\n")
    time.sleep(0.2)
except OSError:
    pass
s.close()
PY

kill "$QPID" 2>/dev/null
wait "$QPID" 2>/dev/null

[ -s "$PPM" ] || die "no screendump written"
GOT="$(ppm_to_png "$PPM" "$PNG")" || die "could not convert the screendump"
rm -f "$PPM"
IHDR="$(png_ihdr "$PNG")" || die "could not read the IHDR of $PNG"

if [ "$IHDR" != "$REPORTED" ]; then
    fail "$(basename "$PNG") is $IHDR, the loader reported $REPORTED — the name would be a lie"
else
    pass "$(basename "$PNG") is $IHDR, the canvas the loader reported"
fi

# The boot must still be a boot: the menu is a screen, not a replacement for the
# handoff.
for line in "stage=bootinfo event=sealed" "stage=exit-boot-services result=EFI_SUCCESS" \
            "stage=handoff event=jump" "stage=bootmenu event=hold"; do
    if grep -qF "$line" "$LOG"; then
        pass "serial log has \"$line\""
    else
        fail "serial log is missing \"$line\""
    fi
done

printf '\n'
if [ "$FAILED" -ne 0 ]; then
    printf '\033[1;31m[bootmenu] FAIL\033[0m\n' >&2
    exit 1
fi
say "screenshot: $PNG"
exit 0
