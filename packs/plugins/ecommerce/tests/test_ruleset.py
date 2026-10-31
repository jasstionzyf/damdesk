"""规则语义单测：阈值边界、VL 应答映射（含 undetermined 不冒充）、资源文件同步。"""
from __future__ import annotations

import json
import os
import unittest

import _support

import ruleset

PACK_DIR = _support.PACK_DIR


class SizeRuleTest(unittest.TestCase):
    def test_boundary(self):
        self.assertEqual(ruleset.eval_size(2400, 1600)["status"], "pass")
        self.assertEqual(ruleset.eval_size(1600, 1600)["status"], "pass")
        self.assertEqual(ruleset.eval_size(1599, 2400)["status"], "fail")
        self.assertEqual(ruleset.eval_size(2400, 1599)["status"], "fail")    # 短边为准

    def test_missing_size_is_undetermined_not_fail(self):
        """量不出尺寸 = 前提不成立 → undetermined（不是「尺寸不达标」）。卡 task。"""
        for args in ((0, 0), (2400, 0), (None, 1600), ("", 1600)):
            v = ruleset.eval_size(*args)
            self.assertEqual(v["status"], "undetermined", args)
            self.assertIsNone(v["score"], args)
            self.assertEqual(v["note"], "no_image_size", args)

    def test_score_is_short_side_ratio(self):
        verdict = ruleset.eval_size(800, 2400)
        self.assertAlmostEqual(verdict["score"], 0.5)
        self.assertEqual(verdict["evidence"]["short_side"], 800)
        self.assertEqual(verdict["note"], "below_short_side")
        self.assertEqual(ruleset.eval_size(1600, 1600)["score"], 1.0)

    def test_threshold_override(self):
        th = ruleset.thresholds_for("R1_size", {"R1_size": {"short_side_min": 700}})
        self.assertEqual(th["short_side_min"], 700)
        self.assertEqual(ruleset.eval_size(800, 800,
                                           short_side_min=th["short_side_min"])["status"],
                         "pass")

    def test_long_side_threshold_defaults_to_off(self):
        """长边门槛默认 0 = 不约束（淘宝档历史口径）：700×700 在短边 700 下 pass。"""
        self.assertEqual(ruleset.THRESHOLDS["R1_size"]["long_side_min"], 0)
        self.assertEqual(ruleset.eval_size(700, 700, short_side_min=700)["status"], "pass")

    def test_long_side_threshold_from_spec(self):
        """亚马逊规格显式给长边 1600：短边够、长边不够 → fail，note 指长边。"""
        v = ruleset.eval_size(1200, 800, short_side_min=500, long_side_min=1600)
        self.assertEqual(v["status"], "fail")
        self.assertEqual(v["note"], "below_long_side")
        self.assertEqual(v["evidence"]["long_side"], 1200)
        self.assertEqual(v["evidence"]["long_side_threshold"], 1600)
        # 两条都够才 pass；长边够短边不够 → below_short_side（短边口径优先）
        self.assertEqual(ruleset.eval_size(1600, 1600, short_side_min=500,
                                           long_side_min=1600)["status"], "pass")
        self.assertEqual(ruleset.eval_size(1500, 1600, short_side_min=1600,
                                           long_side_min=1600)["note"], "below_short_side")


class SubjectRatioRuleTest(unittest.TestCase):
    def test_window_edges(self):
        cases = [(0.549, "fail"), (0.55, "pass"), (0.75, "pass"),
                 (0.85, "pass"), (0.851, "fail")]
        for ratio, expected in cases:
            verdict = ruleset.eval_subject_ratio(ratio)
            self.assertEqual(verdict["status"], expected, ratio)
        self.assertEqual(ruleset.eval_subject_ratio(0.4)["note"], "below_min_ratio")
        self.assertEqual(ruleset.eval_subject_ratio(0.9)["note"], "above_max_ratio")

    def test_score_keeps_full_precision(self):
        # 与旧引擎基线对账要靠 score 逐值相等，不能在这里四舍五入
        verdict = ruleset.eval_subject_ratio(0.5680348712345)
        self.assertEqual(verdict["score"], 0.5680348712345)

    def test_legacy_caliber_is_supported(self):
        """旧引擎 main 上仍是 0.65：给覆盖口子，不靠改包重发。"""
        th = ruleset.thresholds_for("R4_subject_ratio", {"R4_subject_ratio": {"min": 0.65}})
        self.assertEqual(ruleset.eval_subject_ratio(0.62, minimum=th["min"])["status"],
                         "fail")
        self.assertEqual(ruleset.eval_subject_ratio(0.62)["status"], "pass")


class WhiteEdgeRuleTest(unittest.TestCase):
    def test_boundary(self):
        self.assertEqual(ruleset.eval_white_edge(8)["status"], "pass")
        self.assertEqual(ruleset.eval_white_edge(8.1)["status"], "fail")
        self.assertEqual(ruleset.eval_white_edge(167.5)["score"], 167.5)


