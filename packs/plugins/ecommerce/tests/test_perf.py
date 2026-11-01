"""K07-FIX-D 单测：R2 走 classic（不占 VL）、修复后**增量复判**、收尾只补最小集。

三条语义（对应卡面 1/2 项）：
1. 宿主能算边缘白度时，R2 **一次 VL 都不问**（`/io/read` + `measure:["white"]`）；
2. repair 第 2 轮起只复判「被上一轮改动波及」的规则，R0/R3 这类没被动的规则
   不再重复烧 VL（旧实现每轮全量 → 3 轮 × 2 次 VL = 6 次，本卡压到 2 次）；
3. 收尾只补「还没在最新版本上判过」+「最近一次仍 fail」的规则；几何无解的规则
   （`unfixable`）不再重问 —— 省下来的调用是真钱。
"""
from __future__ import annotations

import os
import unittest

import _support

import ruleset
from behavorkit import FakeImageGateway, Image

PLUGIN = _support.load_plugin()


def _vl_prompts(gw, needle):
    """某个 prompt 片段被问过几次（按端点 + 请求体统计，不看次数计数器）。"""
    return [body for (path, body) in gw.requests
            if path == "/vl/judge" and needle in str(body.get("prompt") or "")]


class Base(unittest.TestCase):
    def setUp(self):
        self.root = _support.tempdir("pack-perf-")
        self.workdir = os.path.join(self.root, "work")
        os.makedirs(self.workdir, exist_ok=True)
        self._orig_url = PLUGIN._GATEWAY_URL
        self._orig_token = PLUGIN._GATEWAY_TOKEN
        PLUGIN._CANCEL.clear()
        PLUGIN._REPORTS.clear()

    def tearDown(self):
        PLUGIN._GATEWAY_URL = self._orig_url
        PLUGIN._GATEWAY_TOKEN = self._orig_token
        PLUGIN._CANCEL.clear()

    def gateway(self, image, **kwargs):
        gw = FakeImageGateway(self.root, image, **kwargs).start()
        PLUGIN._GATEWAY_URL = gw.base_url
        PLUGIN._GATEWAY_TOKEN = gw.token
        self.addCleanup(gw.stop)
        self.asset = gw.seed_asset("asset.png")
        return gw

    def judge(self, **options):
        return PLUGIN.judge("job-p", {"id": "asset", "path": self.asset}, None,
                            dict(options, workdir=self.workdir))

    def repair(self, **options):
        return PLUGIN.repair("job-p", {"id": "asset", "path": self.asset}, None,
                             dict(options, workdir=self.workdir))

    def verdicts(self, result):
        return {v["ruleId"]: v for v in result["verdicts"]}


class R2ClassicTest(Base):
    """卡面第 1 项：R2 白底判定走 classic，不再有 VL 往返。"""

    def test_judge_white_uses_io_read_measure_and_never_asks_vl(self):
        gw = self.gateway(Image(2000, 2000, (700, 700, 1300, 1300)), white_dev=3.0)
        verdicts = self.verdicts(self.judge())
        self.assertEqual(verdicts["R2_white_bg"]["status"], "pass")
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["source"],
                         "classic_white_edge")
        # 测量口径随 evidence 一起回：band 决定「外圈几像素」参与均值、worst 是最坏单像素偏差，
        # 缺了这两项就没有办法从结论复现这次统计（协议 1.1 的 io/read measure 同源给出）
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["band"], 4)
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["white_worst_dev"], 3.0)
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["white_dev"], 3.0)
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["threshold"], 8.0)
        # 白底问题一次都没问到 VL
        self.assertEqual(_vl_prompts(gw, ruleset.PROMPT_WHITE_BG), [])
        # 而且 io/read 是带 measure 要的（宿主据此算 classic white_edge）
        io_bodies = [body for (path, body) in gw.requests if path == "/io/read"]
        self.assertTrue(io_bodies)
        self.assertEqual(io_bodies[0].get("measure"), ["white"])

    def test_old_gateway_without_white_dev_still_falls_back_to_vl(self):
        """老网关（协议 1.0：不认 `measure`、也没有 `/detect/white_stats` 端点）→ 必须退回 VL。

        「不认 measure」不等于「只能退 VL」：1.1 宿主有 white_stats 端点，即使 io/read
        没回填 white_dev 也该走 classic。真正的老网关两样都没有 —— 桩要把这两条能力一起关掉，
        否则测的是「插件在 1.1 宿主上会不会乱走」，不是老宿主降级。
        """
        gw = self.gateway(Image(2000, 2000, (700, 700, 1300, 1300)), white_bg="fail",
                          supports_measure=False, white_stats_available=False)
        verdicts = self.verdicts(self.judge())
        self.assertEqual(verdicts["R2_white_bg"]["status"], "fail")
        self.assertNotEqual(verdicts["R2_white_bg"]["evidence"].get("source"),
                            "classic_white_edge")
        self.assertEqual(verdicts["R2_white_bg"]["evidence"]["source"],
                         "vl_fallback_no_white_stats")
        self.assertTrue(_vl_prompts(gw, ruleset.PROMPT_WHITE_BG))


