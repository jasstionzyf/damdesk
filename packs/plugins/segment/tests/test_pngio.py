"""PNG 编解码：五种 filter 都能解、编码可复现、越界显式报错。"""
from __future__ import annotations

import os
import struct
import unittest
import zlib

import _support  # noqa: F401  (装配 sys.path)
import pngio


def _png(width, height, channels, rows, ftype):
    """手工造 PNG：`rows` 为原始（未 filter）行字节，按 ftype 施加 filter。"""
    color = {1: 0, 3: 2, 4: 6}[channels]
    bpp = channels
    prev = bytearray(width * channels)
    out = bytearray()
    for row in rows:
        cur = bytearray(row)
        filtered = bytearray(len(cur))
        for i in range(len(cur)):
            a = cur[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            if ftype == 0:
                pr = 0
            elif ftype == 1:
                pr = a
            elif ftype == 2:
                pr = b
            elif ftype == 3:
                pr = (a + b) >> 1
            else:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
            filtered[i] = (cur[i] - pr) & 0xFF
        out.append(ftype)
        out += filtered
        prev = cur
    ihdr = struct.pack(">IIBBBBB", width, height, 8, color, 0, 0, 0)
    return (pngio.PNG_SIG
            + pngio._chunk(b"IHDR", ihdr)
            + pngio._chunk(b"IDAT", zlib.compress(bytes(out)))
            + pngio._chunk(b"IEND", b""))


class PngDecodeTest(unittest.TestCase):
    def test_all_five_filters_decode_to_same_pixels(self):
        width, height = 7, 5
        rows = [bytes(((x * 13 + y * 7) % 256) for x in range(width * 3))
                for y in range(height)]
        for ftype in range(5):
            blob = _png(width, height, 3, rows, ftype)
            w, h, ch, raw = pngio.decode_bytes(blob)
            self.assertEqual((w, h, ch), (width, height, 3), "filter=%d" % ftype)
            self.assertEqual(bytes(raw), b"".join(rows), "filter=%d 像素必须逐字节一致" % ftype)

    def test_gray_and_rgba_channels_reported(self):
        gray = _png(3, 2, 1, [b"\x01\x02\x03", b"\x04\x05\x06"], 0)
        self.assertEqual(pngio.decode_bytes(gray)[2], 1)
        rgba = _png(2, 1, 4, [b"\x01\x02\x03\x04\x05\x06\x07\x08"], 0)
        self.assertEqual(pngio.decode_bytes(rgba)[2], 4)

    def test_rejects_unsupported_input(self):
        width, height = 2, 2
        rows = [b"\x00\x01\x02\x03\x04\x05", b"\x06\x07\x08\x09\x0a\x0b"]
        blob = bytearray(_png(width, height, 3, rows, 0))
        inter = bytearray(blob)
        ihdr_at = 8 + 8              # IHDR 数据起点
        inter[ihdr_at + 12] = 1      # interlace = 1
        with self.assertRaises(pngio.PngError):
            pngio.decode_bytes(bytes(inter))
        with self.assertRaises(pngio.PngError):
            pngio.decode_bytes(b"not a png at all")
        deep = bytearray(blob)
        deep[ihdr_at + 8] = 16       # bit depth = 16
        with self.assertRaises(pngio.PngError):
            pngio.decode_bytes(bytes(deep))


class PngEncodeTest(unittest.TestCase):
    def test_roundtrip_rgb_and_rgba(self):
        for channels in (3, 4):
            raw = bytes(range(256)) * (2 * channels // 2)
            raw = bytes((i * 7) % 256 for i in range(4 * 3 * channels))
            blob = pngio.encode(4, 3, channels, raw)
            w, h, ch, back = pngio.decode_bytes(blob)
            self.assertEqual((w, h, ch), (4, 3, channels))
            self.assertEqual(bytes(back), raw)

    def test_encoding_is_byte_reproducible(self):
        raw = bytes((i * 31) % 256 for i in range(16 * 16 * 3))
        first = pngio.encode(16, 16, 3, raw)
        second = pngio.encode(16, 16, 3, raw)
        self.assertEqual(first, second, "同输入必须同字节（N14 重跑 sha256 一致）")
        self.assertNotIn(b"tIME", first)
        self.assertNotIn(b"tEXt", first)

    def test_write_png_and_size_guard(self):
        tmp = _support.tempdir()
        path = os.path.join(tmp, "x.png")
        raw = bytes([1, 2, 3] * 4)
        pngio.write_png(path, 2, 2, 3, raw)
        self.assertEqual(pngio.decode_path(path)[3], bytearray(raw))
        with self.assertRaises(pngio.PngError):
            pngio.encode(2, 2, 3, b"too short")


if __name__ == "__main__":
    unittest.main()
