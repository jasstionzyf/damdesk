#!/usr/bin/env python3
"""几何组单测（N6a）：纯几何对语料真值 + 动作层对桩网关。

跑法：
    python3 packs/plugins/imaging/tests/test_geometry.py            # 直接跑
    cd packs/plugins/imaging/tests && python3 -m unittest -v test_geometry
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
PLUGIN = os.path.join(PACK, "plugin")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import geometry                                    # noqa: E402
from gw import GatewayError                        # noqa: E402

CORPUS = os.environ.get("DAM_CORPUS", "")
LABELS_RESIZE = os.path.join(CORPUS, "labels", "resize.json")
LABELS_CROP = os.path.join(CORPUS, "labels", "smart_crop.json")


def _load_items(path):
    if not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)["items"]


def _round_half_up(x):
    import math
    return int(math.floor(float(x) + 0.5))


class ParseAspectTest(unittest.TestCase):
    def test_basic(self):
        self.assertAlmostEqual(geometry.parse_aspect("1:1"), 1.0)
        self.assertAlmostEqual(geometry.parse_aspect("4:3"), 4 / 3)
        self.assertAlmostEqual(geometry.parse_aspect("16:9"), 16 / 9)
        self.assertAlmostEqual(geometry.parse_aspect("4：3"), 4 / 3)  # 全角冒号
        self.assertAlmostEqual(geometry.parse_aspect(1.5), 1.5)

    def test_invalid(self):
        for bad in ("", "4:x", "0:1", "-1:2", "1:0"):
            with self.assertRaises(ValueError):
                geometry.parse_aspect(bad)


class PlanResizeTest(unittest.TestCase):
    def test_corpus_long_edge_1600(self):
        """语料 10 张：长边 1600 等比 → 计划尺寸必须与 labels.expect.out_size 逐像素相等。"""
        items = _load_items(LABELS_RESIZE)
        if items is None:
            self.skipTest("无语料 labels（%s）" % LABELS_RESIZE)
        self.assertEqual(len(items), 10)
        for it in items:
            exp = it["expect"]
            plan = geometry.plan_resize(exp["in_size"], {"resizeMode": "long_edge",
                                                         "resizeLongEdge": 1600})
            self.assertEqual(plan["out"], exp["out_size"], it["file"])
            self.assertIsNone(plan["skip"], it["file"])
            # 只缩不放：产物两条边都不许超过源图
            self.assertLessEqual(plan["out"][0], exp["in_size"][0], it["file"])
            self.assertLessEqual(plan["out"][1], exp["in_size"][1], it["file"])

    def test_no_upscale_default(self):
        plan = geometry.plan_resize((800, 600), {"resizeMode": "long_edge", "resizeLongEdge": 1600})
        self.assertEqual(plan["skip"], "no_upscale")
        self.assertEqual(plan["out"], [800, 600])

    def test_allow_upscale(self):
        plan = geometry.plan_resize((800, 600), {"resizeMode": "long_edge",
                                                 "resizeLongEdge": 1600,
                                                 "resizeAllowUpscale": True})
        self.assertIsNone(plan["skip"])
        self.assertEqual(plan["out"], [1600, 1200])

    def test_already_satisfied(self):
        plan = geometry.plan_resize((1600, 1200), {"resizeMode": "long_edge",
                                                   "resizeLongEdge": 1600})
        self.assertEqual(plan["skip"], "already_satisfied")

    def test_short_edge(self):
        plan = geometry.plan_resize((3000, 1000), {"resizeMode": "short_edge",
                                                   "resizeShortEdge": 800})
        self.assertEqual(plan["out"], [2400, 800])

    def test_width_height_scale(self):
        self.assertEqual(geometry.plan_resize((3000, 2000), {"resizeMode": "width",
                                                             "resizeWidth": 1200})["out"],
                         [1200, 800])
        self.assertEqual(geometry.plan_resize((3000, 2000), {"resizeMode": "height",
                                                             "resizeHeight": 1000})["out"],
                         [1500, 1000])
        self.assertEqual(geometry.plan_resize((3000, 2000), {"resizeMode": "scale",
                                                             "resizeScale": 0.5})["out"],
                         [1500, 1000])

    def test_rounding_matches_rust_half_up(self):
        # 3782x8799 → 长边 1600：短边 3782*1600/8799 = 687.57 → 688（不是银行家舍入的 688/687 抖动）
        self.assertEqual(geometry.plan_resize((3782, 8799), {"resizeLongEdge": 1600})["out"],
                         [688, 1600])

    def test_bad_params(self):
        with self.assertRaises(ValueError):
            geometry.plan_resize((100, 100), {"resizeMode": "nope"})
        with self.assertRaises(ValueError):
            geometry.plan_resize((100, 100), {"resizeMode": "long_edge", "resizeLongEdge": 0})


class PlanCropTest(unittest.TestCase):
    def _check_plan(self, plan, w, h, aspect, subject):
        self.assertAlmostEqual(plan["aspect"], round(aspect, 6), places=5)
        bx1, by1, bx2, by2 = plan["box"]
        self.assertTrue(0 <= bx1 < bx2 <= w, plan["box"])
        self.assertTrue(0 <= by1 < by2 <= h, plan["box"])
        if subject:
            x1, y1, x2, y2 = subject
            self.assertLessEqual(bx1, x1, "主体左侧被切")
            self.assertLessEqual(by1, y1, "主体上侧被切")
            self.assertGreaterEqual(bx2, x2, "主体右侧被切")
            self.assertGreaterEqual(by2, y2, "主体下侧被切")
        ow, oh = plan["out"]
        self.assertLessEqual(abs(ow / float(oh) - aspect), 1.0 / min(ow, oh) + 1e-9)

    def test_corpus_smart_crop(self):
        """语料 10 张：比例正确 + 主体完整落在结果内（裁不下就补边，绝不切主体）。"""
        items = _load_items(LABELS_CROP)
        if items is None:
            self.skipTest("无语料 labels（%s）" % LABELS_CROP)
        self.assertEqual(len(items), 10)
        feasible = 0
        for it in items:
            exp = it["expect"]
            aspect = geometry.parse_aspect(exp["aspect"])
            plan = geometry.plan_crop(exp["in_size"], {"cropAspect": exp["aspect"]},
                                      exp.get("subject_bbox"))
            self.assertTrue(plan["subjectCovered"], it["file"])
            self._check_plan(plan, exp["in_size"][0], exp["in_size"][1], aspect,
                             exp.get("subject_bbox"))
            if plan["mode"] == "crop":
                feasible += 1
                self.assertEqual(plan["out"], [plan["box"][2] - plan["box"][0],
                                               plan["box"][3] - plan["box"][1]])
                self.assertIsNone(plan["pad"])
            else:
                self.assertEqual(plan["mode"], "crop+pad")
                self.assertIsNotNone(plan["pad"])
                # 补边后画布必须罩住「紧裁框 + 居中偏移」
                tw = plan["box"][2] - plan["box"][0]
                th = plan["box"][3] - plan["box"][1]
                ox, oy = plan["padOffset"]
                self.assertGreaterEqual(plan["out"][0], tw)
                self.assertGreaterEqual(plan["out"][1], th)
                self.assertTrue(0 <= ox <= plan["out"][0] - tw)
                self.assertTrue(0 <= oy <= plan["out"][1] - th)
        # 语料事实：10 张里 2 张能「只裁不补」，其余 8 张必须补边（主体 + 5% 留白放不进
        # 比例正确的窗口）。不是实现偏差：语料把横图裁成 1:1 / 竖图裁成 4:3，多数无解。
        self.assertEqual(feasible, 2, "语料可行/不可行分布变了，先核对 labels")

    def test_horizontal_anchors(self):
        """水平锚点：宽图（2000x1000，1:1 → 窗口 1000²，x ∈ [600,1000] 为保主体可行区间）。

        `right` = 主体贴结果右边 → 窗口左移取 600；`left` = 主体贴左 → 窗口右移取 1000。
        """
        size, subject = (2000, 1000), [1400, 300, 1600, 700]
        plans = {}
        for anchor in ("center", "left", "right"):
            plans[anchor] = geometry.plan_crop(size, {"cropAspect": "1:1", "cropAnchor": anchor,
                                                     "cropPadding": 0.0}, subject)
            self._check_plan(plans[anchor], 2000, 1000, 1.0, subject)
        self.assertLess(plans["right"]["box"][0], plans["center"]["box"][0])
        self.assertLessEqual(plans["center"]["box"][0], plans["left"]["box"][0])
        self.assertEqual(plans["right"]["box"][2], 1600)   # 窗口右边 = 主体右边（贴右）
        self.assertEqual(plans["right"]["box"][0], 600)
        self.assertEqual(plans["center"]["box"][0], 1000)  # 夹到上界（主体本就贴右边缘）
        self.assertEqual(plans["left"]["box"][0], 1000)

    def test_vertical_anchors(self):
        """垂直锚点：竖图（1000x2000，1:1 → 窗口 1000²，y ∈ [600,1000] 为保主体可行区间）。"""
        size, subject = (1000, 2000), [300, 1400, 700, 1600]
        plans = {}
        for anchor in ("center", "top", "bottom"):
            plans[anchor] = geometry.plan_crop(size, {"cropAspect": "1:1", "cropAnchor": anchor,
                                                     "cropPadding": 0.0}, subject)
            self._check_plan(plans[anchor], 1000, 2000, 1.0, subject)
        self.assertGreater(plans["top"]["box"][1], plans["bottom"]["box"][1])
        self.assertEqual(plans["bottom"]["box"][1], 600)   # 窗口下边 = 主体下边（贴下）
        self.assertEqual(plans["top"]["box"][1], 1000)     # 夹到上界（主体本就贴下边缘）

    def test_no_subject_is_plain_aspect_crop(self):
        plan = geometry.plan_crop((2000, 1000), {"cropAspect": "1:1"})
        self.assertEqual(plan["mode"], "crop")
        self.assertFalse(plan["subjectDetected"])
        self.assertEqual(plan["out"], [1000, 1000])
        self._check_plan(plan, 2000, 1000, 1.0, None)

    def test_custom_aspect(self):
        plan = geometry.plan_crop((3000, 2000), {"cropAspect": "custom",
                                                 "cropAspectCustom": "3:5"})
        self.assertAlmostEqual(plan["aspect"], 0.6, places=6)
        self._check_plan(plan, 3000, 2000, 0.6, None)

    def test_padding_keeps_margin(self):
        subject = [800, 500, 1200, 900]
        tight = geometry.plan_crop((2000, 2000), {"cropAspect": "1:1", "cropPadding": 0.0},
                                   subject)
        loose = geometry.plan_crop((2000, 2000), {"cropAspect": "1:1", "cropPadding": 0.1},
                                   subject)
        self.assertLessEqual(tight["subjectMargin"][0], loose["subjectMargin"][0])
        self._check_plan(loose, 2000, 2000, 1.0, subject)

    def test_bad_padding(self):
        with self.assertRaises(ValueError):
            geometry.plan_crop((100, 100), {"cropPadding": 0.5})


class StubGateway:
    """桩网关：按宿主同一口径算尺寸，并记录调用序列（不做真解码）。"""

    def __init__(self, size=(2000, 1000), subject=(400, 200, 1600, 800), fail=None):
        self.size = size
        self.subject = subject
        self.fail = fail or {}
        self.calls = []
        self.written = {}

    def _maybe_fail(self, path):
        if path in self.fail:
            raise self.fail[path]

    def io_read(self, path):
        self.calls.append(("/io/read", {"path": path}))
        self._maybe_fail("/io/read")
        return {"path": path, "width": self.size[0], "height": self.size[1], "sha256": "0" * 64}

    def detect_bbox(self, path, target_ratio=None, min_instance_ratio=None):
        self.calls.append(("/detect/bbox", {"path": path}))
        self._maybe_fail("/detect/bbox")
        return {"path": path, "width": self.size[0], "height": self.size[1],
                "bbox": list(self.subject) if self.subject else None,
                "dominant_bbox": list(self.subject) if self.subject else None,
                "instances": [list(self.subject)] if self.subject else []}

    def crop(self, path, box, out):
        self.calls.append(("/op/crop", {"box": list(box), "out": out}))
        self._maybe_fail("/op/crop")
        w, h = int(box[2]) - int(box[0]), int(box[3]) - int(box[1])
        self.written[out] = (w, h)
        with open(out, "wb") as fh:
            fh.write(b"stub-crop")
        return {"path": out, "width": w, "height": h, "op": "crop"}

    def letterbox(self, path, target_ratio, out, background=None):
        self.calls.append(("/op/letterbox", {"target_ratio": target_ratio, "out": out}))
        self._maybe_fail("/op/letterbox")
        w, h = self.written[path]
        if w / float(h) > target_ratio:
            cw, ch = w, _round_half_up(w / float(target_ratio))
        else:
            cw, ch = _round_half_up(h * float(target_ratio)), h
        self.written[out] = (cw, ch)
        with open(out, "wb") as fh:
            fh.write(b"stub-pad")
        return {"path": out, "width": cw, "height": ch, "op": "letterbox",
                "target_ratio": target_ratio}

    def upscale(self, path, scale, out):
        self.calls.append(("/op/upscale", {"scale": scale, "out": out}))
        self._maybe_fail("/op/upscale")
        w = max(1, _round_half_up(self.size[0] * float(scale)))
        h = max(1, _round_half_up(self.size[1] * float(scale)))
        self.written[out] = (w, h)
        with open(out, "wb") as fh:
            fh.write(b"stub-resize")
        return {"path": out, "width": w, "height": h, "op": "upscale.resample"}


class ResizeActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="geom-resize-")

    def test_ok_row_has_no_verdict(self):
        gw = StubGateway(size=(3200, 2400))
        row = geometry.resize_action({"id": "a1", "path": "/in/a1.jpg"},
                                     {"resizeLongEdge": 1600}, gw, self.tmp)
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row, "处理类不得回 verdict（N5a 契约）")
        self.assertEqual(row["fields"]["out"], [1600, 1200])
        self.assertEqual(row["assetId"], "a1")
        self.assertTrue(row["fields"]["outPath"].startswith(self.tmp))
        self.assertEqual([c[0] for c in gw.calls], ["/io/read", "/op/upscale"])
        self.assertEqual(row["evidence"]["opDetail"], "upscale.resample")
        self.assertTrue(row["evidence"]["sizeMatchesPlan"])
        self.assertTrue(row["evidence"]["noUpscale"])
        self.assertEqual(len(row["fields"]["sha256"]), 64)

    def test_skip_no_upscale(self):
        gw = StubGateway(size=(800, 600))
        row = geometry.resize_action({"id": "a2", "path": "/in/a2.jpg"},
                                     {"resizeLongEdge": 1600}, gw, self.tmp)
        self.assertEqual(row["result"], "skipped")
        self.assertEqual(row["skipReason"], "no_upscale")
        self.assertNotIn("/op/upscale", [c[0] for c in gw.calls])

    def test_missing_path(self):
        row = geometry.resize_action({"id": "a3"}, {}, StubGateway(), self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")

    def test_gateway_error_becomes_failed_row(self):
        gw = StubGateway(size=(3200, 2400),
                         fail={"/op/upscale": GatewayError(-32001, "capability_missing",
                                                           status=403, error="capability_missing",
                                                           capability="op.geometric")})
        row = geometry.resize_action({"id": "a4", "path": "/in/a4.jpg"},
                                     {"resizeLongEdge": 1600}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32001")


class CropActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="geom-crop-")

    def test_feasible_single_op(self):
        gw = StubGateway(size=(2000, 2000), subject=(500, 500, 1500, 1500))
        row = geometry.crop_action({"id": "c1", "path": "/in/c1.jpg"},
                                   {"cropAspect": "1:1"}, gw, self.tmp)
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row)
        self.assertEqual(row["fields"]["mode"], "crop")
        self.assertEqual([c[0] for c in gw.calls], ["/io/read", "/detect/bbox", "/op/crop"])
        self.assertEqual(row["fields"]["out"], [2000, 2000])
        self.assertTrue(row["fields"]["subjectCovered"])
        self.assertLess(row["evidence"]["aspectError"], 1e-6)

    def test_infeasible_pads_not_cuts(self):
        """主体竖长 + 1:1 → 裁不下就补边：产物仍是 1:1，主体完整且在画布内。"""
        gw = StubGateway(size=(946, 2200), subject=(117, 56, 828, 2148))
        row = geometry.crop_action({"id": "c2", "path": "/in/c2.jpg"},
                                   {"cropAspect": "1:1", "cropPadding": 0.02}, gw, self.tmp)
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["mode"], "crop+pad")
        self.assertEqual([c[0] for c in gw.calls],
                         ["/io/read", "/detect/bbox", "/op/crop", "/op/letterbox"])
        ow, oh = row["fields"]["out"]
        self.assertEqual(ow, oh, "1:1 补边后应为方形画布")
        self.assertGreaterEqual(ow, 828 - 117)
        self.assertTrue(row["fields"]["subjectCovered"])
        self.assertFalse(row["fields"]["feasible"])
        # 中间产物用完即删，只留一张产物
        self.assertFalse(os.path.exists(row["fields"]["outPath"].replace("crop-pad", "crop")))
        self.assertTrue(os.path.exists(row["fields"]["outPath"]))

    def test_detect_failure_is_not_swallowed(self):
        gw = StubGateway(fail={"/detect/bbox": GatewayError(-32001, "模型未安装", status=403,
                                                            error="capability_missing",
                                                            capability="detect.u2net")})
        row = geometry.crop_action({"id": "c3", "path": "/in/c3.jpg"}, {}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32001")
        self.assertIn("主体检测", row["error"]["message"])

    def test_subject_none_falls_back_to_full_frame(self):
        gw = StubGateway(size=(1600, 900), subject=None)
        row = geometry.crop_action({"id": "c4", "path": "/in/c4.jpg"},
                                   {"cropAspect": "1:1"}, gw, self.tmp)
        self.assertEqual(row["result"], "ok")
        self.assertFalse(row["fields"]["subjectDetected"])
        self.assertEqual(row["fields"]["out"], [900, 900])


class SchemaTest(unittest.TestCase):
    def test_schema_shape_matches_host_params_contract(self):
        props = geometry.SCHEMA["properties"]
        groups = {g["key"] for g in geometry.SCHEMA["groups"]}
        self.assertTrue(props)
        allowed_keys = {"type", "title", "default", "min", "max", "step", "options", "unit",
                        "accept", "group", "advanced", "when", "irreversible"}
        numeric = {"integer", "number", "range"}
        for key, spec in props.items():
            self.assertLessEqual(set(spec), allowed_keys, key)
            self.assertIn(spec["type"],
                          {"integer", "number", "boolean", "enum", "string", "color",
                           "asset_ref", "range"}, key)
            self.assertIn(spec["group"], groups, key)
            self.assertTrue(spec["title"], key)
            if spec["type"] == "enum":
                self.assertTrue(spec["options"], key)
                self.assertIn(spec["default"], spec["options"], key)
            if spec["type"] not in numeric:
                self.assertFalse({"min", "max", "step"} & set(spec), key)
            for dep, values in (spec.get("when") or {}).items():
                self.assertIn(dep, props, "%s.when 指向未声明参数 %s" % (key, dep))
                self.assertTrue(values)
                self.assertIn(props[dep]["type"], {"enum", "boolean", "string"}, dep)

    def test_resize_mode_params_declared(self):
        props = geometry.SCHEMA["properties"]
        for mode, key in geometry.RESIZE_MODES.items():
            self.assertIn(key, props, mode)
            self.assertEqual(props[key]["when"], {"resizeMode": [mode]})
        self.assertEqual(set(geometry.SCHEMA["properties"]["resizeMode"]["options"]),
                         set(geometry.RESIZE_MODES))

    def test_resources_fragment_in_sync(self):
        """`resources/params.geometry.json` 必须与 `geometry.SCHEMA` 逐字一致（防漂移）。

        片段是 N6d 汇总进 `resources/params.schema.json` 的源；漂了就等于装上另一套参数。
        修法：`python3 packs/plugins/imaging/tests/dump_params_schema.py`
        """
        path = os.path.join(PACK, "resources", "params.geometry.json")
        self.assertTrue(os.path.isfile(path), "缺片段文件（跑 dump_params_schema.py 生成）")
        with open(path, encoding="utf-8") as fh:
            on_disk = fh.read()
        self.assertEqual(on_disk, json.dumps(geometry.SCHEMA, ensure_ascii=False, indent=1) + "\n")

    def test_schema_version_declared(self):
        self.assertEqual(geometry.SCHEMA["schemaVersion"], 1)

    def test_actions_registry(self):
        self.assertEqual(set(geometry.ACTIONS), set(geometry.ACTIONS.keys()))
        self.assertEqual(set(geometry.ACTIONS), {"resize", "crop"})
        for fn in geometry.ACTIONS.values():
            self.assertTrue(callable(fn))


if __name__ == "__main__":
    unittest.main(verbosity=2)