class IncrementalRecheckTest(Base):
    """卡面第 2 项：修复后只复判受影响的规则。"""

    def test_geometry_repair_does_not_rejudge_vl_rules_each_round(self):
        # 600x600 / fg≈0.30 → R1 fail（短边）+ R4 fail（占比），几何一轮收敛
        gw = self.gateway(Image(600, 600, (110, 110, 490, 490)), white_dev=1.0)
        verdicts = self.verdicts(self.repair())
        self.assertEqual(verdicts["R1_size"]["status"], "pass")
        self.assertEqual(verdicts["R4_subject_ratio"]["status"], "pass")
        # R0 有确定性前置（主体面积 0.30 ≥ 门槛 → 不问 VL）、R3 是**确定性**判据
        # （OCR + 主体框分档，这张图没有文字）→ 整轮零 VL，后续轮次更没有「全量复判」可省
        self.assertEqual(len(_vl_prompts(gw, ruleset.domain_prompt())), 0)
        self.assertEqual(len(_vl_prompts(gw, ruleset.PROMPT_NO_OVERLAY)), 0)
        self.assertEqual(gw.count("/vl/judge"), 0)
        # 但该复判的必须复判：crop 改了画布 → 文字区相对面积变了 → 每轮一次 OCR（2 轮）
        self.assertEqual(gw.count("/detect/ocr"), 2)
        self.assertEqual(gw.inference_count("/detect/ocr"), 2)
        self.assertEqual(gw.cache_hit_count("/detect/ocr"), 0)

    def test_replace_bg_round_rejudges_only_white(self):
        # 白度不合格 → 换底 → 下一轮只复判 R2（尺寸/占比/文字都不动）
        gw = self.gateway(Image(2000, 2000, (700, 700, 1300, 1300)), white_dev=40.0)
        verdicts = self.verdicts(self.repair())
        self.assertEqual(gw.count("/op/replace_bg"), 1)
        self.assertEqual(verdicts["R2_white_bg"]["status"], "pass")
        self.assertTrue(verdicts["R2_white_bg"]["fixed"])
        # 换底轮之后不该再问 R0（面积 0.09 落边界区间 → 只确认一次）；R3 无文字，VL 一次都不问
        # （换底后的复判走 OCR 确定性档）
        self.assertEqual(len(_vl_prompts(gw, ruleset.domain_prompt())), 1)
        self.assertEqual(_vl_prompts(gw, ruleset.PROMPT_NO_OVERLAY), [])
        self.assertEqual(gw.count("/vl/judge"), 1)
        self.assertEqual(gw.count("/detect/ocr"), 2)

    def test_confirm_pass_measures_affected_rules_after_last_round(self):
        """轮次用尽时，收尾只补「受影响」的规则（不是整轮全量）。"""
        class NeverConverges(Image):
            def fg_ratio(self):
                return 0.10

        gw = self.gateway(NeverConverges(600, 600, (200, 200, 400, 400)),
                          white_dev=1.0)
        verdicts = self.verdicts(self.repair())
        # 几何一直判不达标 → 每轮都裁；R0 落边界区间（fg 0.10）只问一次 VL，
        # R3 无文字 → 一次都不问（后续轮次没有全量复判）
        self.assertEqual(len(_vl_prompts(gw, ruleset.domain_prompt())), 1)
        self.assertEqual(_vl_prompts(gw, ruleset.PROMPT_NO_OVERLAY), [])
        self.assertEqual(gw.count("/vl/judge"), 1)
        # 收尾同样没有对同一份文件重复问 OCR（每次 OCR 都是被裁过的新画布）
        self.assertEqual(gw.cache_hit_count("/detect/ocr"), 0)
        self.assertEqual(verdicts["R4_subject_ratio"]["status"], "fail")
        self.assertTrue(str(verdicts["R4_subject_ratio"]["note"]).startswith("needs_human"))

    def test_unfixable_rule_is_not_rejudged(self):
        """几何无解（主体占满）→ 只判一次，不在收尾再问一遍。"""
        gw = self.gateway(Image(2000, 2000, (0, 0, 2000, 2000)), white_dev=1.0)
        verdicts = self.verdicts(self.repair())
        self.assertGreaterEqual(verdicts["R4_subject_ratio"]["score"], 0.85)
        # 掩码测量一共 2 次 HTTP：① measure 里 R0/R3/R4 共用一份；② 几何规划要
        # `target_ratio` 的裁剪窗（协议 1.1 `/detect/bbox`）。宿主 `segment_cached` 让
        # u2net/bbox 共用同一份推理，所以**真实推理仍是 1 次**；收尾也没有为「无解规则」
        # 再重问一遍（第 3 次不存在）。
        self.assertEqual(gw.count("/detect/bbox"), 2)
        self.assertEqual(gw.inference_count("/detect/bbox"), 1)
        self.assertEqual(gw.count("/detect/u2net"), 0)
        self.assertEqual(gw.count("/detect/ocr"), 1)
        # R0 面积铺满整幅 → 掩码不像单个主体 → 交 VL 确认一次（3 份采样在同一请求里）
        self.assertEqual(gw.count("/vl/judge"), 1)
        self.assertEqual(len(_vl_prompts(gw, ruleset.domain_prompt())), 1)

    def test_round_cap_still_hard(self):
        class NeverConverges(Image):
            def fg_ratio(self):
                return 0.10

        gw = self.gateway(NeverConverges(800, 800, (200, 200, 600, 600)),
                          white_dev=1.0)
        result = self.repair()
        rounds = [item.get("round") for item in result["trail"] if item.get("round")]
        self.assertLessEqual(max(rounds + [0]), ruleset.MAX_ROUNDS)