class AnswerMappingTest(unittest.TestCase):
    def test_structured_reply_wins(self):
        reply = {"undetermined": False, "raw": "随便写",
                 "rules": [{"status": "fail", "label": "not_apparel", "score": 0.31}]}
        verdict = ruleset.eval_answer(reply, expect_yes=True,
                                      rule_id="R0_product_main", label="domain_gate")
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["score"], 0.31)

    def test_text_answer_mapping(self):
        for raw, expect_yes, expected in (("是", True, "pass"), ("否", True, "fail"),
                                          ("否", False, "pass"), ("是", False, "fail"),
                                          ("Yes", True, "pass"), ("No", True, "fail")):
            verdict = ruleset.eval_answer({"raw": raw, "undetermined": False},
                                          expect_yes=expect_yes,
                                          rule_id="R3_no_watermark", label="overlay")
            self.assertEqual(verdict["status"], expected, (raw, expect_yes))

    def test_undetermined_never_promoted(self):
        verdict = ruleset.eval_answer({"undetermined": True, "reason": "vl_parse_failed",
                                       "raw": ""}, expect_yes=True,
                                      rule_id="R0_product_main", label="domain_gate")
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_undetermined")
        self.assertIsNone(verdict["score"])

    def test_unparsable_is_undetermined_not_pass(self):
        verdict = ruleset.eval_answer({"undetermined": False, "raw": "看不清"},
                                      expect_yes=True,
                                      rule_id="R0_product_main", label="domain_gate")
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_answer_unparsed")

    def test_answer_echo_is_ignored(self):
        """问句回声（「是否有水印：否」）不能把答案带偏：回声词先剥掉再判。"""
        for raw, expected in (("是否有水印：否", "pass"), ("是否有水印：是", "fail"),
                              ("不是商品主图", "fail"), ("不符合白底要求", "fail"),
                              ("无法判断", "undetermined")):
            verdict = ruleset.eval_answer({"undetermined": False, "raw": raw},
                                          expect_yes=False if "水印" in raw else True,
                                          rule_id="R3_no_watermark", label="overlay")
            self.assertEqual(verdict["status"], expected, raw)


