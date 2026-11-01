"""repair 修复链单测：真 HTTP 假网关 + 有状态图像模型（收敛 / 上限 / 不可解）。

验四件事：
1. 闭式解一轮就把「占比 + 短边 + 1:1」三件事一起解决，且不裁掉主体（面积守恒）；
2. 轮次上限 3 是**硬**的：永不收敛的图也只在第 3 轮收手（无死循环）；
3. 几何无解（主体占满/无测量）→ 不瞎裁，落 `needs_human:*`；
4. 换底 / OCR 局部 inpaint 各自落盘产物，掩码是合法 PNG。
"""
from __future__ import annotations

import os
import sys
import unittest

import _support

import maskpng
import ruleset
from behavorkit import FakeImageGateway, Image

PLUGIN = _support.load_plugin()


class NeverConverges(Image):
    """不管怎么裁，u2net 都报 0.10（模拟「主体是整幅实景、掩码判不出主体」）。"""

    def fg_ratio(self):
        return 0.10


class Base(unittest.TestCase):
    def setUp(self):
        self.root = _support.tempdir("pack-repair-")
        self.workdir = os.path.join(self.root, "work")
        os.makedirs(self.workdir, exist_ok=True)
        self._orig_url = PLUGIN._GATEWAY_URL
        self._orig_token = PLUGIN._GATEWAY_TOKEN
        PLUGIN._CANCEL.clear()

    def gateway(self, image, **kwargs):
        gw = FakeImageGateway(self.root, image, **kwargs).start()
        PLUGIN._GATEWAY_URL = gw.base_url
        PLUGIN._GATEWAY_TOKEN = gw.token
        self.addCleanup(gw.stop)
        self.addCleanup(self._restore)
        self.asset = gw.seed_asset("asset.png")
        return gw

    def _restore(self):
        PLUGIN._GATEWAY_URL = self._orig_url
        PLUGIN._GATEWAY_TOKEN = self._orig_token
        PLUGIN._CANCEL.clear()

    def repair(self, **options):
        return PLUGIN.repair("job-r", {"id": "asset", "path": self.asset},
                             None, dict(options, workdir=self.workdir))

    def verdicts(self, result):
        return {v["ruleId"]: v for v in result["verdicts"]}