class NoProgressRetirementTest(Base):
    """卡面第 2 项的性能侧：同一算子重复做且结论没变 → 不再第二遍（省下真实算子时间）。"""

    def test_inpaint_not_retried_when_text_survives(self):
        # 补图后文字还在（现实里 lama 补不干净就是这种）→ 只补一次，然后判 needs_human
        # 画布不再变（R1/R2/R4 全过）：假网关照真宿主口径会在几何算子后搬 OCR 框、裁掉画布外的字，
        # 用「角落促销条 + 小主体」的老夹具会让文字在裁剪后消失、R3 直接变 pass，测不到补图。
        gw = self.gateway(Image(2000, 2000, (300, 300, 1800, 1800)), white_dev=1.0,
                          overlay_lines=[("限时 5 折", [10, 10, 300, 80])],
                          inpaint_removes_text=False)
        verdicts = self.verdicts(self.repair())
        self.assertEqual(gw.count("/op/inpaint"), 1)
        self.assertEqual(verdicts["R3_no_watermark"]["status"], "fail")
        self.assertTrue(str(verdicts["R3_no_watermark"]["note"]).startswith("needs_human"))
        self.assertIn("repair_not_converged", verdicts["R3_no_watermark"]["note"])

    def test_inpaint_ocr_called_once_per_round_not_twice(self):
        """OCR 只按「每轮一次」花：补图那一步复用本轮 R3 已有的框（不再独立跑 OCR）。"""
        gw = self.gateway(Image(2000, 2000, (300, 300, 1800, 1800)), white_dev=1.0,
                          overlay_lines=[("限时 5 折", [10, 10, 300, 80])],
                          inpaint_removes_text=False)
        self.repair()
        # 第 1 轮 measure(OCR) + 第 2 轮 measure(OCR)，补图步骤不再额外调 OCR
        self.assertEqual(gw.count("/detect/ocr"), 2)
        # 两次都是被裁/补过的新画布 → 真推理两次；同一份文件被问两遍会记成缓存命中
        self.assertEqual(gw.inference_count("/detect/ocr"), 2)
        self.assertEqual(gw.cache_hit_count("/detect/ocr"), 0)

    def test_overlay_vl_verdict_carries_boxes_for_inpaint(self):
        """R3 走 VL 裁量档时，evidence 也要带「该擦的框」——补图据此复用，不再跑第二遍 OCR。

        K07-FIX-G 实测：VL 档只回了 OCR 事实（lines/面积），漏了修复用的 `boxes`，
        于是 repair 的 inpaint 步骤拿不到框 → 为**同一份文件**再跑一遍 OCR（+2.9 s/张）。
        """
        gw = self.gateway(Image(2000, 2000, (300, 300, 1800, 1800)), white_dev=1.0,
                          overlay_lines=[("限时 5 折", [10, 10, 300, 80])])
        verdicts = self.verdicts(self.repair())
        self.assertEqual(gw.count("/op/inpaint"), 1)
        self.assertEqual(verdicts["R3_no_watermark"]["status"], "pass")
        self.assertTrue(verdicts["R3_no_watermark"]["fixed"])
        before = (verdicts["R3_no_watermark"]["evidence"] or {}).get("before") or {}
        self.assertEqual(before.get("source"), "ocr+vl")
        self.assertEqual(len(before.get("boxes") or []), 1)
        # 第 1 轮 measure + 第 2 轮 measure；补图没为自己再要一次 OCR
        self.assertEqual(gw.count("/detect/ocr"), 2)
        self.assertEqual(gw.cache_hit_count("/detect/ocr"), 0)

    def test_replace_bg_not_retried_when_still_off_white(self):
        # 换底后白度仍不达标（模拟换底接口没真的变白）→ 只换一次
        class StayDirty(FakeImageGateway):
            def _white_dev(self):
                return 40.0

        gw = StayDirty(self.root, Image(2000, 2000, (700, 700, 1300, 1300)),
                       white_dev=40.0).start()
        PLUGIN._GATEWAY_URL = gw.base_url
        PLUGIN._GATEWAY_TOKEN = gw.token
        self.addCleanup(gw.stop)
        self.asset = gw.seed_asset("asset.png")
        verdicts = self.verdicts(self.repair())
        self.assertEqual(gw.count("/op/replace_bg"), 1)
        self.assertEqual(verdicts["R2_white_bg"]["status"], "fail")
        self.assertIn("repair_not_converged", verdicts["R2_white_bg"]["note"])


