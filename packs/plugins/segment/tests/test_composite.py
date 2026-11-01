"""alpha 与合成：二值化、斜坡宽度、羽化单调性、三种输出形态。"""
from __future__ import annotations

import unittest

import _support  # noqa: F401
import composite


def _binary(width, height, box):
    x1, y1, x2, y2 = box
    raw = bytearray(width * height)
    for y in range(y1, y2):
        raw[y * width + x1:y * width + x2] = b"\xff" * (x2 - x1)
    return bytes(raw)


class BinarizeTest(unittest.TestCase):
    def test_threshold_edge_cases(self):
        plane = bytes([0, 1, 127, 128, 254, 255])
        self.assertEqual(bytes(composite.binarize(plane, 128)), bytes([0, 0, 0, 255, 255, 255]))
        self.assertEqual(bytes(composite.binarize(plane, 0)), b"\xff" * 6)
        self.assertEqual(bytes(composite.binarize(plane, 255)), bytes([0, 0, 0, 0, 0, 255]))


class AlphaTest(unittest.TestCase):
    def test_ramp_at_edge_and_solid_inside(self):
        width = height = 640            # cell = round(640/320) = 2
        box = (200, 200, 440, 440)
        binary = _binary(width, height, box)
        alpha = composite.build_alpha(binary, width, height, feather=0)
        self.assertEqual(alpha[300 * width + 300], 255, "主体内部必须是不透明")
        self.assertEqual(alpha[10 * width + 10], 0, "远处背景必须全透明")
        partial = sum(1 for v in alpha if 0 < v < 255)
        self.assertGreater(partial, 0, "边缘必须有部分透明像素（抗锯齿）")

    def test_feather_only_widens_the_ramp(self):
        width = height = 640
        box = (200, 200, 440, 440)
        binary = _binary(width, height, box)
        soft0 = sum(1 for v in composite.build_alpha(binary, width, height, 0) if 0 < v < 255)
        soft6 = sum(1 for v in composite.build_alpha(binary, width, height, 6) if 0 < v < 255)
        self.assertGreater(soft6, soft0, "feather 必须真的加宽斜坡（6 > 0）")

    def test_ramp_metric(self):
        self.assertEqual(composite.alpha_ramp_px(None, 640, 640, 0), 2 * composite.cell_size(640, 640))
        self.assertEqual(composite.alpha_ramp_px(None, 640, 640, 4), 2 * (composite.cell_size(640, 640) + 4))

    def test_cell_size_tracks_host_grid(self):
        self.assertEqual(composite.cell_size(6016, 4000), 12)   # 4000/320
        self.assertEqual(composite.cell_size(554, 500), 2)      # 500/320 → 1.56 → 2
        self.assertEqual(composite.cell_size(10, 10), 1)


class BlendTest(unittest.TestCase):
    def test_rgb_uses_bg_where_transparent_and_src_where_opaque(self):
        width, height = 4, 2
        src = bytes([10, 20, 30] * width + [200, 210, 220] * width)
        alpha = bytearray([0] * width + [255] * width)
        out = composite.blend_rgb(src, width, height, alpha, (255, 255, 255))
        self.assertEqual(bytes(out[:width * 3]), bytes([255, 255, 255] * width))
        self.assertEqual(bytes(out[width * 3:]), bytes([200, 210, 220] * width))

    def test_rgb_half_alpha_midpoint(self):
        width, height = 1, 1
        src = bytes([0, 0, 0])
        out = composite.blend_rgb(src, width, height, bytearray([128]), (255, 255, 255))
        # a=128/255 → 白占 127/255 → 127（不是 128：分母是 255 不是 256）
        self.assertEqual(bytes(out), bytes([127, 127, 127]), "半透明 → 黑与白的中点")

    def test_rgba_keeps_unpremultiplied_rgb(self):
        width, height = 2, 1
        src = bytes([10, 20, 30, 40, 50, 60])
        alpha = bytearray([0, 200])
        out = composite.blend_rgba(src, width, height, alpha)
        self.assertEqual(bytes(out), bytes([10, 20, 30, 0, 40, 50, 60, 200]),
                         "alpha=0 的像素也要保留 RGB（后续贴图工具自己决定）")


class SubjectChangeTest(unittest.TestCase):
    """守卫③ 的度量层：只数「不透明核心」被改动，斜坡像素的合法混色不算。"""

    def test_opaque_pixels_untouched_is_zero_change(self):
        width, height = 3, 1
        src = bytes([10, 20, 30, 40, 50, 60, 70, 80, 90])
        alpha = bytearray([0, 128, 255])
        out = composite.blend_rgb(src, width, height, alpha, (255, 255, 255))
        subject, changed = composite.subject_change(src, out, alpha, width, height)
        self.assertEqual(subject, 1, "只有 alpha=255 的像素算主体核心")
        self.assertEqual(changed, 0, "斜坡像素被背景色混合不算改动")

    def test_shifted_content_counts_as_change(self):
        width, height = 3, 1
        src = bytes([10, 20, 30, 40, 50, 60, 70, 80, 90])
        alpha = bytearray([255, 255, 255])
        shifted = composite.blend_rgb(src, width, height, alpha, (255, 255, 255))
        moved = shifted[3:] + shifted[:3]        # 整体错位一个像素
        subject, changed = composite.subject_change(src, moved, alpha, width, height)
        self.assertEqual(subject, 3)
        self.assertGreater(changed, 0, "错位必须被量出来（否则守卫形同虚设）")


if __name__ == "__main__":
    unittest.main()