class GeometryRepairTest(Base):
    def test_one_round_fixes_ratio_and_short_side(self):
        gw = self.gateway(Image(600, 600, (110, 110, 490, 490)))   # fg≈0.30
        before_area = gw.images[self.asset].area()
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertEqual(verdicts["R1_size"]["status"], "pass")
        self.assertEqual(verdicts["R4_subject_ratio"]["status"], "pass")
        self.assertTrue(verdicts["R1_size"]["fixed"])
        self.assertEqual({a["rule_id"] for a in result["artifacts"]},
                         {"R1_size", "R4_subject_ratio"})
        artifact = next(a for a in result["artifacts"] if a["rule_id"] == "R1_size")
        self.assertTrue(os.path.isfile(artifact["path"]))
        self.assertGreaterEqual(min(artifact["width"], artifact["height"]), 1600)
        self.assertEqual(artifact["width"], artifact["height"])     # 1:1 画布
        after = gw.state_of(artifact["path"])
        self.assertGreaterEqual(after.fg_ratio(), ruleset.THRESHOLDS[
            "R4_subject_ratio"]["min"])
        # 无裁切：中间裁剪产物的主体面积与原图一致（面积守恒 = 主体没被切掉）
        crop_outs = [name for name in os.listdir(self.workdir) if "-1crop.png" in name]
        self.assertEqual(len(crop_outs), 1)
        cropped_area = gw.state_of(os.path.join(self.workdir, crop_outs[0])).area()
        self.assertAlmostEqual(cropped_area / float(before_area), 1.0, places=2)
        self.assertEqual(gw.count("/op/crop"), 1)                   # 一轮一裁
        self.assertEqual(gw.count("/op/upscale"), 1)

    def test_ratio_only_repair_does_not_upscale(self):
        gw = self.gateway(Image(3200, 3200, (940, 940, 2690, 2690)))  # fg≈0.30, 尺寸已够
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertEqual(verdicts["R1_size"]["status"], "pass")
        self.assertEqual(verdicts["R4_subject_ratio"]["status"], "pass")
        self.assertTrue(verdicts["R4_subject_ratio"]["fixed"])
        self.assertEqual([a["rule_id"] for a in result["artifacts"]],
                         ["R4_subject_ratio"])
        self.assertEqual(gw.count("/op/upscale"), 0)
        self.assertGreaterEqual(gw.count("/op/crop"), 1)

    def test_non_square_converges_without_letterbox_when_host_window_available(self):
        # 2000x1200 主体占 44%（框距上下各 40px，不触发 R0「主体被裁切」否决）：
        # 协议 1.1 宿主窗按短边钳成 1200² 方形窗
        # → 不用补白就成 1:1（旧行为是「裁短边 + 补白」，白边是额外动作）
        gw = self.gateway(Image(2000, 1200, (400, 40, 1400, 1160)))
        result = self.repair()
        self.assertEqual(gw.count("/op/letterbox"), 0)
        artifact = result["artifacts"][0]
        self.assertEqual(artifact["width"], artifact["height"])
        # 主体没被裁掉：成品的占比仍在窗口内（旧行为居中裁会把贴边主体削掉一块）
        after = gw.state_of(artifact["path"])
        self.assertGreaterEqual(after.fg_ratio(), ruleset.THRESHOLDS["R4_subject_ratio"]["min"])
        self.assertLessEqual(after.fg_ratio(), ruleset.THRESHOLDS["R4_subject_ratio"]["max"])

    def test_letterbox_used_when_host_has_no_bbox_endpoint(self):
        # 旧宿主（协议 1.0，无 /detect/bbox）：退回居中推定 → 非正方形只能裁短边 + 补白
        gw = self.gateway(Image(2000, 1200, (400, 40, 1400, 1160)), bbox_available=False)
        result = self.repair()
        self.assertGreaterEqual(gw.count("/op/letterbox"), 1)
        artifact = result["artifacts"][0]
        self.assertEqual(artifact["width"], artifact["height"])

    def test_offcenter_subject_still_converges_within_budget(self):
        # 主体贴近左下（留 2.5% 边距，不触发 R0 裁切否决）：第一轮裁剪会切掉一部分面积
        # → 实测占比低于预测 → 第二轮收紧
        gw = self.gateway(Image(800, 800, (20, 380, 430, 770)))
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertLessEqual(gw.count("/op/crop"), ruleset.MAX_ROUNDS)
        self.assertIn(verdicts["R4_subject_ratio"]["status"], ("pass", "fail"))
        if verdicts["R4_subject_ratio"]["status"] == "fail":
            self.assertTrue(verdicts["R4_subject_ratio"]["note"].startswith(
                "needs_human:"))

    def test_subject_too_full_is_not_cropped(self):
        gw = self.gateway(Image(1600, 1600, (0, 0, 1600, 1600)))    # fg=1.0
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertEqual(gw.count("/op/crop"), 0, "占比超上限时裁剪只会更糟，必须不动刀")
        self.assertEqual(gw.count("/op/upscale"), 0)
        verdict = verdicts["R4_subject_ratio"]
        self.assertEqual(verdict["status"], "fail")
        self.assertFalse(verdict["fixed"])
        self.assertEqual(verdict["note"], "needs_human:subject_ratio_above_max")
        self.assertEqual(result["artifacts"], [])

    def test_never_converging_stops_at_max_rounds(self):
        gw = self.gateway(NeverConverges(800, 800))
        result = self.repair()
        # 轮次上限是**硬**的（旧实现钉的是「正好 3 次裁剪」）。K07-FIX-D 之后允许更早收手：
        # 同一套几何方案做完画布都没变（不动点）就判 repair_not_converged，不再空转 ——
        # 但绝不能超过上限，也绝不能没有产物/结论。
        crops = gw.count("/op/crop")
        self.assertGreaterEqual(crops, 1)
        self.assertLessEqual(crops, ruleset.MAX_ROUNDS,
                            "轮次上限必须是硬的：%d" % ruleset.MAX_ROUNDS)
        self.assertLessEqual(len(result["trail"]), ruleset.MAX_ROUNDS * 2)
        verdict = self.verdicts(result)["R4_subject_ratio"]
        self.assertFalse(verdict["fixed"])
        self.assertTrue(verdict["note"].startswith("needs_human:"))
        # 尺寸那一路是真修好了（不因占比失败而连带否认），但占比产物不许伪造
        self.assertNotIn("R4_subject_ratio", [a["rule_id"] for a in result["artifacts"]])