class AffectedRulesTableTest(unittest.TestCase):
    """「算子 → 波及规则」必须是**推导**出来的（用户裁决 2026-10-21：手写列表必漏项）。

    口径：算子声明 writes 的测量量；规则声明 reads 的测量量；
    波及 = `reads ∩ writes(已执行 ops) ≠ ∅`。这里只钉住推导的**边界条件**，
    不再逐条手写 op→规则（那正是被废掉的写法）。
    """

    def test_every_write_and_read_is_a_known_measurement(self):
        for op, writes in ruleset.OP_WRITES.items():
            for m in writes:
                self.assertIn(m, ruleset.MEASUREMENTS, "%s 写了未知测量量 %s" % (op, m))
        for rid, reads in ruleset.RULE_READS.items():
            self.assertIn(rid, ruleset.PIPELINE_ORDER)
            for m in reads:
                self.assertIn(m, ruleset.MEASUREMENTS, "%s 读了未知测量量 %s" % (rid, m))

    def test_geometry_ops_rejudge_size_ratio_white(self):
        for op in ("crop", "letterbox"):
            got = set(ruleset.AFFECTED_RULES[op])
            self.assertTrue({"R1_size", "R2_white_bg", "R4_subject_ratio"} <= got, op)
        self.assertIn("R1_size", ruleset.AFFECTED_RULES["upscale"])
        # 裁剪会改变文字区在画面里的相对位置/面积 → 文字规则也要复判
        self.assertIn("R3_no_watermark", ruleset.AFFECTED_RULES["crop"])
        self.assertNotIn("R3_no_watermark", ruleset.AFFECTED_RULES["letterbox"])

    def test_bg_and_inpaint_rejudge_what_they_touch(self):
        self.assertIn("R2_white_bg", ruleset.AFFECTED_RULES["replace_bg"])
        self.assertIn("R3_no_watermark", ruleset.AFFECTED_RULES["inpaint"])
        self.assertIn("R2_white_bg", ruleset.AFFECTED_RULES["inpaint"])

    def test_no_op_ever_rejudges_vl_or_tagging(self):
        """R0（VL，≈8 s）与 R5（打标）不该被任何修复算子拖进复判：只读 `domain`。"""
        for op in ruleset.OP_WRITES:
            self.assertNotIn("R0_product_main", ruleset.AFFECTED_RULES[op], op)
            self.assertNotIn("R5_attribute", ruleset.AFFECTED_RULES[op], op)
        self.assertNotIn("domain", {m for w in ruleset.OP_WRITES.values() for m in w})

    def test_affected_rules_accepts_multiple_ops_and_is_ordered(self):
        got = ruleset.affected_rules(["crop", "inpaint", "nope"])
        self.assertEqual(list(got), [rid for rid in ruleset.PIPELINE_ORDER if rid in got])
        self.assertEqual(ruleset.affected_rules([]), ())


if __name__ == "__main__":
    unittest.main()