class TextOverlayRuleTest(unittest.TestCase):
    """R3 纯判定函数（用户裁决 D1=A）：文字区性质 + 面积占比，OCR 只做定位。"""

    W, H = 2000, 2000
    SUBJECT = [400, 400, 1600, 1600]        # 1200² 主体

    def classify(self, boxes, subject=SUBJECT):
        return ruleset.classify_text_overlay(boxes, width=self.W, height=self.H,
                                             subject_bbox=subject)

    def test_no_text(self):
        decision = self.classify([])
        self.assertEqual(decision["action"], "pass")
        self.assertEqual(decision["note"], "no_text")
        self.assertEqual(decision["metrics"]["lines"], 0)

    def test_subject_label_is_excluded(self):
        decision = self.classify([[700, 900, 950, 960], [1000, 900, 1200, 950]])
        self.assertEqual(decision["action"], "pass")
        self.assertEqual(decision["note"], "subject_label_text")
        self.assertEqual(len(decision["regions"]["labels"]), 2)
        self.assertEqual(decision["regions"]["outside"], [])
        self.assertEqual(decision["regions"]["watermark"], [])

    def test_bar_outside_subject_fails(self):
        decision = self.classify([[400, 100, 1600, 300]])          # 5% 面积、中心在主体外
        self.assertEqual(decision["action"], "fail")
        self.assertEqual(decision["note"], "promo_text_area")
        self.assertAlmostEqual(decision["metrics"]["outside_area_ratio"], 0.06, places=3)

    def test_text_covering_subject_fails(self):
        decision = self.classify([[500, 800, 1700, 1100]])         # 覆盖主体 25%
        self.assertEqual(decision["action"], "fail")
        self.assertEqual(decision["note"], "text_covers_subject")
        self.assertEqual(len(decision["regions"]["watermark"]), 1)

    def test_small_outside_text_goes_to_vl(self):
        decision = self.classify([[0, 0, 200, 100]])               # 0.5% < 2%
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["note"], "small_outside_text")

    def test_area_threshold_boundary(self):
        self.assertEqual(self.classify([[0, 0, 400, 200]])["note"], "promo_text_area")
        self.assertEqual(self.classify([[0, 0, 398, 200]])["action"], "vl")

    def test_cover_threshold_boundary(self):
        """覆盖主体 8% 是水印门槛（含）；7.9% 且中心在主体内 → 吊牌。"""
        subject_area = 1200 * 1200
        on_subject = 0.08 * subject_area                        # 正好 8%
        width = int(round(on_subject / 200))                    # 高 200 的横条
        decision = self.classify([[500, 800, 500 + width, 1000]])
        self.assertEqual(decision["note"], "text_covers_subject")
        narrower = width - 4                                    # 略低于 8%
        decision = self.classify([[500, 800, 500 + narrower, 1000]])
        self.assertEqual(decision["note"], "subject_label_text")

    def test_invalid_boxes_are_ignored(self):
        decision = self.classify([[0, 0, 0, 0], [5, 5, 5, 5], None])
        self.assertEqual(decision, self.classify([]))

    def test_overlay_boxes_excludes_subject_labels(self):
        boxes = [[700, 900, 950, 960], [0, 0, 400, 200]]
        self.assertEqual(ruleset.overlay_boxes(boxes, width=self.W, height=self.H,
                                               subject_bbox=self.SUBJECT),
                         [[0, 0, 400, 200]])

    def test_verdict_shape_for_deterministic_outcomes(self):
        decision = self.classify([[0, 0, 400, 200]])
        verdict = ruleset.text_overlay_verdict(decision)
        self.assertEqual(set(verdict), {"ruleId", "status", "score", "label",
                                        "evidence", "fixed", "note"})
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["ruleId"], "R3_no_watermark")
        self.assertEqual(verdict["evidence"]["source"], "ocr+subject_bbox")

    def test_single_char_boxes_are_noise(self):
        """真实语料 a002 实测：裙面花纹上给出「P」「0」两只大框（占画面 42%），不是文字。

        若按「覆盖主体 ≥8%」判水印，干净商品图会被判 fail —— 所以单字符/空文本行先当噪声剔除。
        """
        lines = [{"text": "P", "box": [0, 446, 697, 1159], "score": 0.587},
                 {"text": "0", "box": [339, 739, 2028, 1680], "score": 0.604}]
        decision = ruleset.classify_text_overlay(lines, width=2560, height=1920,
                                                 subject_bbox=[88, 318, 2472, 1602])
        self.assertEqual(decision["action"], "pass")
        self.assertEqual(decision["note"], "no_text")
        self.assertEqual(decision["metrics"]["noise_lines"], 2)
        self.assertEqual(decision["metrics"]["lines"], 0)
        self.assertEqual(ruleset.overlay_boxes(lines, width=2560, height=1920,
                                               subject_bbox=[88, 318, 2472, 1602]), [])

    def test_label_text_with_content_stays_label(self):
        lines = [{"text": "SAMSUNG", "box": [1957, 1210, 2105, 1261], "score": 1.0}]
        decision = ruleset.classify_text_overlay(lines, width=3221, height=2530,
                                                 subject_bbox=[362, 134, 3091, 2396])
        self.assertEqual(decision["note"], "subject_label_text")
        self.assertEqual(decision["metrics"]["noise_lines"], 0)

    def test_empty_text_line_is_noise(self):
        lines = [{"text": "", "box": [0, 0, 400, 200], "score": 0.0}]
        decision = ruleset.classify_text_overlay(lines, width=self.W, height=self.H,
                                                 subject_bbox=self.SUBJECT)
        self.assertEqual(decision["action"], "pass")
        self.assertEqual(decision["metrics"]["noise_lines"], 1)

    # --- 多实例主体（K07 f002 咖啡胶囊，`r3_probe` 实测几何） -------------------
    # 3840×2564 的图：主玻璃杯 dominant_bbox=[1104,56,3492,2428]，第二件商品（胶囊簇）
    # instances[1]=[468,1306,1296,1931]；三行 OCR（"an"/"NEEN"/"DO00W"）中心都落在胶囊簇里，
    # 只认主主体时它们合计 3.39% ≥ 2% → 误判促销条（f002 实际被误判就是这个口径）。
    F002 = dict(width=3840, height=2564, dominant=[1104, 56, 3492, 2428],
                instances=[[1104, 56, 3492, 2428], [468, 1306, 1296, 1931]])
    F002_LINES = [{"text": "an", "box": [630, 1368, 1106, 1808], "score": 0.343},
                  {"text": "NEEN", "box": [597, 1516, 883, 1822], "score": 0.668},
                  {"text": "DO00W", "box": [917, 1635, 1124, 1814], "score": 0.559}]

    def f002(self, **kw):
        return ruleset.classify_text_overlay(
            self.F002_LINES, width=self.F002["width"], height=self.F002["height"],
            subject_bbox=self.F002["dominant"], **kw)

    def test_second_product_text_is_label_not_promo_bar(self):
        """第二件商品的自带文字：单主体口径误判 fail → 多实例口径 pass（f002 回归点）。"""
        single = self.f002()
        self.assertEqual(single["action"], "fail")
        self.assertEqual(single["note"], "promo_text_area")
        self.assertAlmostEqual(single["metrics"]["outside_area_ratio"], 0.0339, places=3)

        multi = self.f002(subject_boxes=self.F002["instances"])
        self.assertEqual(multi["action"], "pass")
        self.assertEqual(multi["note"], "subject_label_text")
        self.assertEqual(multi["metrics"]["outside_area_ratio"], 0.0)
        self.assertEqual(multi["metrics"]["subject_count"], 2)
        self.assertEqual(len(multi["regions"]["labels"]), 3)
        self.assertEqual(multi["regions"]["outside"], [])
        # 证据带齐实例框：能按「文字落在哪块商品上」逐行复现
        verdict = ruleset.text_overlay_verdict(multi)
        self.assertEqual(verdict["evidence"]["subject_count"], 2)
        self.assertEqual(verdict["evidence"]["subject_boxes"], self.F002["instances"])
        # 修复口径同源：商品自带文字不许被 inpaint 抹掉
        self.assertEqual(ruleset.overlay_boxes(
            self.F002_LINES, width=self.F002["width"], height=self.F002["height"],
            subject_bbox=self.F002["dominant"],
            subject_boxes=self.F002["instances"]), [])

    def test_promo_bar_outside_all_instances_still_fails(self):
        """真促销条在两块商品之外（底部通栏 6.2%）→ 面积 ≥2% 仍判 fail，多实例不放水。"""
        lines = [{"text": "限时 5 折", "box": [0, 2400, 3840, 2560], "score": 0.9}]
        decision = ruleset.classify_text_overlay(
            lines, width=self.F002["width"], height=self.F002["height"],
            subject_bbox=self.F002["dominant"], subject_boxes=self.F002["instances"])
        self.assertEqual(decision["action"], "fail")
        self.assertEqual(decision["note"], "promo_text_area")
        self.assertEqual(len(decision["regions"]["outside"]), 1)

    def test_small_promo_text_outside_all_instances_goes_to_vl(self):
        """面积 <2% 且不在任何实例内 → 仍是 VL 裁量档（不被多实例吞成 pass）。"""
        lines = [{"text": "限时", "box": [100, 100, 300, 160], "score": 0.9}]
        decision = ruleset.classify_text_overlay(
            lines, width=self.F002["width"], height=self.F002["height"],
            subject_bbox=self.F002["dominant"], subject_boxes=self.F002["instances"])
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["note"], "small_outside_text")

    def test_watermark_is_judged_against_dominant_subject_only(self):
        """水印判据只用主主体：压在主商品上的大字条（22.6%）仍判水印，不受小实例分母影响。"""
        lines = [{"text": "官方旗舰店", "box": [1200, 600, 2800, 1400], "score": 0.9}]
        decision = ruleset.classify_text_overlay(
            lines, width=self.F002["width"], height=self.F002["height"],
            subject_bbox=self.F002["dominant"], subject_boxes=self.F002["instances"])
        self.assertEqual(decision["action"], "fail")
        self.assertEqual(decision["note"], "text_covers_subject")
        self.assertEqual(len(decision["regions"]["watermark"]), 1)

    def test_empty_or_missing_instances_fall_back_to_single_subject(self):
        """老宿主（无 instances）与空数组行为一致：退回单主体口径，老结论不变。"""
        base = self.f002()
        self.assertEqual(self.f002(subject_boxes=[]), base)
        self.assertEqual(self.f002(subject_boxes=[None, [0, 0, 0, 0]]), base)
        # 宿主给 dict 形式（`{"bbox": [...]}`）也认
        as_dict = self.f002(subject_boxes=[{"bbox": b} for b in self.F002["instances"]])
        self.assertEqual(as_dict["action"], "pass")
        self.assertEqual(as_dict["metrics"]["subject_count"], 2)


