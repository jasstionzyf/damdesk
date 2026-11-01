"""插件全链路单测：按协议拉起子进程 + 可编排网关桩，考规则语义与三态口径。

覆盖（与 conformance 的分工：那边只考协议面，这边考业务语义）：
- F0 领域 gate 短路（非房源图 → 后续规则 skipped，不产生 VL/像素开销）
- F1 清晰 / F2 曝光 的 pass 与 fail 分界
- F3 重复（软规则：fail + 仅标记）、组内无可比对成员 → undetermined
- F4 场景齐套 pass / 缺场景 fail / 归类不可用 → undetermined
- F5 尺寸不足 → 几何 upscale 修复落盘（禁生成式）
- 不可修复的失败 → 转人工（undetermined + needs_human），**不自动放行**
- 能力白名单 403 → -32001；参数/资产/未知方法错误码
- job 归账隔离 + 确定性规则复跑同结论
"""
from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import _support   # noqa: E402

FIXTURES = _support.FIXTURES


def _copy(tmp, name, as_name=None):
    target = os.path.join(tmp, as_name or name)
    shutil.copyfile(os.path.join(FIXTURES, name), target)
    return target


def _statuses(verdicts):
    return {v["ruleId"]: v["status"] for v in verdicts}


class DomainGateTest(unittest.TestCase):
    def test_domain_fail_short_circuits_everything(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "sharp_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims,
                              domain={os.path.basename(path): {"raw": "否"}}) as pack:
                reply = pack.call("run", {"job_id": "g1",
                                            "asset": _support.asset(path),
                                            "options": {"fix_policy": "judge_only"}})
            verdicts = reply["result"]["verdicts"]
            statuses = _statuses(verdicts)
            self.assertEqual(statuses["F0_domain"], "fail")
            for rule_id in ("F1_sharpness", "F2_exposure", "F3_duplicate",
                            "F4_scene_complete", "F5_min_size"):
                self.assertEqual(statuses[rule_id], "skipped", rule_id)
            gate = [v for v in verdicts if v["ruleId"] == "F1_sharpness"][0]
            self.assertEqual(gate["note"], "domain_gate_fail")

    def test_domain_undetermined_also_blocks(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "sharp_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims, scenes={},
                              domain={os.path.basename(path): {"undetermined": True}}) as pack:
                reply = pack.call("run", {"job_id": "g2",
                                           "asset": _support.asset(path),
                                           "options": {"fix_policy": "judge_only"}})
            statuses = _statuses(reply["result"]["verdicts"])
            self.assertEqual(statuses["F0_domain"], "undetermined")
            self.assertEqual(statuses["F1_sharpness"], "skipped")


class ClassicRulesTest(unittest.TestCase):
    def test_sharp_passes_blur_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            sharp = _copy(tmp, "sharp444_640x480.jpg")
            blur = _copy(tmp, "blur_640x480.jpg")
            dims = {os.path.abspath(sharp): (640, 480), os.path.abspath(blur): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                sharp_reply = pack.call("run", {
                    "job_id": "j-sharp", "asset": _support.asset(sharp),
                    "rules": ["F1_sharpness"], "options": {"fix_policy": "judge_only"}})
                blur_reply = pack.call("run", {
                    "job_id": "j-blur", "asset": _support.asset(blur),
                    "rules": ["F1_sharpness"], "options": {"fix_policy": "judge_only"}})
            sharp_v = sharp_reply["result"]["verdicts"][0]
            blur_v = blur_reply["result"]["verdicts"][0]
            self.assertEqual(sharp_v["status"], "pass")
            self.assertGreater(sharp_v["evidence"]["variance"], 100.0)
            self.assertEqual(blur_v["status"], "fail")
            self.assertLess(blur_v["evidence"]["variance"], 100.0)
            self.assertEqual(blur_v["note"], "below_sharpness_min")

    def test_exposure_whiteout_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "whiteout_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-exp",
                                           "asset": _support.asset(path),
                                           "rules": ["F2_exposure"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "fail")
            self.assertEqual(verdict["evidence"]["dead_ratio"], 1.0)

    def test_classic_rules_are_deterministic_across_runs(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "dup_a_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                first = pack.call("run", {"job_id": "j1", "asset": _support.asset(path),
                                           "rules": ["F1_sharpness", "F2_exposure"],
                                           "options": {"fix_policy": "judge_only"}})
                second = pack.call("run", {"job_id": "j2", "asset": _support.asset(path),
                                            "rules": ["F1_sharpness", "F2_exposure"],
                                            "options": {"fix_policy": "judge_only"}})
            self.assertEqual(first["result"]["verdicts"], second["result"]["verdicts"])


