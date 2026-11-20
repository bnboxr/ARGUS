#!/usr/bin/env python3
"""Generate the boot menu's bitmap glyph atlas on the build host.

    python3 tools/gen-bootmenu-atlas.py [-o shared/src/bootmenu_atlas.bin]

Why a host atlas and not a boot-time rasteriser: `shared/design/boot-manager-spec-v1.md`
§7 costs a TrueType rasteriser at ~1-3 ms per glyph in a UEFI loader with no GPU,
which is 30-90 ms for one label on every redraw. A pre-baked alpha atlas costs one
blend per glyph pixel instead (~0.1 ms per label). So every glyph this screen draws
is rasterised *here*, once, on the build host, and the loader only ever blits.

The face is **FreeSans / FreeSansBold** (GNU FreeFont, GPL v3 or later with the
font embedding exception), rendered through ffmpeg's `drawtext`. The reference PNG
names no font and ships none (spec §8: "Font identity — no font file is in the
reference"), so this is a substitute chosen for its shape class: both the reference
and FreeSans are a geometric-ish sans whose `O` is very nearly a circle. See
`deps.md` for the vendoring record.

What the atlas holds, per style:

  cap height in pixels, and for every character: an 8-bit alpha bitmap cropped to
  its ink, its ink offset from the pen origin (`dx`, and `dy` from the baseline),
  and its advance.

Advance is measured at cap 100 for each weight (where two copies of a glyph never
touch, so the column projection always finds two ink runs) and scaled linearly to
the real size, so a small size and a large size of the same glyph have consistent
proportions instead of accumulated rounding.

The output is committed. Re-running this script is only needed when the face, the
sizes or the charset change; `build.sh` does not need python or ffmpeg.
"""

import os
import struct
import subprocess
import sys
import tempfile

FONT_REGULAR = "/usr/share/fonts/truetype/freefont/FreeSans.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/freefont/FreeSansBold.ttf"

# Every style the boot menu draws, from spec §3. `cap` is the measured ink height
# of the reference's capitals; `ratio` is the reference's advance ÷ cap, which the
# renderer uses for tracking (it is the one number that survives a face change).
STYLES = [
    # name        cap  bold   advance/cap
    ("wordmark",   41, True,  1.02),
    ("subtitle",   16, False, 0.75),
    ("section",    15, False, 0.80),
    ("label",      11, False, 0.82),
    ("row",        18, False, 0.69),
    ("hint",       16, False, 0.66),
    ("footer",     11, False, 0.82),
]

# Printable ASCII, plus the two arrows the key-hint block needs (§1.10).
CHARSET = [chr(c) for c in range(0x20, 0x7F)] + ["\u2191", "\u2193"]

MAGIC = b"ARGBA1\x00\x00"
# Alpha at or below this is antialiasing dust, not ink.
INK = 8
PAD = 6
MEASURE_CAP = 100


def render(tmpdir, tag, font, fontsize, texts, cell_w, cell_h, pad_y):
    """Render `texts` into one RGBA frame, one text per cell; return the alpha.

    Every string gets its own textfile, so no character ever has to be escaped
    into a filter graph. One ffmpeg call per batch keeps the generator fast.
    """
    canvas_w = cell_w * len(texts)
    filters = []
    for index, text in enumerate(texts):
        path = os.path.join(tmpdir, "cell%04d.txt" % index)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        filters.append(
            "drawtext=fontfile=%s:textfile=%s:fontcolor=white@1.0"
            ":fontsize=%d:x=%d:y=%d:expansion=none"
            % (font, path, fontsize, index * cell_w, pad_y)
        )
    out = os.path.join(tmpdir, "render-%s.rgba" % tag)
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y",
         "-f", "lavfi", "-i",
         "color=c=black@0.0:s=%dx%d,format=rgba" % (canvas_w, cell_h),
         "-vf", ",".join(filters),
         "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", out],
        check=True,
    )
    data = open(out, "rb").read()
    expected = canvas_w * cell_h * 4
    if len(data) != expected:
        sys.exit("ffmpeg produced %d bytes, expected %d" % (len(data), expected))
    return data[3::4], canvas_w