class DomainPromptTest(unittest.TestCase):
    """R0 判据口径（K07-FIX-I 用户裁决）：可售、完整、单一的商品主体。

    f001「笔记本 + 咖啡杯」生活场景图不是商品主图 —— 三项硬否决（多主体/被裁切/场景陪衬）
    必须写进 prompt（模型给事实）**并且**写进 schema 参数（代码执行判据）。
    """

    def test_prompt_asks_for_structured_facts(self):
        prompt = ruleset.domain_prompt()
        self.assertIn("商品主图", prompt)
        for token in ("subject", "others", "verdict", "area_ratio", "cut"):
            self.assertIn(token, prompt, token)

    def test_prompt_states_the_three_vetoes(self):
        prompt = ruleset.domain_prompt()
        for token in ("多主体", "裁切", "生活场景"):
            self.assertIn(token, prompt, token)
        # 数字来自 schema 参数，prompt 里出现的是同一组：改阈值不会漏改 prompt
        th = ruleset.THRESHOLDS["R0_product_main"]
        self.assertIn("%g%%" % (th["foreign_object_ratio_max"] * 100), prompt)   # 30%
        self.assertIn("%g%%" % (th["edge_margin_ratio"] * 100), prompt)          # 2%
        self.assertIn("上限 %d" % th["scene_prop_max"], prompt)

    def test_prompt_keeps_non_veto_cases_out_of_the_gate(self):
        prompt = ruleset.domain_prompt()
        self.assertIn("不构成否决", prompt)
        for token in ("白底", "促销文字", "吊牌", "模特"):
            self.assertIn(token, prompt, token)

    def test_scene_and_prop_shots_are_no_longer_auto_products(self):
        """场景摆拍/道具搭配**不再**一律 pass（f001 假阳性的根因）。"""
        prompt = ruleset.domain_prompt()
        for token in ("桌面", "餐具饮具", "家具", "装饰摆件"):
            self.assertIn(token, prompt, token)
        self.assertNotIn("场景摆拍图都算「是」", prompt)