class GroupRulesTest(unittest.TestCase):
    def test_duplicate_pair_is_soft_marked(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = _copy(tmp, "dup_a_640x480.jpg")
            _copy(tmp, "dup_b_640x480.jpg")
            dims = {os.path.abspath(a): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-dup",
                                           "asset": _support.asset(a),
                                           "rules": ["F3_duplicate"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "fail")
            self.assertTrue(verdict["evidence"]["soft_rule"])
            self.assertLessEqual(verdict["evidence"]["min_distance"], 8)
            self.assertTrue(verdict["evidence"]["nearest"])

    def test_lonely_asset_is_undetermined_not_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "sharp_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-lonely",
                                           "asset": _support.asset(path),
                                           "rules": ["F3_duplicate"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "undetermined")
            self.assertEqual(verdict["note"], "no_comparable_sibling")

    def _scene_group(self, tmp, scenes):
        paths = []
        for index, scene in enumerate(scenes):
            source = ["sharp444_640x480.jpg", "dup_a_640x480.jpg",
                      "dup_b_640x480.jpg", "small_320x240.jpg"][index % 4]
            paths.append(_copy(tmp, source, "room%d_%s.jpg" % (index, scene)))
        dims = {}
        for path, scene in zip(paths, scenes):
            dims[os.path.abspath(path)] = (640, 480) if "320" not in path else (320, 240)
        mapping = {os.path.basename(path): scene for path, scene in zip(paths, scenes)}
        return paths[0], dims, mapping

    def test_scene_complete_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, dims, mapping = self._scene_group(
                tmp, ["客厅", "卧室", "厨房", "卫生间"])
            with _support.Pack(tmp, dims=dims, scenes=mapping) as pack:
                reply = pack.call("run", {"job_id": "j-scene",
                                           "asset": _support.asset(path),
                                           "rules": ["F4_scene_complete"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "pass")
            self.assertEqual(sorted(verdict["evidence"]["seen"]),
                             sorted(["客厅", "卧室", "厨房", "卫生间"]))

    def test_scene_missing_fails_with_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, dims, mapping = self._scene_group(tmp, ["客厅", "卧室"])
            with _support.Pack(tmp, dims=dims, scenes=mapping) as pack:
                reply = pack.call("run", {"job_id": "j-scene2",
                                           "asset": _support.asset(path),
                                           "rules": ["F4_scene_complete"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "fail")
            self.assertEqual(sorted(verdict["evidence"]["missing"]), ["卫生间", "厨房"])

    def test_scene_unparsable_is_undetermined(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, dims, _ = self._scene_group(tmp, ["客厅", "卧室"])
            with _support.Pack(tmp, dims=dims, scenes={}) as pack:
                reply = pack.call("run", {"job_id": "j-scene3",
                                           "asset": _support.asset(path),
                                           "rules": ["F4_scene_complete"],
                                           "options": {"fix_policy": "judge_only"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "undetermined")
            self.assertEqual(verdict["note"], "group_classification_incomplete")


class RepairTest(unittest.TestCase):
    def test_small_asset_is_upscaled_and_marked_fixed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "small_320x240.jpg")
            dims = {os.path.abspath(path): (320, 240)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-fix",
                                            "asset": _support.asset(path),
                                            "rules": ["F5_min_size"],
                                            "options": {"workdir": os.path.join(tmp, "out"), "fix_policy": "auto"}})
            result = reply["result"]
            self.assertEqual(len(result["artifacts"]), 1)
            artifact = result["artifacts"][0]
            self.assertTrue(os.path.isfile(artifact["path"]))
            self.assertGreaterEqual(max(artifact["width"], artifact["height"]), 800)
            self.assertGreaterEqual(min(artifact["width"], artifact["height"]), 600)
            self.assertEqual(artifact["kind"], "geometric_upscale")
            verdict = result["verdicts"][0]
            self.assertEqual(verdict["status"], "pass")
            self.assertTrue(verdict["fixed"])
            # 只走几何：全程没有 generative 端点调用
            used = {r["path"] for r in pack.gateway.requests}
            self.assertNotIn("/op/replace_bg", used)
            self.assertNotIn("/op/inpaint", used)

    def test_unrepairable_failure_goes_to_human(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "blur_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-human",
                                            "asset": _support.asset(path),
                                            "rules": ["F0_domain", "F1_sharpness"],
                                            "options": {"workdir": os.path.join(tmp, "out"), "fix_policy": "auto"}})
            result = reply["result"]
            self.assertEqual(result["artifacts"], [])
            verdict = [v for v in result["verdicts"] if v["ruleId"] == "F1_sharpness"][0]
            self.assertEqual(verdict["status"], "undetermined")
            self.assertTrue(verdict["note"].startswith("needs_human:"))
            self.assertEqual(verdict["label"], "manual_review_required")

    def test_soft_duplicate_is_not_escalated(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = _copy(tmp, "dup_a_640x480.jpg")
            _copy(tmp, "dup_b_640x480.jpg")
            dims = {os.path.abspath(a): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                reply = pack.call("run", {"job_id": "j-soft",
                                            "asset": _support.asset(a),
                                            "rules": ["F3_duplicate"],
                                            "options": {"workdir": os.path.join(tmp, "out"), "fix_policy": "auto"}})
            verdict = reply["result"]["verdicts"][0]
            self.assertEqual(verdict["status"], "fail")
            self.assertTrue(verdict["evidence"]["soft_rule"])


class ProtocolEdgesTest(unittest.TestCase):
    def test_capability_denied_maps_to_minus_32001(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "sharp_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            with _support.Pack(tmp, dims=dims, deny=("vl.judge",)) as pack:
                reply = pack.call("run", {"job_id": "j-deny",
                                           "asset": _support.asset(path),
                                           "options": {"fix_policy": "judge_only"}})
            self.assertEqual(reply["error"]["code"], -32001)

    def test_error_codes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _copy(tmp, "sharp_640x480.jpg")
            dims = {os.path.abspath(path): (640, 480)}
            missing = os.path.join(tmp, "nope.jpg")
            with _support.Pack(tmp, dims=dims) as pack:
                bad_params = pack.call("run", {"job_id": "e1", "asset": {},
                                               "options": {"fix_policy": "judge_only"}})
                missing_asset = pack.call("run", {"job_id": "e2",
                                                   "asset": _support.asset(missing),
                                                   "options": {"fix_policy": "judge_only"}})
                unknown = pack.call("no_such_method", {})
            self.assertEqual(bad_params["error"]["code"], -32003)
            self.assertEqual(missing_asset["error"]["code"], -32002)
            self.assertEqual(unknown["error"]["code"], -32601)

    def test_report_ledger_is_per_job(self):
        with tempfile.TemporaryDirectory() as tmp:
            a = _copy(tmp, "sharp444_640x480.jpg")
            b = _copy(tmp, "dup_a_640x480.jpg")
            dims = {os.path.abspath(a): (640, 480), os.path.abspath(b): (640, 480)}
            with _support.Pack(tmp, dims=dims) as pack:
                pack.call("run", {"job_id": "A", "asset": _support.asset(a, "a1"),
                                   "rules": ["F1_sharpness"],
                                   "options": {"fix_policy": "judge_only"}})
                pack.call("run", {"job_id": "B", "asset": _support.asset(b, "b1"),
                                   "rules": ["F1_sharpness"],
                                   "options": {"fix_policy": "judge_only"}})
                report_a = pack.call("report", {"job_id": "A"})
            self.assertEqual(report_a["result"]["assets"], ["a1"])
            self.assertEqual(len(pack.gateway.emits_for("A")), 1)
            self.assertEqual(len(pack.gateway.emits_for("B")), 1)


if __name__ == "__main__":
    unittest.main()
