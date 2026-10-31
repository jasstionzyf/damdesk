"""几何闭式解单测：窗口、收敛目标、不可解分支、确定性。"""
from __future__ import annotations

import math
import unittest

import _support  # noqa: F401  (导入即把 plugin/ 放上 sys.path)

import geometry
import ruleset


class SolveTest(unittest.TestCase):
    def test_small_low_ratio_crops_and_upscales(self):
        plan = geometry.solve(600, 600, 0.30)
        self.assertTrue(plan.fixable)
        self.assertEqual(len(plan["crop_box"]), 4)
        x1, y1, x2, y2 = plan["crop_box"]
        self.assertEqual(x2 - x1, y2 - y1)          # 方形裁剪窗
        self.assertLessEqual(x2 - x1, 600)
        self.assertGreaterEqual(plan["predicted_ratio"], ruleset.THRESHOLDS[
            "R4_subject_ratio"]["min"])
        self.assertLessEqual(plan["predicted_ratio"], ruleset.THRESHOLDS[
            "R4_subject_ratio"]["max"])
        self.assertGreaterEqual(plan["predicted_side"], 1600)   # 短边门槛
        self.assertGreater(plan["scale"], 1.0)

    def test_centered_crop_is_centered(self):
        plan = geometry.solve(1000, 1000, 0.20)
        x1, y1, x2, y2 = plan["crop_box"]
        self.assertAlmostEqual(x1, 1000 - x2, delta=2)
        self.assertAlmostEqual(y1, 1000 - y2, delta=2)

    def test_wide_image_needs_letterbox(self):
        # fg 已接近目标 + 非正方形 → 只能裁短边、靠补白成 1:1，不能把长边裁掉
        plan = geometry.solve(1600, 1000, 0.60)
        self.assertTrue(plan.fixable)
        x1, y1, x2, y2 = plan["crop_box"] or [0, 0, 1600, 1000]
        self.assertEqual(y1, 0)
        self.assertEqual(y2, 1000)                   # 短边不裁
        self.assertTrue(geometry.needs_letterbox(plan["crop_box"], 1600, 1000))
        self.assertGreaterEqual(plan["predicted_ratio"], 0.55)
        self.assertLessEqual(plan["predicted_ratio"], 0.85)

    def test_subject_too_full_is_unfixable(self):
        plan = geometry.solve(1600, 1600, 0.92)
        self.assertFalse(plan.fixable)
        self.assertEqual(plan["unfixable"], "subject_ratio_above_max")
        self.assertIsNone(plan["crop_box"])
        self.assertTrue(plan["reason"])              # 必须给人读理由（落 needs_human）

    def test_missing_measurement_is_unfixable(self):
        plan = geometry.solve(1600, 1600, None)
        self.assertFalse(plan.fixable)
        self.assertEqual(plan["unfixable"], "no_subject_measurement")

    def test_tiny_asset_is_unfixable(self):
        plan = geometry.solve(16, 16, 0.30)
        self.assertFalse(plan.fixable)
        self.assertEqual(plan["unfixable"], "asset_too_small")

    def test_predictions_always_inside_window(self):
        """扫参数：只要判为可解，预测占比就必须落进 [min, max]、短边达标。"""
        lo, hi = 0.55, 0.85
        for width, height in ((600, 600), (1600, 1600), (2400, 1600), (1600, 2400),
                              (3000, 2000), (800, 2400)):
            for ratio in (0.05, 0.10, 0.20, 0.30, 0.45, 0.549, 0.55, 0.70, 0.84, 0.851):
                plan = geometry.solve(width, height, ratio)
                if not plan.fixable:
                    self.assertGreater(ratio, hi - 1e-9, "只有超上限才允许不可解：%s"
                                       % ((width, height, ratio),))
                    continue
                if ratio >= lo and min(width, height) >= 1600 and width == height:
                    self.assertEqual(plan["crop_box"], None, "已达标且正方形不该裁：%s"
                                     % ((width, height, ratio),))
                self.assertLessEqual(plan["predicted_ratio"], hi + 1e-6,
                                     "预测占比越上限：%s" % ((width, height, ratio),))
                self.assertGreaterEqual(plan["predicted_side"], 1600,
                                        "短边没达标：%s" % ((width, height, ratio),))

    def test_deterministic(self):
        first = dict(geometry.solve(2400, 1600, 0.31))
        second = dict(geometry.solve(2400, 1600, 0.31))
        self.assertEqual(first, second)

    def test_noop_when_already_compliant(self):
        plan = geometry.solve(1600, 1600, 0.70)
        self.assertTrue(plan.fixable)
        self.assertIsNone(plan["crop_box"])
        self.assertEqual(plan["scale"], 1.0)
        self.assertFalse(geometry.needs_letterbox(plan["crop_box"], 1600, 1600))
        self.assertAlmostEqual(plan["predicted_ratio"], 0.70, places=3)

    def test_letterbox_helper(self):
        self.assertFalse(geometry.needs_letterbox([0, 0, 500, 500], 800, 800))
        self.assertTrue(geometry.needs_letterbox([0, 0, 500, 400], 800, 800))
        self.assertTrue(geometry.needs_letterbox(None, 1600, 1000))
        self.assertFalse(geometry.needs_letterbox(None, 1000, 1000))

    def test_crop_box_inside_canvas(self):
        for width, height in ((600, 600), (1000, 3000), (3000, 1000)):
            plan = geometry.solve(width, height, 0.10)
            x1, y1, x2, y2 = plan["crop_box"]
            self.assertGreaterEqual(x1, 0)
            self.assertGreaterEqual(y1, 0)
            self.assertLessEqual(x2, width)
            self.assertLessEqual(y2, height)
            self.assertGreaterEqual(min(x2 - x1, y2 - y1), geometry.MIN_SIDE)

    def test_target_ratio_matches_area_math(self):
        """闭式解自证：预测占比 == 主体面积 / 画布面积（不靠调参凑出来的数）。"""
        plan = geometry.solve(1200, 900, 0.25, target_ratio=0.75)
        area = 0.25 * 1200 * 900
        side = max(plan["predicted_side"] / plan["scale"], 1)
        self.assertAlmostEqual(plan["predicted_ratio"], round(area / (side * side), 4),
                               places=3)
        self.assertLess(math.fabs(plan["predicted_ratio"] - 0.75), 0.02)


