"""Build the tiny geometric application icon using only Python's stdlib."""
from pathlib import Path
import struct
import zlib


def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)


size = 64
pixels = bytearray()
for y in range(size):
    pixels.append(0)
    for x in range(size):
        color = (33, 42, 56, 255)
        for left, top, fill in [(13, 13, (141, 221, 187, 255)), (35, 13, (230, 237, 245, 255)),
                                (13, 35, (230, 237, 245, 255)), (35, 35, (230, 237, 245, 255))]:
            if left <= x < left + 16 and top <= y < top + 16:
                color = fill
        pixels.extend(color)
png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 6, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(pixels, 9)) + chunk(b'IEND', b'')
output = Path(__file__).resolve().parents[1] / 'src-tauri' / 'icons'
output.mkdir(parents=True, exist_ok=True)
(output / 'icon.png').write_bytes(png)
ico = struct.pack('<HHH', 0, 1, 1) + struct.pack('<BBBBHHII', size, size, 0, 0, 1, 32, len(png), 22) + png
(output / 'icon.ico').write_bytes(ico)
