"""Regenerates assets/ from source/icon.png. Run it after changing the logo:

    python assets/source/mkicons.py

Produces:
    assets/icon.ico        the app icon, 16-256px, for electron-builder and the window
    assets/icon.png        the same mark at 512, for anything that wants a PNG
    assets/tray.png        the mark alone, transparent, in colour   (recording)
    assets/tray-idle.png   the mark alone, transparent, grey        (idle)

ffmpeg is the only image tool this repo ships, so it does the scaling; the PNG and ICO
containers are written here rather than pulled in as a dependency.

The source is a bright mark on pure black, which means every edge pixel is already
colour x alpha. Dividing that back out recovers a clean alpha channel for the tray icons —
a colour key would leave the dark fringe that makes a small icon look dirty.
"""
import os, struct, subprocess, zlib

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FFMPEG = os.path.join(ROOT, 'ffmpeg-bin', 'ffmpeg.exe')
OUT = os.path.join(ROOT, 'assets')
SRC = os.path.join(OUT, 'source', 'icon.png')

# The master is square; read its side out of the PNG header rather than hardcoding it.
with open(SRC, 'rb') as f:
    SIDE = struct.unpack_from('>I', f.read(24), 16)[0]


def raw(vf, w, h):
    """One frame of SRC through `vf`, as RGBA bytes."""
    args = [FFMPEG, '-hide_banner', '-loglevel', 'error', '-i', SRC]
    if vf:
        args += ['-vf', vf]
    args += ['-f', 'rawvideo', '-pix_fmt', 'rgba', '-']
    data = subprocess.run(args, capture_output=True, check=True).stdout
    assert len(data) == w * h * 4, (len(data), w * h * 4)
    return bytearray(data)


def write_png(path, px, w, h):
    rows = b''.join(b'\x00' + bytes(px[y * w * 4:(y + 1) * w * 4]) for y in range(h))

    def chunk(tag, data):
        body = tag + data
        return struct.pack('>I', len(data)) + body + struct.pack('>I', zlib.crc32(body))

    blob = (b'\x89PNG\r\n\x1a\n'
            + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(rows, 9))
            + chunk(b'IEND', b''))
    if path:
        with open(path, 'wb') as f:
            f.write(blob)
    return blob


def unpremultiply(px):
    """Black background -> alpha, mark recovered with its edges intact."""
    for i in range(0, len(px), 4):
        a = max(px[i], px[i + 1], px[i + 2])
        px[i + 3] = a
        if a:
            for c in range(3):
                px[i + c] = min(255, px[i + c] * 255 // a)


def desaturate(px):
    for i in range(0, len(px), 4):
        g = (px[i] * 54 + px[i + 1] * 183 + px[i + 2] * 19) >> 8
        # Lifted, because a flat luma grey reads as mud at 16px on a dark taskbar.
        g = min(255, 96 + g * 2 // 3)
        px[i] = px[i + 1] = px[i + 2] = g


# ── The mark's bounding box, so a tray icon fills its 16 logical pixels ──
full = raw(None, SIDE, SIDE)
x0, y0, x1, y1 = SIDE, SIDE, 0, 0
for y in range(SIDE):
    row = full[y * SIDE * 4:(y + 1) * SIDE * 4]
    for x in range(SIDE):
        if max(row[x * 4], row[x * 4 + 1], row[x * 4 + 2]) > 8:
            x0, x1 = min(x0, x), max(x1, x)
            y0, y1 = min(y0, y), max(y1, y)
side = max(x1 - x0 + 1, y1 - y0 + 1)
cx, cy = (x0 + x1) // 2, (y0 + y1) // 2
crop = f'crop={side}:{side}:{cx - side // 2}:{cy - side // 2}'

# ── Tray: the mark alone, transparent. Colour means buffering, grey means idle. ──
for name, grey in (('tray.png', False), ('tray-idle.png', True)):
    px = raw(f'{crop},scale=32:32:flags=lanczos', 32, 32)
    unpremultiply(px)
    if grey:
        desaturate(px)
    write_png(os.path.join(OUT, name), px, 32, 32)
    print('wrote', name)

# ── App icon: the mark on its black tile, as drawn ──
write_png(os.path.join(OUT, 'icon.png'), raw('scale=512:512:flags=lanczos', 512, 512), 512, 512)
print('wrote icon.png')

# ── icon.ico: BMP entries at the small sizes, PNG at 128 and up (the Vista convention).
# electron-builder rejects an .ico without a 256, which is what the hand-made one lacked.
entries = []
for s in [16, 24, 32, 48, 64, 128, 256]:
    px = raw(f'scale={s}:{s}:flags=lanczos', s, s)
    if s >= 128:
        blob = write_png(None, px, s, s)
    else:
        # DIB: BGRA bottom-up, then the AND mask — all opaque, rows padded to 32 bits.
        bgra = bytearray()
        for y in range(s - 1, -1, -1):
            for x in range(s):
                i = (y * s + x) * 4
                bgra += bytes((px[i + 2], px[i + 1], px[i], px[i + 3]))
        header = struct.pack('<IiiHHIIiiII', 40, s, s * 2, 1, 32, 0, len(bgra), 0, 0, 0, 0)
        blob = header + bytes(bgra) + bytes((s + 31) // 32 * 4 * s)
    entries.append((s, blob))

ico = struct.pack('<HHH', 0, 1, len(entries))
offset = 6 + 16 * len(entries)
for s, blob in entries:
    ico += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(blob), offset)
    offset += len(blob)
ico += b''.join(b for _, b in entries)
with open(os.path.join(OUT, 'icon.ico'), 'wb') as f:
    f.write(ico)
print(f'wrote icon.ico — {len(entries)} sizes, {len(ico)} bytes')
