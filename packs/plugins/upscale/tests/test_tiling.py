"""分块几何（纯函数）：网格 / 边距裁剪框 / 落点偏移 / 画布上限 / 越界拒绝。"""
from __future__ import annotations

import sys
import unittest

import _support
import tiling


class GridTest(unittest.TestCase):
    def test_grid_covers_image_without_overlap(self):
        boxes = tiling.grid(900, 600, 384)
        self.assertEqual(boxes, [(0, 0, 384, 384), (384, 0, 768, 384), (768, 0, 900, 384),
                                 (0, 384, 384, 600), (384, 384, 768, 600), (768, 384, 900, 600)])
        area = sum((x1 - x0) * (y1 - y0) for x0, y0, x1, y1 in boxes)
        self.assertEqual(area, 900 * 600, "网格必须无重叠无遗漏")

    def test_grid_exact_multiple(self):
        self.assertEqual(tiling.grid(768, 384, 384), [(0, 0, 384, 384), (384, 0, 768, 384)])

    def test_grid_rejects_zero_block(self):
        with self.assertRaises(ValueError):
            tiling.grid(10, 10, 0)


class BlockForTest(unittest.TestCase):
    def test_auto_no_tile_under_cap(self):
        self.assertIsNone(tiling.block_for("auto", 600, 450, 4_000_000))

    def test_auto_tiles_over_cap(self):
        self.assertEqual(tiling.block_for("auto", 4000, 3000, 4_000_000), tiling.AUTO_BLOCK)

    def test_auto_block_is_under_cap(self):
        self.assertLess(tiling.AUTO_BLOCK * tiling.AUTO_BLOCK, 4_000_000)

    def test_explicit_block_always_honoured(self):
        self.assertEqual(tiling.block_for("384", 100, 100, 4_000_000), 384,
                         "显式块边长在同一参数下必须行为一致（图小也照切）")


class PaddedBoxTest(unittest.TestCase):
    def test_margin_expands_and_clips(self):
        self.assertEqual(tiling.padded_box((0, 0, 384, 384), 900, 600), (0, 0, 392, 392))
        self.assertEqual(tiling.padded_box((768, 384, 900, 600), 900, 600), (760, 376, 900, 600))

    def test_interior_box_gets_full_margin(self):
        x0, y0, x1, y1 = tiling.padded_box((384, 384, 768, 600), 900, 600)
        self.assertEqual((x0, y0), (384 - tiling.MARGIN, 384 - tiling.MARGIN))
        self.assertEqual((x1, y1), (768 + tiling.MARGIN, 600))


class StitchOffsetsTest(unittest.TestCase):
    def test_offsets_account_for_margin(self):
        box = (384, 0, 768, 384)
        pbox = tiling.padded_box(box, 900, 600)
        dst_x, dst_y, src_x, src_y, w, h = tiling.stitch_offsets(box, pbox, 4)
        self.assertEqual((dst_x, dst_y), (1536, 0))
        self.assertEqual((src_x, src_y), (tiling.MARGIN * 4, 0))
        self.assertEqual((w, h), ((768 - 384) * 4, 384 * 4))

    def test_border_box_clipped_margin(self):
        box = (0, 0, 384, 384)
        pbox = tiling.padded_box(box, 900, 600)
        dst_x, dst_y, src_x, src_y, _w, _h = tiling.stitch_offsets(box, pbox, 4)
        self.assertEqual((dst_x, dst_y, src_x, src_y), (0, 0, 0, 0))


class CanvasTest(unittest.TestCase):
    def test_canvas_size_is_factor_times(self):
        canvas, cw, ch = tiling.new_canvas(30, 20, 4)
        self.assertEqual((cw, ch, len(canvas)), (120, 80, 120 * 80 * 3))

    def test_canvas_cap_rejected_with_actionable_message(self):
        with self.assertRaises(ValueError) as ctx:
            tiling.new_canvas(8000, 8000, 4)
        self.assertIn("上限", str(ctx.exception))
        self.assertIn("缩小", str(ctx.exception))


class BlitTest(unittest.TestCase):
    def test_blit_copies_inner_region_rows(self):
        canvas, cw, ch = tiling.new_canvas(2, 2, 1)      # 简化：factor=1 直接看落点
        tile_w, tile_h = 4, 4
        raw = bytes(range(4 * 4 * 3))
        tiling.blit(canvas, cw, ch, tile_w, tile_h, 3, raw,
                    dst_x=1, dst_y=0, src_x=1, src_y=1, w=1, h=2)
        # 画布 (1,0) 取块的 (1,1)
        self.assertEqual(bytes(canvas[3:6]), raw[(1 * tile_w + 1) * 3:(1 * tile_w + 1) * 3 + 3])
        # 画布 (1,1) 取块的 (1,2)
        self.assertEqual(bytes(canvas[(1 * cw + 1) * 3:(1 * cw + 1) * 3 + 3]),
                         raw[(2 * tile_w + 1) * 3:(2 * tile_w + 1) * 3 + 3])

    def test_blit_rejects_out_of_range(self):
        canvas, cw, ch = tiling.new_canvas(1, 1, 1)
        with self.assertRaises(ValueError):
            tiling.blit(canvas, cw, ch, 2, 2, 3, bytes(2 * 2 * 3),
                        dst_x=0, dst_y=0, src_x=1, src_y=0, w=2, h=2)

    def test_blit_rejects_non_rgb(self):
        canvas, cw, ch = tiling.new_canvas(1, 1, 1)
        with self.assertRaises(ValueError):
            tiling.blit(canvas, cw, ch, 1, 1, 4, bytes(4), 0, 0, 0, 0, 1, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