class CombinedChainTest(Base):
    def test_three_rules_fixed_in_one_repair(self):
        """卡面验收 2 的组合形态：占比 + 短边 + 白底 同时不达标，一次 repair 全达成。"""
        gw = self.gateway(Image(700, 700, (280, 280, 555, 555)),    # fg≈0.15, 700px
                          white_bg="fail")
        result = self.repair()
        verdicts = self.verdicts(result)
        for rule_id in ("R1_size", "R4_subject_ratio", "R2_white_bg"):
            self.assertEqual(verdicts[rule_id]["status"], "pass", rule_id)
            self.assertTrue(verdicts[rule_id]["fixed"], rule_id)
        artifact_ids = {a["rule_id"] for a in result["artifacts"]}
        self.assertEqual(artifact_ids, {"R1_size", "R4_subject_ratio", "R2_white_bg"})
        # 复检用的最终产物：1:1、短边 ≥1600、占比在窗口内、背景已白
        final = gw.state_of(result["artifacts"][0]["path"])
        self.assertEqual(final.width, final.height)
        self.assertGreaterEqual(final.width, 1600)
        self.assertGreaterEqual(final.fg_ratio(), ruleset.THRESHOLDS["R4_subject_ratio"]["min"])
        self.assertLessEqual(final.fg_ratio(), ruleset.THRESHOLDS["R4_subject_ratio"]["max"])
        self.assertEqual(gw.white_bg, "pass")
        rounds = [t.get("round") for t in result["trail"] if t.get("round")]
        self.assertLessEqual(max(rounds), ruleset.MAX_ROUNDS)


class GenerativeRepairTest(Base):
    def test_replace_bg_runs_and_is_fixed(self):
        gw = self.gateway(Image(2000, 2000, (500, 500, 1500, 1500)), white_bg="fail")
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertGreaterEqual(gw.count("/op/replace_bg"), 1)
        self.assertTrue(verdicts["R2_white_bg"]["fixed"])
        self.assertEqual(verdicts["R2_white_bg"]["status"], "pass")
        self.assertIn("R2_white_bg", [a["rule_id"] for a in result["artifacts"]])

    def test_overlay_masked_with_real_png_then_inpainted(self):
        """主体外的促销条 → 局部 inpaint；主体内吊牌不许进掩码（会抹掉商品信息）。"""
        gw = self.gateway(Image(2000, 2000, (150, 150, 1850, 1850)),   # 占比 0.72 → 几何不动
                          overlay_lines=[("限时 5 折 全场包邮", [0, 0, 2000, 160]),
                                         ("款号 12345", [800, 900, 1100, 980])])
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertEqual(gw.count("/op/inpaint"), 1)
        self.assertTrue(verdicts["R3_no_watermark"]["fixed"])
        masks = [name for name in os.listdir(self.workdir) if name.endswith("-mask.png")]
        self.assertEqual(len(masks), 1)
        width, height, rows = maskpng.read_mask_png(os.path.join(self.workdir, masks[0]))
        self.assertEqual((width, height), (2000, 2000))
        whites = sum(1 for row in rows for value in row if value)
        self.assertGreater(whites, 0, "掩码里必须有白块（否则 inpaint 什么也不会改）")
        # 促销条在顶部 → 顶部有白；吊牌在主体内 → 那一带必须是黑的（不重绘商品本体）
        self.assertTrue(any(rows[80]))
        self.assertFalse(any(rows[940][800:1100]))
        # 掩码只覆盖文字框附近，不是整张图
        self.assertLess(whites, width * height * 0.5)

    def test_allow_inpaint_false_keeps_verdict_unfixed(self):
        gw = self.gateway(Image(2000, 2000, (150, 150, 1850, 1850)),
                          overlay_lines=[("限时 5 折 全场包邮", [0, 0, 2000, 160])])
        result = self.repair(allow_inpaint=False)
        self.assertEqual(gw.count("/op/inpaint"), 0)
        verdict = self.verdicts(result)["R3_no_watermark"]
        self.assertFalse(verdict["fixed"])
        self.assertEqual(verdict["status"], "fail")

    def test_watermark_covering_subject_is_inpainted(self):
        """覆盖主体的水印（≥8% 主体面积）属于该擦的叠加物 → fail → inpaint。"""
        gw = self.gateway(Image(2000, 2000, (150, 150, 1850, 1850)),
                          overlay_lines=[("样图 SAMPLE", [400, 900, 1600, 1300])])
        result = self.repair()
        verdicts = self.verdicts(result)
        self.assertEqual(gw.count("/op/inpaint"), 1)
        self.assertTrue(verdicts["R3_no_watermark"]["fixed"])