class DomainGateTest(unittest.TestCase):
    """R0 确定性前置（K07-FIX-H）：面积/填充度决定，只有边界区间才问 VL。"""

    def classify(self, ratio, **kwargs):
        return ruleset.classify_domain(ratio, **kwargs)

    def test_large_compact_subject_passes_deterministically(self):
        decision = self.classify(0.42, bbox_area_ratio=0.60)
        self.assertEqual(decision["action"], "pass")
        self.assertEqual(decision["note"], "subject_area")
        self.assertAlmostEqual(decision["metrics"]["solidity"], 0.7, places=6)

    def test_absent_subject_is_not_applicable(self):
        """画面里没有可判主体 → `skip`（本包对该图不适用），不是 `fail`（卡 task）。"""
        self.assertEqual(self.classify(0.0)["action"], "skip")
        self.assertEqual(self.classify(0.02)["action"], "skip")   # 边界含等号
        self.assertEqual(self.classify(0.02)["note"], "no_subject_area")

    def test_boundary_band_goes_to_vl(self):
        for ratio in (0.021, 0.05, 0.114, 0.149):
            decision = self.classify(ratio)
            self.assertEqual(decision["action"], "vl", ratio)
            self.assertEqual(decision["note"], "boundary_subject_area")

    def test_area_threshold_is_exclusive_at_top(self):
        self.assertEqual(self.classify(0.15)["action"], "pass")
        self.assertEqual(self.classify(0.1499)["action"], "vl")

    def test_diffuse_mask_goes_to_vl(self):
        decision = self.classify(0.20, bbox_area_ratio=1.0)     # 填充度 0.20 < 0.25
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["note"], "diffuse_mask")

    def test_full_frame_mask_on_big_subject_goes_to_vl(self):
        decision = self.classify(0.90, bbox_area_ratio=0.99)    # 整幅显著：多半是场景纹理
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["note"], "full_frame_mask")

    def test_no_measurement_goes_to_vl(self):
        decision = self.classify(None)
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["note"], "no_subject_measurement")

    def test_thresholds_are_overridable(self):
        decision = self.classify(0.30, area_min=0.40)           # 调用方提高门槛 → 交 VL
        self.assertEqual(decision["action"], "vl")
        self.assertEqual(decision["metrics"]["thresholds"]["subject_area_min"], 0.40)

    def test_deterministic_verdict_shape(self):
        verdict = ruleset.domain_gate_verdict(self.classify(0.42, bbox_area_ratio=0.60))
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["evidence"]["source"], "u2net_area")
        self.assertEqual(verdict["evidence"]["decided_by"], "subject_area")
        self.assertEqual(verdict["evidence"]["vl_calls"], 0)
        self.assertIsNone(verdict["note"])

    def test_absent_subject_verdict_is_skipped_without_score(self):
        """skipped 行**不带 score**：不适用不造假分（宿主对缺省 score 会显示 0.000）。"""
        verdict = ruleset.domain_gate_verdict(self.classify(0.0))
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "no_subject_area")
        self.assertIsNone(verdict["score"])
        self.assertEqual(verdict["evidence"]["decided_by"], "subject_absent")
        self.assertIs(verdict["evidence"]["applicable"], False)


