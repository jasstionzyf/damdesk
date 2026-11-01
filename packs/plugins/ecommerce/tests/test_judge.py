"""judge 语义单测（gw 用可编排替身，不起进程）：短路、打标、错误码、归账。"""
from __future__ import annotations

import json
import unittest

import _support

import ruleset

PLUGIN = _support.load_plugin()


class Base(unittest.TestCase):
    def setUp(self):
        self._orig_gw = PLUGIN.gw
        PLUGIN._REPORTS.clear()
        PLUGIN._CANCEL.clear()
        self.workdir = _support.tempdir()

    def tearDown(self):
        PLUGIN.gw = self._orig_gw
        PLUGIN._CANCEL.clear()

    def use(self, **kwargs):
        stub = _support.StubGateway(**kwargs)
        PLUGIN.gw = stub
        return stub


class JudgeFlowTest(Base):
    def test_full_pipeline_order_and_shape(self):
        gw = self.use()
        result = PLUGIN.judge("job-1", {"id": "a1", "path": "/tmp/a1.png"})
        ids = [v["ruleId"] for v in result["verdicts"]]
        self.assertEqual(ids, ruleset.PIPELINE_ORDER)
        for verdict in result["verdicts"]:
            self.assertEqual(set(verdict), {"ruleId", "status", "score", "label",
                                            "evidence", "fixed", "note", "reasonCode"})
            # N9：reasonCode 恒等 ruleId（规格只改阈值/启用集，不改规则 id）
            self.assertEqual(verdict["reasonCode"], verdict["ruleId"])
            self.assertIn(verdict["status"], ("pass", "fail", "undetermined",
                                              "skipped"))
        self.assertEqual(gw.count("/report/emit"), 1)

    def test_domain_gate_fail_short_circuits(self):
        # 主体面积落边界区间（2%~15%）→ 确定性段不下结论，交 VL 确认；VL 说「否」→ fail
        self.use(u2net={"fg_ratio": 0.05},
                 vl={"商品主图": {"undetermined": False, "raw": "否",
                                  "rules": [{"status": "fail", "label": "no",
                                             "score": 0.0}]}})
        result = PLUGIN.judge("job-gate", {"id": "a2", "path": "/tmp/a2.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R0_product_main"]["status"], "fail")
        for rule_id in ("R1_size", "R2_white_bg", "R4_subject_ratio",
                        "R3_no_watermark", "R5_attribute"):
            self.assertEqual(by_rule[rule_id]["status"], "skipped", rule_id)
            self.assertEqual(by_rule[rule_id]["note"], "domain_gate_fail")

    def test_domain_gate_vetoes_f001_like_scene(self):
        """f001 形态（小主体 + 咖啡杯 + 桌面）→ R0 fail，与 legacy 同语义。

        旧 gate 就栽在这类图上：VL 说「是」（画面里确实有商品），于是整张放行。
        现在 VL 只给事实（others 清单），三项否决由插件按 schema 参数执行 → fail 并短路。
        """
        answer = {"subject": {"present": True, "name": "笔记本电脑",
                              "area_ratio": 0.051, "cut": False},
                  "others": [{"name": "咖啡杯", "kind": "life_scene", "area_ratio": 0.03},
                             {"name": "桌面", "kind": "life_scene", "area_ratio": 0.40}],
                  "verdict": "pass", "reason": "有商品"}
        self.use(u2net={"fg_ratio": 0.051},
                 vl={"商品主图": {"undetermined": False,
                                  "raw": json.dumps(answer, ensure_ascii=False),
                                  "rules": [{"status": "pass", "label": "yes",
                                             "score": 0.72}]}})
        result = PLUGIN.judge("job-gate-f001", {"id": "f001", "path": "/tmp/f001.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "fail")
        reasons = verdict["evidence"]["reasons"]
        self.assertTrue(any(r.startswith("scene_prop_present") for r in reasons), reasons)
        self.assertTrue(any(r.startswith("foreign_object_too_large") for r in reasons), reasons)
        self.assertEqual(verdict["evidence"]["votes"], {"fail": 3})
        self.assertEqual(verdict["evidence"]["decided_by"], "vl_veto")
        self.assertEqual(by_rule["R1_size"]["status"], "skipped")

    def test_domain_gate_cut_goes_to_vl_and_fails_when_subject_present(self):
        """主体框贴边 → **VL 复核**（不是零 VL 直接 fail）：模型确认有主体且被裁 → fail。

        t_9267ed7a：贴边只是「必须问模型」的理由 —— 非商品图（截图/整屏 UI）的掩码同样贴边，
        纯几何判据会把它判成「主体被裁掉」的假不合格。
        """
        answer = {"subject": {"present": True, "name": "鞋子", "area_ratio": 0.30,
                              "cut": False},
                  "others": [], "verdict": "pass", "reason": "有商品主体"}
        gw = self.use(u2net={"fg_ratio": 0.30},
                      bbox={"bbox": [10, 300, 900, 900]},          # 左边距 1% → 贴边
                      vl={"商品主图": {"undetermined": False,
                                      "raw": json.dumps(answer, ensure_ascii=False)}})
        result = PLUGIN.judge("job-gate-cut", {"id": "a4c", "path": "/tmp/a4c.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "fail")
        self.assertIn("subject_cut_off", verdict["evidence"]["reasons"])
        self.assertEqual(verdict["evidence"]["confirm_note"], "subject_cut_edge")
        self.assertEqual(gw.count("/vl/judge"), 1)
        self.assertEqual(by_rule["R4_subject_ratio"]["note"], "domain_gate_fail")

    def test_domain_gate_cut_with_no_subject_skips(self):
        """贴边 + 模型说画面里没有商品主体（截图类）→ skipped，不是 fail（t_9267ed7a）。"""
        answer = {"subject": {"present": False, "name": "", "area_ratio": 0.0,
                              "cut": False},
                  "others": [], "verdict": "fail", "reason": "软件界面截图，无商品"}
        self.use(u2net={"fg_ratio": 0.63},                        # 掩码面积大也是噪声
                 bbox={"bbox": [553, 74, 2086, 1402]},            # bottom 边距 0 → 贴边
                 vl={"商品主图": {"undetermined": False,
                                 "raw": json.dumps(answer, ensure_ascii=False)}})
        result = PLUGIN.judge("job-gate-cut5", {"id": "shot-a", "path": "/tmp/shot-a.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "vl_not_applicable")
        self.assertIsNone(verdict["score"])
        self.assertEqual(by_rule["R1_size"]["status"], "skipped")

    def test_domain_gate_undetermined_short_circuits(self):
        self.use(u2net={"fg_ratio": 0.05},
                 vl={"商品主图": {"undetermined": True, "reason": "vl_parse_failed",
                                  "raw": ""}})
        result = PLUGIN.judge("job-gate2", {"id": "a3", "path": "/tmp/a3.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R0_product_main"]["status"], "undetermined")
        self.assertEqual(by_rule["R1_size"]["note"], "domain_gate_undetermined")

    def test_domain_gate_large_subject_passes_without_vl(self):
        """主体面积 ≥15% 且掩码成团 → pass，一次 VL 都不问（确定性前置）。"""
        # 主体框在画布内（边距 25%）→ 不触发「主体被裁切」否决，走面积确定性档
        gw = self.use(u2net={"fg_ratio": 0.42},
                      bbox={"bbox": [400, 400, 1200, 1200]})
        result = PLUGIN.judge("job-gate3", {"id": "a3b", "path": "/tmp/a3b.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["evidence"]["decided_by"], "subject_area")
        self.assertEqual(verdict["evidence"]["source"], "u2net_area")
        self.assertEqual(verdict["evidence"]["vl_calls"], 0)
        self.assertEqual(verdict["evidence"]["fg_ratio"], 0.42)
        self.assertIsNotNone(verdict["evidence"]["solidity"])
        domain_calls = [body for (path, body) in gw.calls
                        if path == "/vl/judge" and "商品主图" in str(body.get("prompt"))]
        self.assertEqual(domain_calls, [])

    def test_domain_gate_absent_subject_skips_without_vl(self):
        """主体面积 ≤2% → 画面里没有可判主体 = 本包对该图**不适用** → skipped（也不问 VL）。

        卡 t_9267ed7a：先前这里判 fail，inspector 上表现为「Fail / 置信度 0.00 / 原始分 0.000」
        —— 等于给非商品图挂了个假不合格。现在是 skipped（不判），后续规则连带 skipped。
        """
        gw = self.use(u2net={"fg_ratio": 0.008})
        result = PLUGIN.judge("job-gate4", {"id": "a3c", "path": "/tmp/a3c.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "no_subject_area")
        self.assertIsNone(verdict["score"])          # 不给 0.000 假分（confidence 也就不再 0.00）
        self.assertEqual(by_rule["R1_size"]["note"], "domain_gate_skipped")
        for rule_id in ("R1_size", "R2_white_bg", "R4_subject_ratio",
                        "R3_no_watermark", "R5_attribute"):
            self.assertEqual(by_rule[rule_id]["status"], "skipped", rule_id)
        domain_calls = [body for (path, body) in gw.calls
                        if path == "/vl/judge" and "商品主图" in str(body.get("prompt"))]
        self.assertEqual(domain_calls, [])

    def test_domain_gate_vl_subject_missing_skips(self):
        """VL 结构化应答说 present=false（画面里没有商品主体）→ R0 skipped，不是 fail。"""
        raw = json.dumps({"subject": {"present": False, "name": "", "area_ratio": 0.0,
                                      "cut": False},
                          "others": [], "verdict": "fail", "reason": "风景照，无商品"})
        self.use(u2net={"fg_ratio": 0.05},          # 边界区间 → 交 VL 确认
                 vl_samples={"商品主图": [{"undetermined": False, "raw": raw}] * 3})
        result = PLUGIN.judge("job-gate4b", {"id": "a3c2", "path": "/tmp/a3c2.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "skipped")
        self.assertIsNone(verdict["score"])
        self.assertIn("no_product_subject", verdict["evidence"]["reasons"])
        self.assertEqual(by_rule["R1_size"]["note"], "domain_gate_skipped")

    def test_product_image_keeps_three_states(self):
        """商品图口径不变（卡 t_9267ed7a 只改「不适用」）：pass / fail / undetermined 照旧。

        三档一次钉住，防止「把不适用改成 skipped」时顺手把真不合规也放过。
        """
        present = {"subject": {"present": True, "name": "鞋子", "area_ratio": 0.30,
                               "cut": False},
                   "others": [], "verdict": "pass", "reason": "有商品主体"}
        cases = [
            ("有主体且成团", {"fg_ratio": 0.42}, {"bbox": [400, 400, 1200, 1200]},
             None, "pass"),
            ("主体贴边（VL 确认有主体）", {"fg_ratio": 0.30}, {"bbox": [10, 300, 900, 900]},
             {"undetermined": False, "raw": json.dumps(present, ensure_ascii=False)}, "fail"),
            ("VL 判不出来", {"fg_ratio": 0.05}, {},
             {"undetermined": True, "reason": "vl_parse_failed", "raw": ""}, "undetermined"),
        ]
        for name, u2net, bbox, vl, want in cases:
            kwargs = {"u2net": u2net, "bbox": bbox}
            if vl is not None:
                kwargs["vl"] = {"商品主图": vl}
            self.use(**kwargs)
            result = PLUGIN.judge("job-three-%s" % want,
                                  {"id": "a3x", "path": "/tmp/a3x.png"})
            verdict = {v["ruleId"]: v for v in result["verdicts"]}["R0_product_main"]
            self.assertEqual(verdict["status"], want, name)

    def test_domain_gate_diffuse_mask_goes_to_vl(self):
        """面积够但掩码散碎（填充度 <0.25）→ 不是单个主体，交 VL 确认。"""
        # fg 0.20 / 主导框铺满整幅 → 填充度 0.20 < 0.25：掩码散碎，不是一个商品
        gw = self.use(u2net={"fg_ratio": 0.20},
                      bbox={"bbox": [0, 0, 1600, 1600]})
        result = PLUGIN.judge("job-gate5", {"id": "a3d", "path": "/tmp/a3d.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        # K07-FIX-I：VL 档的结论由「结构化应答 + 三项否决」在代码里算出来 → vl_veto
        self.assertEqual(verdict["evidence"]["decided_by"], "vl_veto")
        self.assertEqual(verdict["evidence"]["confirm_note"], "diffuse_mask")
        self.assertEqual(verdict["evidence"]["votes"], {"pass": 3})
        # 一次请求内含 3 份采样（`samples`），不是 3 次请求
        self.assertEqual(gw.count("/vl/judge"), 1)
        samples = [b.get("samples") for (p, b) in gw.calls if p == "/vl/judge"]
        self.assertEqual(samples, [3])
        self.assertEqual(verdict["evidence"]["samples_answered"], 3)

    def test_domain_gate_majority_vote_adopts_the_majority(self):
        """三次采样 2:1 → 采纳多数票（K07-FIX-I 把「全一致」改成「过半多数票」）。

        票数与逐票结论都落 evidence：`votes` + `sample_statuses` + `sample_verdicts`。
        """
        self.use(u2net={"fg_ratio": 0.05},
                 vl_samples={"商品主图": [
                     {"undetermined": False, "raw": "是",
                      "rules": [{"status": "pass", "label": "yes", "score": 0.6}]},
                     {"undetermined": False, "raw": "否",
                      "rules": [{"status": "fail", "label": "no", "score": 0.6}]},
                     {"undetermined": False, "raw": "是",
                      "rules": [{"status": "pass", "label": "yes", "score": 0.6}]},
                 ]})
        result = PLUGIN.judge("job-gate6", {"id": "a3e", "path": "/tmp/a3e.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["note"], "vl_majority_vote")
        self.assertEqual(verdict["evidence"]["votes"], {"pass": 2, "fail": 1})
        self.assertEqual(verdict["evidence"]["vote_margin"], "2/3")
        self.assertEqual(verdict["evidence"]["sample_statuses"],
                         ["pass", "fail", "pass"])
        self.assertEqual(len(verdict["evidence"]["sample_verdicts"]), 3)
        self.assertNotEqual(by_rule["R1_size"]["note"], "domain_gate_undetermined")

    def test_domain_gate_no_majority_lands_undetermined(self):
        """1:1:1（含 undetermined 的平票）→ undetermined，绝不冒充 pass/fail。"""
        self.use(u2net={"fg_ratio": 0.05},
                 vl_samples={"商品主图": [
                     {"undetermined": False, "raw": "是",
                      "rules": [{"status": "pass", "label": "yes", "score": 0.6}]},
                     {"undetermined": False, "raw": "否",
                      "rules": [{"status": "fail", "label": "no", "score": 0.6}]},
                     {"undetermined": True, "reason": "vl_parse_failed", "raw": ""},
                 ]})
        result = PLUGIN.judge("job-gate6b", {"id": "a3e2", "path": "/tmp/a3e2.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R0_product_main"]
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_no_majority")
        self.assertEqual(verdict["evidence"]["votes"],
                         {"pass": 1, "fail": 1, "undetermined": 1})
        self.assertEqual(by_rule["R1_size"]["note"], "domain_gate_undetermined")

    def test_domain_gate_three_consistent_samples_are_stable(self):
        """三次采样一致 → 采纳该结论，不因为「问了三遍」变成 undetermined。"""
        reply = {"undetermined": False, "raw": "是",
                 "rules": [{"status": "pass", "label": "yes", "score": 0.8}]}
        self.use(u2net={"fg_ratio": 0.05},
                 vl_samples={"商品主图": [reply, reply, reply]})
        statuses = []
        for i in range(2):                       # 判两轮，结论必须一样
            result = PLUGIN.judge("job-gate7-%d" % i,
                                  {"id": "a3f", "path": "/tmp/a3f.png"})
            by_rule = {v["ruleId"]: v for v in result["verdicts"]}
            statuses.append(by_rule["R0_product_main"]["status"])
        self.assertEqual(statuses, ["pass", "pass"])

    def test_domain_evidence_carries_prompt_identity(self):
        """prompt 版本化：evidence 里必须看见 prompt_id / sha256 / temperature。"""
        self.use(u2net={"fg_ratio": 0.05},
                 vl={"商品主图": {"undetermined": False, "raw": "是",
                                  "rules": [{"status": "pass", "label": "yes",
                                             "score": 0.9}]}})
        result = PLUGIN.judge("job-gate8", {"id": "a3g", "path": "/tmp/a3g.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        evidence = by_rule["R0_product_main"]["evidence"]
        spec = ruleset.prompt_spec("R0_product_main", ruleset.domain_prompt())
        self.assertEqual(evidence["prompt_id"], spec["id"])
        self.assertEqual(evidence["prompt_sha256"], spec["sha256"])
        self.assertEqual(evidence["temperature"], 0)
        self.assertEqual(evidence["max_tokens"], 4096)

    def test_size_fail_skips_subject_ratio(self):
        """pipeline 的 depends_on（R4 依赖 R1）：尺寸不合格时先修尺寸，占比不判。"""
        self.use(info={"width": 400, "height": 400})
        result = PLUGIN.judge("job-dep", {"id": "a4", "path": "/tmp/a4.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R1_size"]["status"], "fail")
        self.assertEqual(by_rule["R4_subject_ratio"]["status"], "skipped")
        self.assertEqual(by_rule["R4_subject_ratio"]["note"], "depends_on:R1_size")

    def test_subject_ratio_measured_when_size_ok(self):
        gw = self.use(info={"width": 2000, "height": 2000},
                      u2net={"fg_ratio": 0.4})
        result = PLUGIN.judge("job-r4", {"id": "a5", "path": "/tmp/a5.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R4_subject_ratio"]["status"], "fail")
        self.assertEqual(by_rule["R4_subject_ratio"]["score"], 0.4)
        # 协议 1.1：主体几何走 /detect/bbox（占比 + 主体框 + 目标窗一次拿全），
        # u2net 只在旧宿主（无 bbox 端点）路径上用；R3 复用同一份主体测量，不重复调
        self.assertEqual(gw.count("/detect/bbox"), 1)
        self.assertEqual(gw.count("/detect/u2net"), 0)

    def test_subject_bbox_lands_in_r4_evidence(self):
        gw = self.use(info={"width": 2000, "height": 2000},
                      u2net={"fg_ratio": 0.4},
                      bbox={"dominant_bbox": [200, 300, 1800, 1700]})
        result = PLUGIN.judge("job-r4b", {"id": "a5b", "path": "/tmp/a5b.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        evidence = by_rule["R4_subject_ratio"]["evidence"]
        self.assertEqual(evidence["dominant_bbox"], [200, 300, 1800, 1700])
        self.assertEqual(gw.count("/detect/bbox"), 1)

    def test_white_bg_uses_gateway_pixel_stats_by_default(self):
        # 协议 1.1：R2 直接吃网关 /detect/white_stats（确定性，和旧引擎同口径）
        gw = self.use(white={"mean_dev": 3.2, "worst_dev": 9.0})
        result = PLUGIN.judge("job-white1b", {"id": "a6b", "path": "/tmp/a6b.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R2_white_bg"]["status"], "pass")
        self.assertEqual(by_rule["R2_white_bg"]["score"], 3.2)
        self.assertEqual(by_rule["R2_white_bg"]["evidence"]["source"], "classic_white_edge")
        self.assertEqual(gw.count("/detect/white_stats"), 1)
        # 偏白（> max_dev 8）直接 fail，不靠 VL
        gw = self.use(white={"mean_dev": 15.5})
        result = PLUGIN.judge("job-white1c", {"id": "a6c", "path": "/tmp/a6c.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R2_white_bg"]["status"], "fail")
        self.assertEqual(by_rule["R2_white_bg"]["note"], "background_not_white")

    def test_white_bg_prefers_pixel_stats_when_available(self):
        gw = self.use()
        result = PLUGIN.judge("job-white", {"id": "a6", "path": "/tmp/a6.png"},
                              options={"measurements": {"white_dev": 3.2}})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R2_white_bg"]["status"], "pass")
        self.assertEqual(by_rule["R2_white_bg"]["evidence"]["source"], "classic_white_edge")
        # R0 确定性前置 pass、R3 无文字直接 pass → 一次 VL 都不花
        self.assertEqual(gw.count("/vl/judge"), 0)
        self.assertEqual(gw.count("/detect/white_stats"), 0)   # 宿主已给统计，不再问网关

    def test_white_bg_falls_back_to_vl_without_stats_capability(self):
        # 旧宿主（协议 1.0 网关）没有 /detect/white_stats → 退回 VL，并标明降级来源
        gw = self.use(vl={"商品主图": {"undetermined": False, "raw": "是",
                                       "rules": [{"status": "pass", "label": "yes",
                                                  "score": 0.9}]},
                          "白底": {"undetermined": False, "raw": "否",
                                   "rules": [{"status": "fail", "label": "bg",
                                              "score": 0.0}]}},
                      errors={"/detect/white_stats": ruleset.RpcError(-32001, "能力未授权")})
        result = PLUGIN.judge("job-white2", {"id": "a7", "path": "/tmp/a7.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R2_white_bg"]["status"], "fail")
        self.assertEqual(by_rule["R2_white_bg"]["evidence"]["source"],
                         "vl_fallback_no_white_stats")
        self.assertEqual(gw.count("/vl/judge"), 1)   # R2 降级（一次请求内含 3 份采样）

    def test_ocr_text_without_bbox_and_big_area_fails(self):
        """大面积主体外文字（无主体框的老宿主）→ 确定性 fail，不问 VL。"""
        gw = self.use(info={"width": 1000, "height": 1000},
                      ocr={"lines": [{"text": "限时 5 折", "box": [0, 0, 400, 100]},
                                     {"text": "包邮", "box": [0, 120, 300, 220]}]})
        result = PLUGIN.judge("job-ocr", {"id": "a8", "path": "/tmp/a8.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        verdict = by_rule["R3_no_watermark"]
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "promo_text_area")
        self.assertEqual(verdict["evidence"]["source"], "ocr")
        self.assertEqual(len(verdict["evidence"]["boxes"]), 2)
        # R3 是确定性结论 → 不该再问一次 VL
        overlay_calls = [p for p in gw.calls
                         if p[0] == "/vl/judge" and "叠加遮挡物" in str(p[1].get("prompt"))]
        self.assertEqual(overlay_calls, [])

    def test_scene_shot_passes_domain_gate(self):
        """D2=A：场景图/道具图不是「非商品图」，gate 放行，后续规则照常判。"""
        gw = self.use(vl={"商品主图": {"undetermined": False, "raw": "是",
                                       "rules": [{"status": "pass", "label": "yes",
                                                  "score": 0.9}]}},
                      ocr={"lines": [{"text": "限时 5 折 全场包邮", "box": [100, 100, 1900, 320]}]},
                      info={"width": 2000, "height": 2000},
                      u2net={"fg_ratio": 0.40})
        result = PLUGIN.judge("job-scene", {"id": "a13", "path": "/tmp/a13.png"})
        by_rule = {v["ruleId"]: v for v in result["verdicts"]}
        self.assertEqual(by_rule["R0_product_main"]["status"], "pass")
        self.assertNotEqual(by_rule["R1_size"]["status"], "skipped")
        self.assertNotEqual(by_rule["R4_subject_ratio"]["status"], "skipped")
        self.assertNotEqual(by_rule["R3_no_watermark"]["status"], "skipped")
        # 场景图放行后，促销条仍由 R3 判不合格（gate 不替 R3 做决定）
        self.assertEqual(by_rule["R3_no_watermark"]["status"], "fail")

    def test_attribute_rule_is_skipped_by_default(self):
        gw = self.use()
        result = PLUGIN.judge("job-attr", {"id": "a9", "path": "/tmp/a9.png"},
                              rules=["R5_attribute"])
        verdict = result["verdicts"][0]
        self.assertEqual(verdict["status"], "skipped")
        self.assertEqual(verdict["note"], "outputs_only:attribute_tagging")
        self.assertEqual(gw.count("/vl/judge"), 0)

    def test_attribute_opt_in_returns_tags(self):
        self.use(vl={"JSON": {"undetermined": False,
                              "raw": '{"dress_silhouette":"A字","scene":"白底图"}',
                              "rules": []}})
        result = PLUGIN.judge("job-attr2", {"id": "a10", "path": "/tmp/a10.png"},
                              rules=["R5_attribute"], options={"attribute": True})
        verdict = result["verdicts"][0]
        self.assertEqual(verdict["status"], "skipped")          # 打标不算合格判定
        self.assertEqual(verdict["evidence"]["tags"]["dress_silhouette"], "A字")

    def test_unknown_rule_is_reported_not_dropped(self):
        self.use()
        result = PLUGIN.judge("job-unk", {"id": "a11", "path": "/tmp/a11.png"},
                              rules=["R2_white_bg", "R9_made_up"])
        ids = [v["ruleId"] for v in result["verdicts"]]
        self.assertEqual(ids, ["R2_white_bg", "R9_made_up"])     # 声明序 + 未知追加
        self.assertEqual(result["verdicts"][1]["note"], "unknown_rule")

    def test_requested_subset_keeps_pipeline_order(self):
        self.use()
        result = PLUGIN.judge("job-sub", {"id": "a12", "path": "/tmp/a12.png"},
                              rules=["R3_no_watermark", "R1_size"])
        self.assertEqual([v["ruleId"] for v in result["verdicts"]],
                         ["R1_size", "R3_no_watermark"])


class OverlayRuleTest(Base):
    """R3 新语义（用户裁决 D1=A）：OCR 只做候选定位，判据 = 文字区性质 + 面积占比。"""

    SUBJECT = [400, 400, 1600, 1600]      # 1200² 主体，画面 2000²（主体占 36%）

    def judge_overlay(self, lines, *, vl=None, bbox=SUBJECT):
        gw = self.use(info={"width": 2000, "height": 2000},
                      bbox={"dominant_bbox": bbox} if bbox else {},
                      ocr={"lines": [{"text": text, "box": box} for text, box in lines]},
                      vl=vl or {})
        result = PLUGIN.judge("job-r3", {"id": "r3", "path": "/tmp/r3.png"})
        return gw, {v["ruleId"]: v for v in result["verdicts"]}["R3_no_watermark"]

    def test_no_text_passes_without_vl(self):
        gw, verdict = self.judge_overlay([])
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["note"], "no_text")
        self.assertEqual(gw.count("/vl/judge"), 0)          # R0 确定性前置 + R3 无文字 → 零 VL

    def test_subject_label_text_passes(self):
        """主体内的吊牌/型号码：OCR 认出来了，但不是牛皮癣 → pass（K07 门禁 7 行的根因）。"""
        gw, verdict = self.judge_overlay([("款号 12345", [700, 900, 950, 960]),
                                          ("SIZE M", [1000, 900, 1200, 950])])
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["note"], "subject_label_text")
        self.assertEqual(verdict["evidence"]["excluded_label_boxes"], 2)
        self.assertEqual(verdict["evidence"]["boxes"], [])
        self.assertEqual(verdict["evidence"]["source"], "ocr+subject_bbox")
        overlay_calls = [p for p in gw.calls
                         if p[0] == "/vl/judge" and "叠加遮挡物" in str(p[1].get("prompt"))]
        self.assertEqual(overlay_calls, [])

    def test_text_covering_subject_fails(self):
        """覆盖主体的水印：框压在主体上且占主体面积 ≥8% → fail（确定性）。"""
        gw, verdict = self.judge_overlay([("样图", [500, 800, 1700, 1100])])
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "text_covers_subject")
        self.assertEqual(verdict["evidence"]["watermark_boxes"], 1)
        self.assertEqual(len(verdict["evidence"]["boxes"]), 1)

    def test_promo_bar_outside_subject_fails_by_area(self):
        """主体外促销条面积 ≥2% → fail，不问 VL。"""
        _, verdict = self.judge_overlay([("限时 5 折 全场包邮", [100, 100, 1900, 320])])
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "promo_text_area")
        self.assertAlmostEqual(verdict["evidence"]["outside_area_ratio"], 0.099, places=3)

    def test_area_threshold_boundary_is_inclusive(self):
        """2% 阈值边界：正好 2% → fail；1.99% → 交 VL 裁量。"""
        _, verdict = self.judge_overlay([("角标", [0, 0, 400, 200])])       # 80000/4e6
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "promo_text_area")

        gw, verdict = self.judge_overlay(
            [("角标", [0, 0, 398, 200])],                                   # 79600/4e6
            vl={"叠加遮挡物": {"undetermined": False, "raw": "否",
                            "rules": [{"status": "fail", "label": "overlay",
                                       "score": 0.0}]}})
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["evidence"]["source"], "ocr+vl")
        self.assertEqual(gw.count("/vl/judge"), 1)                          # 只剩 R3 裁量（R0 已确定性）

    def test_small_text_passes_when_vl_says_label(self):
        _, verdict = self.judge_overlay(
            [("角标", [0, 0, 200, 100])],
            vl={"叠加遮挡物": {"undetermined": False, "raw": "是",
                            "rules": [{"status": "pass", "label": "clean",
                                       "score": 0.8}]}})
        self.assertEqual(verdict["status"], "pass")
        self.assertEqual(verdict["note"], "small_text_vl_pass")

    def test_vl_undetermined_is_not_turned_into_pass(self):
        _, verdict = self.judge_overlay(
            [("角标", [0, 0, 200, 100])],
            vl={"叠加遮挡物": {"undetermined": True, "reason": "timeout", "raw": ""}})
        self.assertEqual(verdict["status"], "undetermined")
        self.assertEqual(verdict["note"], "vl_undetermined")

    def judge_overlay_multi(self, lines, *, instances, dominant, info, bbox_extra=None,
                            vl=None):
        """多实例宿主（`/detect/bbox` 带 `instances`）的 R3 判定。"""
        body = {"dominant_bbox": dominant}
        if instances:
            body["bbox"] = [min(b[0] for b in instances), min(b[1] for b in instances),
                            max(b[2] for b in instances), max(b[3] for b in instances)]
            body["instances"] = [{"bbox": b, "area_ratio": 0.1} for b in instances]
        body.update(bbox_extra or {})
        gw = self.use(info=info, bbox=body,
                      ocr={"lines": [{"text": t, "box": b, "score": 0.6} for t, b in lines]},
                      vl=vl or {})
        result = PLUGIN.judge("job-r3m", {"id": "r3m", "path": "/tmp/r3m.png"},
                              rules=["R3_no_watermark"])
        return gw, result["verdicts"][0]

    def test_second_product_text_passes_with_multi_instance_subjects(self):
        """K07 f002 实测几何：胶囊簇是第二个实例，其型号码不是「主体外促销条」。

        单主体口径下三行 OCR 合计 3.39% ≥ 2% → 误判 fail（f002 被误判的就是这条路径）；
        宿主给出 instances 后按实例集合判 → pass，且 evidence 里能数出 2 个商品实例。
        """
        info = {"width": 3840, "height": 2564}
        lines = [("an", [630, 1368, 1106, 1808]),
                 ("NEEN", [597, 1516, 883, 1822]),
                 ("DO00W", [917, 1635, 1124, 1814])]
        instances = [[1104, 56, 3492, 2428], [468, 1306, 1296, 1931]]
        gw, verdict = self.judge_overlay_multi(lines, instances=instances,
                                              dominant=[1104, 56, 3492, 2428], info=info)
        self.assertEqual(verdict["status"], "pass", verdict)
        self.assertEqual(verdict["note"], "subject_label_text")
        self.assertEqual(verdict["evidence"]["subject_count"], 2)
        self.assertEqual(verdict["evidence"]["subject_boxes"], instances)
        self.assertEqual(verdict["evidence"]["outside_area_ratio"], 0.0)
        self.assertEqual(verdict["evidence"]["excluded_label_boxes"], 3)
        # 确定性档：R3 不问 VL（只有请求里的 R3，一条 /vl/judge 都不该有）
        self.assertEqual(gw.count("/vl/judge"), 0)
        # 判定与修复同源：这些框不该进 inpaint 掩码
        self.assertEqual(PLUGIN.ruleset.overlay_boxes(
            [{"text": t, "box": b} for t, b in lines], width=3840, height=2564,
            subject_bbox=[1104, 56, 3492, 2428], subject_boxes=instances), [])

    def test_old_host_without_instances_keeps_single_subject_semantics(self):
        """老宿主（应答没有 `instances`）行为不变：同一批文字仍按单主体判 fail。"""
        info = {"width": 3840, "height": 2564}
        lines = [("an", [630, 1368, 1106, 1808]),
                 ("NEEN", [597, 1516, 883, 1822]),
                 ("DO00W", [917, 1635, 1124, 1814])]
        _, verdict = self.judge_overlay_multi(lines, instances=None,
                                              dominant=[1104, 56, 3492, 2428], info=info)
        self.assertEqual(verdict["status"], "fail")
        self.assertEqual(verdict["note"], "promo_text_area")
        # 缺字段 → 退回单主体（evidence 里只有主主体一个「实例」）
        self.assertEqual(verdict["evidence"]["subject_count"], 1)
        self.assertEqual(verdict["evidence"]["subject_boxes"], [[1104, 56, 3492, 2428]])

    def test_old_host_without_bbox_judges_by_area_only(self):
        """旧宿主无主体框：不能假装能排除吊牌，只能按面积判（并把来源标出来）。"""
        _, verdict = self.judge_overlay([("款号 12345", [700, 900, 950, 960])],
                                        bbox=None)
        self.assertEqual(verdict["status"], "pass")      # 面积 0.375% → 交 VL；VL 默认 pass
        self.assertEqual(verdict["evidence"]["source"], "ocr+vl")
        self.assertEqual(verdict["evidence"]["subject_bbox"], None)


class JudgeErrorTest(Base):
    def test_missing_path_is_invalid_params(self):
        self.use()
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.judge("job-e1", {})
        self.assertEqual(ctx.exception.code, -32003)

    def test_gateway_unreadable_propagates(self):
        self.use(errors={"/io/read": ruleset.RpcError(-32002, "asset_unreadable")})
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.judge("job-e2", {"id": "x", "path": "/nope.png"})
        self.assertEqual(ctx.exception.code, -32002)

    def test_capability_missing_is_not_swallowed(self):
        # 边界区间 → R0 必须问 VL；VL 能力缺失是 -32001（不许静默降级成 pass）
        self.use(u2net={"fg_ratio": 0.05},
                 errors={"/vl/judge": ruleset.RpcError(-32001, "capability_missing")})
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.judge("job-e3", {"id": "x", "path": "/tmp/x.png"})
        self.assertEqual(ctx.exception.code, -32001)

    def test_cancel_before_start(self):
        self.use()
        PLUGIN._cancel_flag("job-cxl").set()
        with self.assertRaises(ruleset.RpcError) as ctx:
            PLUGIN.judge("job-cxl", {"id": "x", "path": "/tmp/x.png"})
        self.assertEqual(ctx.exception.code, -32004)

    def test_cancelled_job_emits_nothing(self):
        gw = self.use()
        PLUGIN._cancel_flag("job-cxl2").set()
        with self.assertRaises(ruleset.RpcError):
            PLUGIN.judge("job-cxl2", {"id": "x", "path": "/tmp/x.png"})
        self.assertEqual(gw.count("/report/emit"), 0)


class ReportTest(Base):
    def test_jobs_do_not_mix(self):
        self.use()
        PLUGIN.judge("job-A", {"id": "asset-A", "path": "/tmp/A.png"})
        PLUGIN.judge("job-B", {"id": "asset-B", "path": "/tmp/B.png"})
        rep_a = PLUGIN.report("job-A")
        rep_b = PLUGIN.report("job-B")
        self.assertEqual(rep_a["assets"], ["asset-A"])
        self.assertEqual(rep_b["assets"], ["asset-B"])
        self.assertEqual(rep_a["rows"], 6)
        self.assertEqual(rep_a["entries"][0]["assetId"], "asset-A")

    def test_csv_format(self):
        self.use()
        PLUGIN.judge("job-C", {"id": "asset-C", "path": "/tmp/C.png"})
        payload = PLUGIN.report("job-C", "csv")
        lines = payload["content"].strip().splitlines()
        self.assertEqual(lines[0], "asset_id,ruleId,status,score")
        self.assertEqual(len(lines), 7)

    def test_empty_job_report_is_empty_not_error(self):
        payload = PLUGIN.report("job-none")
        self.assertEqual(payload["assets"], [])
        self.assertEqual(payload["rows"], 0)


if __name__ == "__main__":
    unittest.main()