class RepairContractTest(Base):
    def test_every_artifact_maps_to_fixed_verdict(self):
        self.gateway(Image(600, 600, (110, 110, 490, 490)))
        result = self.repair()
        fixed = {v["ruleId"] for v in result["verdicts"] if v.get("fixed")}
        for artifact in result["artifacts"]:
            self.assertIn(artifact["rule_id"], fixed)
            self.assertEqual(artifact["kind"], "image/png")
            self.assertTrue(os.path.getsize(artifact["path"]) > 0)

    def test_no_fixable_rule_returns_skipped(self):
        self.gateway(Image(2000, 2000, (600, 600, 1500, 1500)))
        result = PLUGIN.repair("job-skip", {"id": "a", "path": self.asset},
                              ["R0_product_main"], {"workdir": self.workdir})
        self.assertEqual(result["artifacts"], [])
        self.assertEqual(result["verdicts"][0]["status"], "skipped")
        self.assertEqual(result["verdicts"][0]["note"], "fix_policy:geometric")

    def test_workdir_required(self):
        self.gateway(Image(2000, 2000, (600, 600, 1500, 1500)))
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.repair("job-nodir", {"id": "a", "path": self.asset}, None, {})
        self.assertEqual(ctx.exception.code, -32003)

    def test_cancel_stops_repair(self):
        self.gateway(Image(600, 600, (110, 110, 490, 490)))
        PLUGIN._cancel_flag("job-cxl").set()
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.repair("job-cxl", {"id": "a", "path": self.asset}, None,
                          {"workdir": self.workdir})
        self.assertEqual(ctx.exception.code, -32004)


class MaskPngTest(unittest.TestCase):
    def test_roundtrip(self):
        path = os.path.join(_support.tempdir("mask-"), "m.png")
        maskpng.write_mask_png(path, 64, 32, [[10, 4, 20, 12]])
        width, height, rows = maskpng.read_mask_png(path)
        self.assertEqual((width, height), (64, 32))
        self.assertEqual(rows[8][15], 0xFF)
        self.assertEqual(rows[30][60], 0x00)

    def test_boxes_are_clipped_into_canvas(self):
        path = os.path.join(_support.tempdir("mask-"), "m.png")
        maskpng.write_mask_png(path, 16, 16, [[-50, -50, 500, 500]])
        width, height, rows = maskpng.read_mask_png(path)
        self.assertEqual(sum(1 for row in rows for value in row if value), 16 * 16)

    def test_empty_boxes_writes_blank_mask(self):
        path = os.path.join(_support.tempdir("mask-"), "m.png")
        maskpng.write_mask_png(path, 8, 8, [])
        _, _, rows = maskpng.read_mask_png(path)
        self.assertEqual(sum(sum(row) for row in rows), 0)


if __name__ == "__main__":
    unittest.main()