class DomainVetoTest(unittest.TestCase):
    """R0 三项硬否决（K07-FIX-I）：判据在代码里执行，数字来自 schema 参数。

    f001 假阳性根因：旧 gate 只问「有没有商品主体」，主体被裁切/旁边摆着大杯子/
    桌面上一拍全都放行。三项否决各自「正例 + 反例」钉在这里。
    """

    SUBJECT = {"bbox": [200, 200, 800, 800], "width": 1000, "height": 1000}   # 边距 20%

    def answer(self, **kwargs):
        body = {"subject": {"present": True, "name": "笔记本电脑", "area_ratio": 0.30,
                            "cut": False},
                "others": [], "verdict": "pass"}
        body.update(kwargs)
        return body

    def evaluate(self, answer, subject=None, thresholds=None):
        return ruleset.eval_domain_answer(answer, subject=subject or self.SUBJECT,
                                          thresholds=thresholds)

    # ① 主体数量：非主体显著物体 ≥ 主体 30% → fail
    def test_foreign_object_over_threshold_fails(self):
        result = self.evaluate(self.answer(others=[
            {"name": "咖啡杯", "kind": "product", "area_ratio": 0.12}]))   # 0.12/0.30 = 40%
        self.assertEqual(result["status"], "fail")
        self.assertTrue(any(r.startswith("foreign_object_too_large")
                            for r in result["reasons"]), result["reasons"])

    def test_small_side_object_passes(self):
        result = self.evaluate(self.answer(others=[
            {"name": "挂绳", "kind": "product", "area_ratio": 0.05}]))     # 0.05/0.30 = 17%
        self.assertEqual(result["status"], "pass")

    # ② 主体完整性：主体框距边 < 2% 短边 → fail（确定性，不需要 VL）
    def test_subject_touching_edge_fails(self):
        subject = {"bbox": [10, 300, 900, 900], "width": 1000, "height": 1000}  # 左边距 1%
        result = self.evaluate(self.answer(), subject=subject)
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["reasons"], ["subject_cut_off"])
        self.assertTrue(result["metrics"]["cut"]["cut"])

    def test_subject_inside_frame_passes(self):
        self.assertEqual(ruleset.subject_cut(self.SUBJECT)["cut"], False)
        self.assertEqual(self.evaluate(self.answer())["status"], "pass")

    def test_vl_cut_claim_also_vetoes(self):
        """模型说主体出画（掩码看不出来时）同样算否决。"""
        result = self.evaluate(self.answer(
            subject={"present": True, "name": "鞋", "area_ratio": 0.3, "cut": True}))
        self.assertEqual(result["status"], "fail")
        self.assertIn("subject_cut_off", result["reasons"])

    # ③ 场景度：生活场景陪衬出现即 fail（scene_prop_max=0）
    def test_scene_prop_present_fails(self):
        result = self.evaluate(self.answer(others=[
            {"name": "桌面", "kind": "life_scene", "area_ratio": 0.01},
            {"name": "咖啡杯", "kind": "life_scene", "area_ratio": 0.02}]))
        self.assertEqual(result["status"], "fail")
        self.assertTrue(any(r.startswith("scene_prop_present")
                            for r in result["reasons"]), result["reasons"])

    def test_no_scene_prop_passes(self):
        result = self.evaluate(self.answer(others=[
            {"name": "挂绳", "kind": "product", "area_ratio": 0.05}]))
        self.assertFalse(any(r.startswith("scene_prop_present")
                             for r in result["reasons"]))

    # 参数来自 schema：同一条应答，改参数就改结论（证明判据没写死在代码里）
    def test_thresholds_come_from_params(self):
        answer = self.answer(others=[{"name": "咖啡杯", "kind": "product",
                                      "area_ratio": 0.40}])     # 0.40/0.30 = 133%
        self.assertEqual(self.evaluate(answer)["status"], "fail")
        relaxed = {"foreign_object_ratio_max": 2.0, "scene_prop_max": 3}
        self.assertEqual(self.evaluate(answer, thresholds=relaxed)["status"], "pass")
        strict = {"edge_margin_ratio": 0.30}                     # 边距 20% < 30% → 裁切
        self.assertEqual(self.evaluate(self.answer(), thresholds=strict)["status"], "fail")

    # 掩码铺满整幅 → 掩码不可信，裁切档不成立（交给 VL 看视觉事实）
    def test_full_frame_mask_skips_edge_veto(self):
        subject = {"bbox": [0, 0, 1000, 1000], "width": 1000, "height": 1000}
        cut = ruleset.subject_cut(subject)
        self.assertFalse(cut["cut"])
        self.assertEqual(cut["note"], "full_frame_mask")
        self.assertEqual(self.evaluate(self.answer(), subject=subject)["status"], "pass")

    def test_unmeasurable_subject_is_not_a_cut(self):
        cut = ruleset.subject_cut(None)
        self.assertFalse(cut["cut"])
        self.assertEqual(cut["note"], "no_subject_measurement")

    def test_unparsed_answer_is_undetermined(self):
        self.assertEqual(self.evaluate(None)["status"], "undetermined")

    def test_veto_verdict_carries_reasons(self):
        result = self.evaluate(self.answer(others=[
            {"name": "桌面", "kind": "life_scene", "area_ratio": 0.30}]))
        verdict = ruleset.domain_veto_verdict(result)
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["evidence"]["decided_by"], "vl_veto")
        self.assertTrue(verdict["evidence"]["reasons"])
        self.assertEqual(verdict["note"], verdict["evidence"]["reasons"][0])

    # ④ 不适用（画面里没有商品主体）→ skipped，不是 fail（卡 task）
    def test_subject_missing_is_skipped_not_failed(self):
        result = self.evaluate(self.answer(
            subject={"present": False, "name": "", "area_ratio": 0.0, "cut": False},
            verdict="fail"))
        self.assertEqual(result["status"], "skipped")
        self.assertEqual(result["reasons"], ["no_product_subject"])
        self.assertIs(result["metrics"]["applicable"], False)

    def test_subject_missing_verdict_has_no_score(self):
        result = self.evaluate(self.answer(
            subject={"present": False, "name": "", "area_ratio": 0.0, "cut": False},
            verdict="fail"))
        verdict = ruleset.domain_veto_verdict(result)
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "no_product_subject")
        self.assertIsNone(verdict["score"])