def cells_of(alpha, canvas_w, cell_w, cell_h, count):
    """Split a rendered frame into one list-of-rows per cell."""
    out = []
    for index in range(count):
        x0 = index * cell_w
        rows = []
        for y in range(cell_h):
            base = y * canvas_w + x0
            rows.append(alpha[base:base + cell_w])
        out.append(rows)
    return out


def ink_box(rows):
    """Ink bbox of one cell, or None when it is empty (the space character)."""
    top = bottom = left = right = None
    for y, row in enumerate(rows):
        for x, value in enumerate(row):
            if value > INK:
                if top is None:
                    top = y
                bottom = y
                left = x if left is None or x < left else left
                right = x if right is None or x > right else right
    if top is None:
        return None
    return left, top, right, bottom


def cap_height_of(font, fontsize, tmpdir):
    """Ink height of `H` at this fontsize, in pixels."""
    cell_w = int(fontsize * 1.6) + 2 * PAD
    cell_h = int(fontsize * 2.1) + 2 * PAD
    pad_y = PAD + int(fontsize * 0.35)
    alpha, canvas_w = render(tmpdir, "cap", font, fontsize, ["H"],
                             cell_w, cell_h, pad_y)
    box = ink_box(cells_of(alpha, canvas_w, cell_w, cell_h, 1)[0])
    if box is None:
        sys.exit("the rendered 'H' has no ink — the font path is wrong")
    return box[3] - box[1] + 1


def fontsize_for_cap(font, cap, tmpdir):
    """Fontsize whose rendered `H` ink height is exactly `cap` px.

    FreeSans' cap/em is close to 0.72, so the search starts there and walks
    outwards a pixel at a time; a face whose cap rounds between two pixels gets
    the nearest size, and the renderer is told the cap it actually got.
    """
    start = max(6, int(round(cap / 0.72)))
    best = None
    for delta in range(0, 6):
        for fontsize in {start - delta, start + delta}:
            if fontsize < 5:
                continue
            height = cap_height_of(font, fontsize, tmpdir)
            if height == cap:
                return fontsize
            if best is None or abs(height - cap) < abs(best[1] - cap):
                best = (fontsize, height)
    return best[0]


def render_charset(font, fontsize, tmpdir, tag):
    """Render the whole charset at one size; return (cells, cell_w, cell_h, pad)."""
    cell_w = int(fontsize * 1.45) + 2 * PAD
    cell_h = int(fontsize * 2.1) + 2 * PAD
    pad_y = PAD + int(fontsize * 0.35)
    visible = [(i, ch) for i, ch in enumerate(CHARSET) if ch != " "]
    # A space has no ink, so it never needs rendering; it keeps its slot by index.
    texts = [ch for _, ch in visible]
    alpha, canvas_w = render(tmpdir, tag, font, fontsize, texts, cell_w, cell_h,
                             pad_y)
    cells = cells_of(alpha, canvas_w, cell_w, cell_h, len(texts))
    by_char = {ch: cells[n] for n, (_, ch) in enumerate(visible)}
    by_char[" "] = [[0] * cell_w for _ in range(cell_h)]
    return by_char, cell_w, cell_h, pad_y


def ink_runs(rows, cell_w):
    """Column projection of one cell: the x of every ink run start."""
    runs = []
    start = None
    for x in range(cell_w):
        ink = any(rows[y][x] > INK for y in range(len(rows)))
        if ink and start is None:
            start = x
        elif not ink and start is not None:
            runs.append(start)
            start = None
    if start is not None:
        runs.append(start)
    return runs


