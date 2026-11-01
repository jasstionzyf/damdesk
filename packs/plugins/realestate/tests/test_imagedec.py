"""解码器单测：PNG 自造图逐像素对账 + JPEG 与 Pillow 参考值对账（容差 3%）。

JPEG 的参考值由 Pillow 按**同一 luma 公式**算出后固化成 fixtures/pillow_reference.json
（不是用本解码器自己算的"金标准"，否则测的是自己）。容许 3% 差异来自两处实现细节：
色度上采样（本包最近邻、libjpeg 花式）与 IDCT 舍入。
"""
from __future__ import annotations

import json
import os
import struct
import sys
import unittest
import zlib

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                "plugin"))

import imagedec          # noqa: E402
import ruleset           # noqa: E402

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")
REFERENCE = json.load(open(os.path.join(FIXTURES, "pillow_reference.json"),
                           encoding="utf-8"))


def _write_png(path, width, height, color_type, rows, depth=8):
    """最小 PNG 写入（无反滤波）：rows = 每行的原始字节。"""
    def chunk(tag, body):
        return (struct.pack(">I", len(body)) + tag + body
                + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF))
    ihdr = struct.pack(">IIBBBBB", width, height, depth, color_type, 0, 0, 0)
    raw = b"".join(b"\x00" + row for row in rows)
    data = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
    with open(path, "wb") as fh:
        fh.write(data)


class PngTest(unittest.TestCase):
    def setUp(self):
        self.tmp = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_tmp_png")
        os.makedirs(self.tmp, exist_ok=True)

    def tearDown(self):
        for name in os.listdir(self.tmp):
            os.remove(os.path.join(self.tmp, name))
        os.rmdir(self.tmp)

    def _decode(self, color_type, rows, width, height, depth=8):
        path = os.path.join(self.tmp, "case.png")
        _write_png(path, width, height, color_type, rows, depth=depth)
        return imagedec.decode_luma(path)

    def test_grayscale_passthrough(self):
        rows = [bytes([0, 128, 255])]
        width, height, luma = self._decode(0, rows, 3, 1)
        self.assertEqual((width, height), (3, 1))
        self.assertEqual(list(luma), [0, 128, 255])

    def test_rgb_uses_rec709(self):
        # 纯红 (255,0,0) → 2126*255/10000 = 54；纯绿 → 182；纯蓝 → 18
        rows = [bytes([255, 0, 0, 0, 255, 0, 0, 0, 255])]
        _, _, luma = self._decode(2, rows, 3, 1)
        self.assertEqual(list(luma), [54, 182, 18])

    def test_rgba_ignores_alpha(self):
        rows = [bytes([255, 255, 255, 0, 0, 0, 0, 255])]
        _, _, luma = self._decode(6, rows, 2, 1)
        self.assertEqual(list(luma), [255, 0])

    def test_16bit_takes_high_byte(self):
        # 每通道 2 字节；灰阶 0x80FF → 高字节 0x80
        rows = [bytes([0x80, 0xFF, 0x00, 0x00])]
        _, _, luma = self._decode(0, rows, 2, 1, depth=16)
        self.assertEqual(list(luma), [0x80, 0x00])

    def test_unknown_format_raises(self):
        path = os.path.join(self.tmp, "bad.bin")
        with open(path, "wb") as fh:
            fh.write(b"not an image at all")
        with self.assertRaises(imagedec.DecodeError):
            imagedec.decode_luma(path)

    def test_truncated_png_raises(self):
        path = os.path.join(self.tmp, "trunc.png")
        _write_png(path, 2, 2, 0, [b"\x01\x02", b"\x03\x04"])
        data = open(path, "rb").read()[:24]
        with open(path, "wb") as fh:
            fh.write(data)
        with self.assertRaises(imagedec.DecodeError):
            imagedec.decode_luma(path)


class JpegReferenceTest(unittest.TestCase):
    """与 Pillow 参考值对账（容差 3%）：四组夹具覆盖 sharp/blur/444/小图。"""

    def _metrics(self, name):
        width, height, luma = imagedec.decode_luma(os.path.join(FIXTURES, name))
        variance, _ = ruleset.laplacian_variance(luma, width, height)
        stats = ruleset.exposure_stats(luma)
        return width, height, variance, stats

    def test_dimensions_and_metrics_match_pillow(self):
        """逐夹具对账 Pillow 参考值；容差是**实测偏差的回归钉**（见模块 docstring）。"""
        for name, ref in REFERENCE.items():
            with self.subTest(name=name):
                width, height, variance, stats = self._metrics(name)
                tol = ref["tolerances"]
                self.assertEqual((width, height), (ref["width"], ref["height"]))
                if ref["laplacian_variance"] >= 1.0:
                    rel = abs(variance - ref["laplacian_variance"]) / ref["laplacian_variance"]
                    self.assertLessEqual(rel, tol["laplacian_variance_rel"],
                                         "%s 拉普拉斯方差偏差 %.3f 超容差 %.3f"
                                         % (name, rel, tol["laplacian_variance_rel"]))
                else:
                    self.assertLessEqual(abs(variance - ref["laplacian_variance"]), 1.0)
                self.assertLessEqual(abs(stats["dead_ratio"] - ref["dead_ratio"]),
                                     tol["dead_ratio_abs"])
                self.assertLessEqual(abs(stats["mean"] - ref["mean"]), tol["mean_abs"])

    def test_dc_plane_is_about_one_eighth(self):
        width, height, plane = imagedec.decode_dc_luma(
            os.path.join(FIXTURES, "sharp_640x480.jpg"))
        self.assertAlmostEqual(width, 640 / 8, delta=1)
        self.assertAlmostEqual(height, 480 / 8, delta=1)
        self.assertEqual(len(plane), width * height)

    def test_deterministic_repeat(self):
        first = imagedec.decode_luma(os.path.join(FIXTURES, "sharp_640x480.jpg"))
        second = imagedec.decode_luma(os.path.join(FIXTURES, "sharp_640x480.jpg"))
        self.assertEqual(first[2], second[2])


if __name__ == "__main__":
    unittest.main()