class FlipGuardTest(unittest.TestCase):
    """多数票（K07-FIX-I 第 2 项）：过半采纳，平票 undetermined；票数进 evidence。"""

    def combine(self, statuses, rule_id="R0_product_main", details=None):
        return ruleset.combine_samples(statuses, rule_id=rule_id, label="domain_gate",
                                       details=details)

    def test_all_pass_is_pass(self):
        verdict = self.combine(["pass", "pass", "pass"])
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["evidence"]["samples"], 3)
        self.assertEqual(verdict["evidence"]["distinct"], ["pass"])
        self.assertEqual(verdict["evidence"]["votes"], {"pass": 3})

    def test_all_fail_is_fail(self):
        verdict = self.combine(["fail"] * 3)
        self.assertEqual(verdict["status"], "fail")

    def test_majority_wins_on_two_to_one(self):
        verdict = self.combine(["pass", "fail", "pass"])
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["note"], "vl_majority_vote")
        self.assertEqual(verdict["evidence"]["votes"], {"pass": 2, "fail": 1})
        self.assertEqual(verdict["evidence"]["vote_margin"], "2/3")

    def test_tie_without_majority_is_undetermined(self):
        verdict = self.combine(["pass", "fail", "undetermined"])
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_no_majority")

    def test_undetermined_sample_is_undetermined(self):
        verdict = self.combine(["undetermined"] * 3)
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_undetermined")

    def test_per_vote_details_are_kept(self):
        details = [{"sample": 0, "status": "fail", "reasons": ["subject_cut_off"]},
                   {"sample": 1, "status": "fail", "reasons": ["scene_prop_present(1>0)"]},
                   {"sample": 2, "status": "pass", "reasons": []}]
        verdict = self.combine(["fail", "fail", "pass"], details=details)
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["evidence"]["sample_verdicts"], details)

    def test_single_sample_is_adopted(self):
        """协议 1.1 宿主只回一份 raw：不因「样本不足」把结论打成 undetermined。"""
        self.assertEqual(self.combine(["fail"])["status"], "fail")

    def test_no_samples_is_undetermined(self):
        verdict = self.combine([])
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_no_sample")

    def test_skipped_majority_stays_skipped(self):
        """「没有商品主体」的多数票 → skipped（不适用），不是 fail（卡 task）。"""
        verdict = self.combine(["skipped", "skipped", "fail"])
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "vl_majority_vote_not_applicable")
        self.assertIsNone(verdict["score"])

    def test_skipped_single_sample_note(self):
        verdict = self.combine(["skipped"])
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "vl_not_applicable")
        self.assertIsNone(verdict["score"])


class PromptVersionTest(unittest.TestCase):
    """prompt 版本化（K07-FIX-H 第 2 项）：id + 正文 sha256 必须可复算、可追溯。"""

    def test_spec_is_stable_and_hashed(self):
        spec = ruleset.prompt_spec("R0_product_main", ruleset.domain_prompt())
        self.assertEqual(spec["id"], "ecom.r0_domain.v3")   # FIX-I：结构化应答 → v3
        self.assertEqual(len(spec["sha256"]), 64)
        self.assertEqual(spec, ruleset.prompt_spec("R0_product_main",
                                                  ruleset.domain_prompt()))   # 幂等
        # 解码参数固定化：temperature=0 / top_p=1 / max_tokens 固定
        self.assertEqual(spec["temperature"], 0)
        self.assertEqual(spec["top_p"], 1.0)
        self.assertEqual(spec["max_tokens"], 4096)

    def test_sha_matches_prompt_text(self):
        import hashlib
        texts = dict(ruleset.PROMPT_TEXTS)
        texts["R0_product_main"] = ruleset.domain_prompt()      # 实际送入的是渲染后的正文
        for rule_id, text in texts.items():
            expect = hashlib.sha256(text.encode("utf-8")).hexdigest()
            spec = ruleset.prompt_spec(rule_id, text)
            self.assertEqual(spec["sha256"], expect, rule_id)

    def test_every_vl_rule_has_a_spec(self):
        for rule in ruleset.RULES:
            if rule["executor"] in ("vl", "ocr+vl", "model_u2net+vl"):
                self.assertIn(rule["id"], ruleset.PROMPT_SPECS, rule["id"])

    def test_evidence_carries_identity(self):
        evidence = ruleset.prompt_evidence("R3_no_watermark", max_tokens=4096)
        self.assertEqual(evidence["prompt_id"], "ecom.r3_no_overlay.v1")
        self.assertEqual(evidence["temperature"], 0)
        self.assertEqual(evidence["top_p"], 1.0)
        self.assertEqual(evidence["max_tokens"], 4096)

    def test_schema_prompts_are_the_effective_ones(self):
        """prompt 正文/版本以 resources/rules.json 为准（pack 可版本化字段）。"""
        data = ruleset.load_schema()
        for rule_id in ruleset.PROMPT_SPECS:
            declared = data["prompts"][rule_id]
            self.assertEqual(declared["version"], ruleset.PROMPT_SPECS[rule_id]["version"])
            self.assertEqual(declared["id"], ruleset.PROMPT_SPECS[rule_id]["id"])
            self.assertEqual(declared["text"], ruleset.PROMPT_TEXTS[rule_id])
            self.assertEqual(declared["temperature"], ruleset.VL_TEMPERATURE)
            self.assertEqual(declared["top_p"], ruleset.VL_TOP_P)
            self.assertEqual(declared["samples"], ruleset.VL_SAMPLES)