class AnchorTest(unittest.TestCase):
    """协议 1.1：裁剪窗不再是「只能居中推定」，而是宿主窗 / 主体框锚定。"""

    def test_window_from_host_is_used_verbatim(self):
        # 宿主给的窗：接管窗口与预测占比（窗内网格占比），插件只补白/放大
        window = {"box": [40, 20, 540, 520], "ratio": 0.75, "target_ratio": 0.75}
        plan = geometry.solve(800, 600, 0.30, window=window)
        self.assertTrue(plan.fixable)
        self.assertEqual(plan["anchor"], "window")
        self.assertEqual(plan["crop_box"], [40, 20, 540, 520])
        # 窗是正方形 → 无补白；short_side 不足 → 放大
        self.assertFalse(geometry.needs_letterbox(plan["crop_box"], 800, 600))
        self.assertAlmostEqual(plan["predicted_ratio"], 0.75, places=3)
        self.assertGreaterEqual(plan["predicted_side"], 1600)

    def test_window_is_clamped_into_canvas(self):
        # 越界窗不许造出负数坐标：钳进画面，最小边长不足则退回下一档锚点
        plan = geometry.solve(600, 600, 0.30,
                             window={"box": [-100, -50, 400, 450], "ratio": 0.8})
        self.assertEqual(plan["crop_box"], [0, 0, 400, 450])
        plan = geometry.solve(600, 600, 0.30, window={"box": [10, 10, 20, 20], "ratio": 0.9})
        self.assertEqual(plan["anchor"], "center")     # 退化窗 → 回退居中档，不报错

    def test_bbox_anchor_keeps_subject_inside_window(self):
        # 主体贴右上角：窗口必须覆盖主体紧框，且不再以画面中心为锚
        bbox = [520, 10, 700, 190]
        plan = geometry.solve(900, 700, 0.06, bbox=bbox)
        self.assertTrue(plan.fixable)
        self.assertEqual(plan["anchor"], "bbox")
        x1, y1, x2, y2 = plan["crop_box"]
        self.assertLessEqual(x1, bbox[0])
        self.assertLessEqual(y1, bbox[1])
        self.assertGreaterEqual(x2, bbox[2])
        self.assertGreaterEqual(y2, bbox[3])
        self.assertGreaterEqual(min(x2 - x1, y2 - y1), 180)   # ≥ 主体紧框

    def test_bbox_anchor_differs_from_center(self):
        # 同一张图：居中推定会裁掉贴边主体，bbox 锚定不会
        centered = geometry.solve(900, 700, 0.06)
        anchored = geometry.solve(900, 700, 0.06, bbox=[520, 10, 700, 190])
        self.assertNotEqual(centered["crop_box"], anchored["crop_box"])
        self.assertGreater(anchored["crop_box"][0], centered["crop_box"][0])

    def test_no_anchor_keeps_legacy_center_behavior(self):
        plan = geometry.solve(1000, 1000, 0.20)
        self.assertEqual(plan["anchor"], "center")
        x1, y1, x2, y2 = plan["crop_box"]
        self.assertAlmostEqual(x1, 1000 - x2, delta=2)

    def test_deterministic_with_anchors(self):
        window = {"box": [40, 20, 540, 520], "ratio": 0.75}
        first = dict(geometry.solve(800, 600, 0.30, window=window))
        second = dict(geometry.solve(800, 600, 0.30, window=window))
        self.assertEqual(first, second)
        first = dict(geometry.solve(900, 700, 0.06, bbox=[520, 10, 700, 190]))
        second = dict(geometry.solve(900, 700, 0.06, bbox=[520, 10, 700, 190]))
        self.assertEqual(first, second)


