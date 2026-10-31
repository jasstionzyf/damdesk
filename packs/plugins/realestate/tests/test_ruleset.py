"""规则集/阈值/纯判定函数的单测（不启进程、不碰网关）。"""
from __future__ import annotations

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                "plugin"))

import imagedec          # noqa: E402
import ruleset           # noqa: E402

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")


def _luma_of(name):
    return imagedec.decode_luma(os.path.join(FIXTURES, name))


class LaplacianTest(unittest.TestCase):
    def test_uniform_is_zero(self):
        luma = bytearray([128] * (32 * 32))
        variance, pixels = ruleset.laplacian_variance(luma, 32, 32)
        self.assertEqual(variance, 0.0)
        self.assertEqual(pixels, 30 * 30)

    def test_exact_checkerboard(self):
        # 4x4 棋盘：内部 2x2 共 4 个像素，拉普拉斯值 ±1020 → 方差 = 1020²
        width = height = 4
        luma = bytearray(width * height)
        for y in range(height):
            for x in range(width):
                luma[y * width + x] = 0 if (x + y) % 2 else 255
        variance, pixels = ruleset.laplacian_variance(luma, width, height)
        self.assertEqual(pixels, 4)
        self.assertAlmostEqual(variance, 1020.0 ** 2, places=6)

    def test_too_small_raises(self):
        with self.assertRaises(ValueError):
            ruleset.laplacian_variance(bytearray(4), 2, 2)

    def test_blur_below_sharp(self):
        sw, sh, sharp = _luma_of("sharp_640x480.jpg")
        bw, bh, blur = _luma_of("blur_640x480.jpg")
        sharp_var, _ = ruleset.laplacian_variance(sharp, sw, sh)
        blur_var, _ = ruleset.laplacian_variance(blur, bw, bh)
        self.assertGreater(sharp_var, 100.0)
        self.assertLess(blur_var, 100.0)
        self.assertGreater(sharp_var, blur_var * 100)


class ExposureTest(unittest.TestCase):
    def test_boundaries_are_dead_zone(self):
        stats = ruleset.exposure_stats(bytearray([10] * 50 + [245] * 50))
        self.assertAlmostEqual(stats["dead_black_ratio"], 0.5)
        self.assertAlmostEqual(stats["dead_white_ratio"], 0.5)
        self.assertAlmostEqual(stats["dead_ratio"], 1.0)

    def test_mean_and_pass(self):
        stats = ruleset.exposure_stats(bytearray([120] * 10))
        verdict = ruleset.eval_exposure(stats)
        self.assertEqual(verdict["status"], "pass")
        self.assertAlmostEqual(verdict["evidence"]["mean"], 120.0)

    def test_too_dark_fails(self):
        stats = ruleset.exposure_stats(bytearray([5] * 10))
        verdict = ruleset.eval_exposure(stats)
        self.assertEqual(verdict["status"], "fail")
        # 全黑图同时踩「死区占比」与「均值过暗」→ 死区优先报（信息量更大）
        self.assertEqual(verdict["note"], "dead_zone_ratio_high")

    def test_dark_but_not_dead_fails_on_mean(self):
        stats = ruleset.exposure_stats(bytearray([20] * 10))
        verdict = ruleset.eval_exposure(stats)
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "mean_luma_too_dark")


class PhashTest(unittest.TestCase):
    def _hash(self, name):
        width, height, plane = imagedec.decode_dc_luma(os.path.join(FIXTURES, name))
        return ruleset.phash(plane, width, height)

    def test_near_duplicate_is_close(self):
        distance = ruleset.hamming(self._hash("dup_a_640x480.jpg"),
                                   self._hash("dup_b_640x480.jpg"))
        self.assertLessEqual(distance, ruleset.THRESHOLDS["F3_duplicate"]["threshold"])

    def test_distinct_is_far(self):
        distance = ruleset.hamming(self._hash("dup_a_640x480.jpg"),
                                   self._hash("sharp_640x480.jpg"))
        self.assertGreater(distance, 8)

    def test_hamming(self):
        self.assertEqual(ruleset.hamming(0, 0), 0)
        self.assertEqual(ruleset.hamming(0, 0b1011), 3)


class VerdictTest(unittest.TestCase):
    def test_seven_fields(self):
        verdict = ruleset.verdict("F1_sharpness", "pass", score=1.0, label="x")
        self.assertEqual(set(verdict), {"ruleId", "status", "score", "label",
                                        "evidence", "fixed", "note"})

    def test_unknown_answer_is_undetermined(self):
        verdict = ruleset.eval_domain({"raw": "", "undetermined": False})
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_answer_unparsed")

    def test_vocabulary_answers(self):
        self.assertEqual(ruleset.eval_domain({"raw": "是"})["status"], "pass")
        self.assertEqual(ruleset.eval_domain({"raw": "否"})["status"], "fail")

    def test_negative_phrase_not_read_as_positive(self):
        # 「不是」里含「是」——必须先判否定再判肯定
        self.assertEqual(ruleset.eval_domain({"raw": "不是房源图"})["status"], "fail")

    def test_duplicate_is_soft_marked(self):
        verdict = ruleset.eval_duplicate(3)
        self.assertEqual(verdict["status"], "fail")
        self.assertTrue(verdict["evidence"]["soft_rule"])
        self.assertIn("仅标记", verdict["note"])

    def test_scene_complete_and_missing(self):
        complete = {"a": "客厅", "b": "卧室", "c": "厨房", "d": "卫生间"}
        self.assertEqual(ruleset.eval_scene_complete(complete)["status"], "pass")
        partial = {"a": "客厅", "b": "卧室"}
        verdict = ruleset.eval_scene_complete(partial)
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(sorted(verdict["evidence"]["missing"]), ["卫生间", "厨房"])

    def test_scene_unclassified_is_not_fail(self):
        verdict = ruleset.eval_scene_complete({"a": "", "b": ""})
        self.assertEqual(verdict["status"], "undetermined")

    def test_needs_human(self):
        verdict = ruleset.needs_human_verdict("F1_sharpness", "below_sharpness_min")
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["label"], "manual_review_required")
        self.assertTrue(verdict["note"].startswith("needs_human:"))


class ResourceSyncTest(unittest.TestCase):
    """resources/*.json 是人读副本：必须与代码里的单一定义点逐项一致。"""

    def test_rules_json_matches_ruleset(self):
        path = os.path.join(os.path.dirname(FIXTURES), "..", "resources", "rules.json")
        payload = json.load(open(os.path.normpath(path), encoding="utf-8"))
        self.assertEqual([r["id"] for r in payload["rules"]], ruleset.RULE_IDS)
        self.assertEqual(payload["pipeline_order"], ruleset.PIPELINE_ORDER)
        self.assertEqual(payload["fix_policy"], ruleset.FIX_POLICY)
        self.assertEqual(payload["max_rounds"], ruleset.MAX_ROUNDS)
        for rule_id, thresholds in ruleset.THRESHOLDS.items():
            self.assertEqual(payload["thresholds"][rule_id], thresholds)

    def test_scenes_json_matches(self):
        path = os.path.normpath(os.path.join(os.path.dirname(FIXTURES), "..",
                                            "resources", "scenes.json"))
        payload = json.load(open(path, encoding="utf-8"))
        self.assertEqual(payload["scene_options"], ruleset.SCENE_OPTIONS)
        self.assertEqual(payload["require_all"], ruleset.REQUIRE_ALL_SCENES)


if __name__ == "__main__":
    unittest.main()