class RuleSetShapeTest(unittest.TestCase):
    def test_ids_and_order(self):
        self.assertEqual(len(ruleset.RULE_IDS), 6)
        self.assertEqual(ruleset.PIPELINE_ORDER[0], "R0_product_main")
        self.assertEqual(set(ruleset.PIPELINE_ORDER), set(ruleset.RULE_IDS))

    def test_every_verdict_has_seven_fields(self):
        verdict = ruleset.verdict("R1_size", "pass", score=1.0, label="short_side")
        self.assertEqual(set(verdict), {"ruleId", "status", "score", "label",
                                        "evidence", "fixed", "note"})

    def test_resources_in_sync(self):
        """resources/rules.json 是**判据权威**：参数与代码默认值必须逐键一致（防漂移）。

        运行时读资源（`THRESHOLDS` 由 `rules[].params` 覆盖而来），代码里的 DEFAULT_* 是兜底；
        两边一旦不一致，说明有人只改了一处 —— 单测必须红。
        """
        with open(os.path.join(PACK_DIR, "resources", "rules.json"),
                  encoding="utf-8") as fh:
            data = json.load(fh)
        self.assertEqual(data["fix_policy"], ruleset.FIX_POLICY)
        self.assertEqual(data["max_rounds"], ruleset.MAX_ROUNDS)
        self.assertEqual([r["id"] for r in data["rules"]], ruleset.RULE_IDS)
        self.assertEqual(data["pipeline_order"], ruleset.PIPELINE_ORDER)
        for rule in data["rules"]:
            declared = {k: rule[k] for k in ("id", "deterministic")}
            actual = next(r for r in ruleset.RULES if r["id"] == rule["id"])
            self.assertEqual(declared["id"], actual["id"])
            # i18n-F：资源里的 name 可写对象形态（{"zh-Hans":…,"en-US":…}）；
            # 与代码单一定义点对齐的口径是 zh-Hans 文案（en 只进展示层，不进代码默认值）
            name = rule["name"]
            if isinstance(name, dict):
                name = name.get("zh-Hans") or next(iter(name.values()))
            self.assertEqual(name, actual["name"])
            self.assertEqual(declared["deterministic"], actual["deterministic"])
        for rule in data["rules"]:
            rule_id = rule["id"]
            if rule_id not in ruleset.DEFAULT_THRESHOLDS:
                self.assertNotIn("params", rule, rule_id)     # 无阈值规则不声明空参数
                continue
            self.assertEqual(rule["params"], ruleset.DEFAULT_THRESHOLDS[rule_id], rule_id)
            self.assertEqual(ruleset.THRESHOLDS[rule_id],
                             ruleset.DEFAULT_THRESHOLDS[rule_id], rule_id)

    def test_manifest_matches_pack_info_expectations(self):
        with open(os.path.join(PACK_DIR, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        self.assertEqual(manifest["api_version"], "1.1")
        self.assertEqual(set(manifest["capabilities"]),
                         {"io.read", "detect.u2net", "detect.ocr", "detect.white",
                          "op.geometric", "op.generative", "vl.judge", "report.emit"})
        for resource in manifest["resources"]:
            self.assertTrue(os.path.isfile(os.path.join(PACK_DIR, resource)), resource)
        self.assertTrue(os.path.isfile(os.path.join(
            PACK_DIR, manifest["entrypoint"]["args"][0])))


if __name__ == "__main__":
    unittest.main()