def measure_advances(font, tmpdir, tag):
    """Advance ÷ cap for every character, measured at MEASURE_CAP px."""
    fontsize = fontsize_for_cap(font, MEASURE_CAP, tmpdir)
    height = cap_height_of(font, fontsize, tmpdir)
    cell_w = int(fontsize * 3.2) + 2 * PAD
    cell_h = int(fontsize * 2.1) + 2 * PAD
    pad_y = PAD + int(fontsize * 0.35)

    chars = [ch for ch in CHARSET if ch != " "]
    alpha, canvas_w = render(tmpdir, tag + "-pairs", font, fontsize,
                             [ch * 2 for ch in chars], cell_w, cell_h, pad_y)
    cells = cells_of(alpha, canvas_w, cell_w, cell_h, len(chars))

    single, sw, sh, spad = render_charset(font, fontsize, tmpdir, tag + "-single")

    advances = {}
    for ch, rows in zip(chars, cells):
        runs = ink_runs(rows, cell_w)
        if len(runs) >= 2:
            advances[ch] = (runs[1] - runs[0]) / float(height)
            continue
        # Touching copies: ink + both sidebearings is the same answer for a
        # monoline face to within a pixel.
        box = ink_box(single[ch])
        if box is None:
            advances[ch] = 0.0
        else:
            left, _, right, _ = box
            lsb = left - PAD
            advances[ch] = (right - left + 1 + 2 * lsb) / float(height)

    # The space: the second `H` in "H H" sits one advance_H + one space further
    # right than the second `H` in "HH".
    alpha, canvas_w = render(tmpdir, tag + "-space", font, fontsize,
                             ["H H", "HH"], cell_w, cell_h, pad_y)
    spaced, tight = cells_of(alpha, canvas_w, cell_w, cell_h, 2)
    spaced_runs = ink_runs(spaced, cell_w)
    tight_runs = ink_runs(tight, cell_w)
    space = 0.26
    if len(spaced_runs) >= 2 and len(tight_runs) >= 2:
        space = ((spaced_runs[1] - spaced_runs[0])
                 - (tight_runs[1] - tight_runs[0])) / float(height)
    advances[" "] = space
    return advances


def build(out_path):
    tmpdir = tempfile.mkdtemp(prefix="argus-atlas-")
    print("measuring advances at cap %d px" % MEASURE_CAP)
    adv_regular = measure_advances(FONT_REGULAR, tmpdir, "regular")
    adv_bold = measure_advances(FONT_BOLD, tmpdir, "bold")

    sizes = []
    for name, cap, bold, ratio in STYLES:
        font = FONT_BOLD if bold else FONT_REGULAR
        advances = adv_bold if bold else adv_regular
        fontsize = fontsize_for_cap(font, cap, tmpdir)
        cells, cell_w, cell_h, pad_y = render_charset(font, fontsize, tmpdir,
                                                      name)
        hbox = ink_box(cells["H"])
        got_cap = hbox[3] - hbox[1] + 1
        # Every glyph in a style is drawn at the same y, so one baseline serves
        # them all: the last ink row of `H`, plus one.
        baseline = hbox[3] + 1

        glyphs = []
        for ch in CHARSET:
            rows = cells[ch]
            box = ink_box(rows)
            advance = int(round(advances.get(ch, 0.0) * cap))
            if box is None:
                glyphs.append((ord(ch), 0, 0, 0, 0, advance, b""))
                continue
            left, top, right, bottom = box
            bitmap = bytearray()
            for y in range(top, bottom + 1):
                bitmap.extend(rows[y][left:right + 1])
            glyphs.append((
                ord(ch), right - left + 1, bottom - top + 1,
                left - PAD,       # dx from the pen origin
                top - baseline,   # dy from the baseline (up is negative)
                advance, bytes(bitmap),
            ))
        sizes.append((name, cap, got_cap, ratio, glyphs))
        print("  %-9s cap %2d (H is %2d px) fontsize %2d  %d glyphs"
              % (name, cap, got_cap, fontsize, len(glyphs)))

    blob = bytearray()
    header = bytearray()
    header.extend(MAGIC)
    header.extend(struct.pack("<HH", len(sizes), 0))
    for _name, cap, got_cap, ratio, glyphs in sizes:
        header.extend(struct.pack("<HHHh", cap, got_cap, len(glyphs),
                                  int(round(ratio * 1000))))
        for cp, width, height, dx, dy, advance, bitmap in glyphs:
            offset = len(blob)
            blob.extend(bitmap)
            while len(blob) % 4:
                blob.append(0)
            header.extend(struct.pack("<HHHhhHI", cp, width, height, dx, dy,
                                      advance, offset))
    header.extend(struct.pack("<I", len(blob)))
    with open(out_path, "wb") as handle:
        handle.write(bytes(header))
        handle.write(bytes(blob))
    print("wrote %s: %d bytes header + %d bytes bitmap"
          % (out_path, len(header), len(blob)))


if __name__ == "__main__":
    target = (sys.argv[sys.argv.index("-o") + 1]
              if "-o" in sys.argv
              else os.path.join(os.path.dirname(os.path.dirname(
                  os.path.abspath(__file__))), "shared", "src",
                  "bootmenu_atlas.bin"))
    build(target)