class BboxDrivenTest(unittest.TestCase):
    """D5=A：几何改 bbox 驱动（不再用 fg_ratio 反推窗口），一次求解就要命中目标区间。"""

    def test_one_pass_hits_target_window_and_short_side(self):
        # 主体紧框 1200x1100 居中的 3000x2400 白底图：占比 0.1375 → 紧框内占比 0.75
        plan = geometry.solve(3000, 2400, 0.1375, bbox=[900, 650, 2100, 1750])
        self.assertTrue(plan.fixable)
        self.assertEqual(plan["anchor"], "bbox")
        self.assertEqual(plan["crop_box"], [900, 650, 2100, 1750])   # 一次裁到紧框
        self.assertGreaterEqual(plan["predicted_ratio"], 0.55)
        self.assertLessEqual(plan["predicted_ratio"], 0.85)
        self.assertGreaterEqual(plan["predicted_side"], 1600)
        self.assertAlmostEqual(plan["predicted_ratio_aspect"], 0.75, places=3)

    def test_letterbox_is_skipped_when_it_would_break_ratio_min(self):
        """宽主体：裁到紧框后占比 0.715 达标，但补成 1:1 会掉到 0.385 → 不补白。"""
        plan = geometry.solve(2560, 1920, 0.4454, bbox=[88, 318, 2472, 1602])
        self.assertTrue(plan.fixable)
        self.assertFalse(plan["letterbox"])
        self.assertEqual(plan["crop_box"], [88, 318, 2472, 1602])
        self.assertAlmostEqual(plan["predicted_ratio"], 0.715, places=3)
        # 被跳过的补白占比要落 evidence，方便人工核对「为什么不补白」
        self.assertLess(plan["letterbox_skipped"], 0.55)

    def test_letterbox_kept_when_square_canvas_still_compliant(self):
        plan = geometry.solve(3221, 2530, 0.5087, bbox=[362, 134, 3091, 2396])
        self.assertTrue(plan["letterbox"])
        self.assertGreaterEqual(plan["predicted_ratio"], 0.55)
        self.assertEqual(plan["predicted_side"], max(2729, 2262))

    def test_subject_too_wide_for_min_ratio_is_unfixable_not_clipped(self):
        """保主体就达不到下限 → 判不可解（交人工），绝不靠继续切主体凑占比。"""
        # 主体紧框吃满宽度（占比本身就低于下限），方形画布下最高只能到 ~0.30
        plan = geometry.solve(2000, 2000, 0.30, bbox=[0, 700, 2000, 1300])
        self.assertFalse(plan.fixable)
        self.assertEqual(plan["unfixable"], "subject_ratio_below_min_keeping_subject")
        self.assertIsNone(plan["crop_box"])
        self.assertIn("切到主体", plan["reason"])

    def test_wide_subject_ratio_above_max_expands_instead_of_giving_up(self):
        # 占比超上限：向主体周围扩窗（纳背景）把占比压回 target；画面够大就能解
        plan = geometry.solve(4000, 2000, 0.92, bbox=[1000, 300, 3000, 1700])
        self.assertTrue(plan.fixable)
        self.assertEqual(plan["anchor"], "bbox")
        self.assertLessEqual(plan["predicted_ratio"], 0.85)
        self.assertGreaterEqual(plan["predicted_ratio"], 0.55)

    def test_k07_four_failed_samples_regression(self):
        """K07 门禁指标 4 的 4 张失败样本（真实语料 + 真实 u2net 掩码）回归。

        输入数据来源（可复算）：
        - 语料 `/data5/damdesk-ecommerce-pack/corpus/pass_fixtures/a00{2,3,4,5}.jpg`；
        - `fg_ratio`/`dominant_bbox` 由 `engine-segment` 的 mask_probe 在 320² 掩码上实测
          （与 K07 报告逐值一致：0.4454 / 0.5087 / 0.3487 / 0.4148），bbox 按
          `pack-host::grid_box_to_pixels` 换算成源图像素；
        - 「实测占比」= crop_close 探针把同一窗口真裁下来再跑一遍 u2net 的读数
          （0.716 / 0.667 / 0.612 / 0.678）。K07 当时修 3 轮后是 0.032 / 0.213 / 0.125。
        """
        cases = [
            # name, w, h, fg_ratio, dominant_bbox, 实测裁剪后占比
            ("a002", 2560, 1920, 0.445381, [88, 318, 2472, 1602], 0.716),
            ("a003", 3221, 2530, 0.508662, [362, 134, 3091, 2396], 0.667),
            ("a004", 2146, 2146, 0.348662, [207, 335, 1925, 1858], 0.612),
            ("a005", 3504, 2336, 0.414834, [251, 343, 3264, 1964], 0.678),
        ]
        for name, width, height, fg, bbox, measured in cases:
            with self.subTest(asset=name):
                plan = geometry.solve(width, height, fg, bbox=bbox)
                self.assertTrue(plan.fixable, name)
                self.assertEqual(plan["anchor"], "bbox", name)
                # 主体紧框必须被完整保留（K07 的失败正是裁掉了主体）
                self.assertEqual(plan["crop_box"], bbox, name)
                self.assertGreaterEqual(plan["predicted_ratio"], 0.55, name)
                self.assertLessEqual(plan["predicted_ratio"], 0.85, name)
                self.assertGreaterEqual(plan["predicted_side"], 1600, name)
                # 闭式解预测（不补白口径）vs 真裁真测：误差 ≤0.03
                # （模型对取景变化的响应，不是实现口径差）
                self.assertAlmostEqual(plan["predicted_ratio_aspect"], measured,
                                       delta=0.03, msg=name)


if __name__ == "__main__":
    unittest.main()
