#!/usr/bin/env python3
"""叠加组单测（N6c）：水印计划/动作 + 改命名/冲突 + 结果行/参数 schema 契约。

跑法：
    python3 packs/plugins/imaging/tests/test_overlay.py            # 直接跑
    cd packs/plugins/imaging/tests && python3 -m unittest -v test_overlay

分工：本文件的动作层用**桩网关**（毫秒级、无 cargo、无模型）；真网关 + 真像素的语料验收
在 `corpus_overlay_e2e.py`。这里刻意把「水印真的落墨/透明度真的生效」留给 e2e ——
单测能证明的只是「发给宿主的请求对、行形状对」，不能证明像素（不假装测过）。
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

import overlay                                     # noqa: E402
from gw import GatewayError                        # noqa: E402

_HOST_CORPUS = os.environ.get("DAM_CORPUS", "")


def _find_local_corpus():
    """env 未指路时，向上找 worktree 里拷好的那份语料。"""
    here = os.path.dirname(os.path.abspath(__file__))
    for _ in range(6):
        cand = os.path.join(here, "reports", "logs", "e2e-corpus", "v1")
        if os.path.isdir(cand):
            return cand
        here = os.path.dirname(here)
    return os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "reports", "logs", "e2e-corpus", "v1")


CORPUS = os.environ.get("DAM_CORPUS") or (
    _HOST_CORPUS if os.path.isdir(_HOST_CORPUS) else _find_local_corpus())
LABELS_WM = os.path.join(CORPUS, "labels", "watermark.json")

# 语料标签的短锚点 → 规范值（标签用人读的 br/bl/tr/tl）
LABEL_ANCHORS = {"br": "bottom_right", "bl": "bottom_left",
                 "tr": "top_right", "tl": "top_left",
                 "c": "center", "center": "center"}


def load_items(path):
    if not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)["items"]


# ---------------------------------------------------------------- 桩网关
class StubGateway:
    """最小网关桩：记录请求 → 按需写产物 → 回宿主同形状应答。

    `errors` 是「端点 → 异常」，用来锁「网关错不许被吞」。
    """

    def __init__(self, width=400, height=300, sha="a" * 64, write_out=True,
                 on_watermark=None, errors=None):
        self.width, self.height, self.sha = width, height, sha
        self.write_out = write_out
        self.on_watermark = on_watermark
        self.errors = errors or {}
        self.calls = []

    def _fail(self, endpoint):
        exc = self.errors.get(endpoint)
        if exc:
            raise exc

    def io_read(self, path):
        self.calls.append(("/io/read", {"path": path}))
        self._fail("io_read")
        return {"path": path, "size": 4096, "sha256": self.sha, "mime": "image/jpeg",
                "width": self.width, "height": self.height}

    def watermark(self, path, text, out, **kw):
        self.calls.append(("/op/watermark", dict(path=path, text=text, out=out, **kw)))
        self._fail("watermark")
        if self.write_out:
            with open(out, "wb") as fh:
                fh.write(b"\xff\xd8fake-jpeg")
        reply = {"path": out, "width": self.width, "height": self.height,
                 "op": "watermark", "coverage": 0.0123}
        if callable(self.on_watermark):
            reply = self.on_watermark(reply) or reply
        return reply


def asset(path="/library/ab/cd.jpg", asset_id="asset-42"):
    return {"id": asset_id, "path": path, "mime": "image/jpeg"}


# ---------------------------------------------------------------- 参数解析
class ParseAnchorTest(unittest.TestCase):
    def test_aliases(self):
        for raw, want in [("br", "bottom_right"), ("BL", "bottom_left"),
                          ("tl", "top_left"), ("tr", "top_right"), ("c", "center"),
                          ("bottom-right", "bottom_right"), ("右下", "bottom_right"),
                          ("Center", "center"), ("top_left", "top_left")]:
            self.assertEqual(overlay.parse_anchor(raw), want, raw)

    def test_default_and_invalid(self):
        self.assertEqual(overlay.parse_anchor(None), overlay.DEFAULT_ANCHOR)
        self.assertEqual(overlay.parse_anchor("  "), overlay.DEFAULT_ANCHOR)
        for bad in ("middle_out", "bottom", "0,0"):
            with self.assertRaises(ValueError):
                overlay.parse_anchor(bad)


class ParseOpacityTest(unittest.TestCase):
    def test_ratio_and_percent(self):
        self.assertEqual(overlay.parse_opacity(0.35), 35)      # 语料口径（比例）
        self.assertEqual(overlay.parse_opacity("0.5"), 50)
        self.assertEqual(overlay.parse_opacity(1), 100)
        self.assertEqual(overlay.parse_opacity(35), 35)        # 旧 YAML 口径（百分比）
        self.assertEqual(overlay.parse_opacity(100), 100)
        self.assertEqual(overlay.parse_opacity(None), 50)

    def test_half_up_rounding(self):
        self.assertEqual(overlay.parse_opacity(0.005), 1)      # 半上取整（下限口径）
        self.assertEqual(overlay.parse_opacity(0.345), 35)     # 34.5 → 35（银行家舍入会得 34）

    def test_rejects_no_ink_and_garbage(self):
        for bad in (0, 0.0, 0.001, 101, "abc", True, float("nan")):
            with self.assertRaises(ValueError, msg=repr(bad)):
                overlay.parse_opacity(bad)


class ParseColorTest(unittest.TestCase):
    def test_normalizes(self):
        self.assertEqual(overlay.parse_color("#AABBCC"), "aabbcc")
        self.assertEqual(overlay.parse_color("aabbcc"), "aabbcc")
        self.assertEqual(overlay.parse_color(None), "ffffff")

    def test_rejects(self):
        for bad in ("#fff", "gggggg", "#1234567", 123):
            with self.assertRaises(ValueError, msg=repr(bad)):
                overlay.parse_color(bad)


# ---------------------------------------------------------------- 水印计划
class PlanWatermarkTest(unittest.TestCase):
    def test_defaults(self):
        plan = overlay.plan_watermark({}, 400, 300)
        self.assertEqual(plan["text"], overlay.DEFAULT_TEXT)
        self.assertEqual(plan["position"], "bottom_right")
        self.assertEqual(plan["opacity"], 50)
        self.assertAlmostEqual(plan["opacityRatio"], 0.5)
        self.assertEqual(plan["font_size"], 48)
        self.assertEqual(plan["margin"], 16)
        self.assertEqual(plan["color"], "ffffff")

    def test_corpus_labels_drive_position_and_opacity(self):
        """语料 10 张标签（锚点 + 透明度 + 文字）→ 计划必须逐项对上（不碰像素）。"""
        items = load_items(LABELS_WM)
        if items is None:
            self.skipTest("语料未就绪：%s" % LABELS_WM)
        self.assertEqual(len(items), 10)
        for item in items:
            expect = item["expect"]
            plan = overlay.plan_watermark({
                "watermarkText": expect["text"],
                "watermarkAnchor": expect["anchor"],
                "watermarkOpacity": expect["opacity"],
            }, expect["in_size"][0], expect["in_size"][1])
            self.assertEqual(plan["position"], LABEL_ANCHORS[expect["anchor"]], item["file"])
            self.assertEqual(plan["opacity"], int(round(expect["opacity"] * 100)), item["file"])
            self.assertNotIn("#", plan["color"], item["file"])

    def test_text_rules(self):
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkText": "  "}, 400, 300)
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkText": "A\nB"}, 400, 300)
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkText": "A\rB"}, 400, 300)

    def test_font_size_and_margin_bounds(self):
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkFontSize": 7}, 400, 300)
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkFontSize": 513}, 400, 300)
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkMargin": -1}, 400, 300)
        # 2×边距 == 短边 → 无处可落（与宿主同门槛，提前报错而不是等网关 400）
        with self.assertRaises(ValueError):
            overlay.plan_watermark({"watermarkMargin": 150}, 400, 300)
        ok = overlay.plan_watermark({"watermarkMargin": 149}, 400, 300)
        self.assertEqual(ok["margin"], 149)

    def test_bad_canvas(self):
        with self.assertRaises(ValueError):
            overlay.plan_watermark({}, 0, 300)


# ---------------------------------------------------------------- 水印动作
class WatermarkActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="n6c-wm-")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_ok_row_has_no_verdict_and_real_paths(self):
        gw = StubGateway(width=860, height=2000)
        row = overlay.watermark_action(asset(), {"watermarkOpacity": 0.35}, gw, self.tmp)
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row)                 # 处理类不许伪造 pass
        self.assertEqual(row["assetId"], "asset-42")
        self.assertEqual(row["fields"]["anchor"], "bottom_right")
        self.assertAlmostEqual(row["fields"]["opacity"], 0.35)
        self.assertEqual(row["fields"]["opacityPct"], 35)
        self.assertEqual(row["fields"]["out"], [860, 2000])
        self.assertTrue(os.path.isfile(row["fields"]["outPath"]))
        self.assertEqual(len(row["fields"]["sha256"]), 64)
        self.assertEqual(row["evidence"]["ops"], ["/io/read", "/op/watermark"])
        self.assertTrue(row["evidence"]["sizeUnchanged"])
        self.assertAlmostEqual(row["evidence"]["coverage"], 0.0123)
        # 产物文件名确定性：同资产同动作 → 同路径（重跑覆盖，不堆垃圾）
        again = overlay.watermark_action(asset(), {"watermarkOpacity": 0.35}, gw, self.tmp)
        self.assertEqual(row["fields"]["outPath"], again["fields"]["outPath"])

    def test_request_payload_matches_plan(self):
        gw = StubGateway()
        overlay.watermark_action(asset(), {
            "watermarkText": "DAMDesk", "watermarkAnchor": "tl",
            "watermarkOpacity": 0.35, "watermarkFontSize": 64,
            "watermarkMargin": 24, "watermarkColor": "#102030",
        }, gw, self.tmp)
        path, payload = gw.calls[-1]
        self.assertEqual(path, "/op/watermark")
        self.assertEqual(payload["text"], "DAMDesk")
        self.assertEqual(payload["position"], "top_left")
        self.assertEqual(payload["opacity"], 35)         # 宿主口径是 1..100 整数
        self.assertEqual(payload["font_size"], 64)
        self.assertEqual(payload["margin"], 24)
        self.assertEqual(payload["color"], "102030")     # 宿主口径无 '#'
        self.assertTrue(payload["out"].startswith(self.tmp))

    def test_param_error_is_32003_and_no_gateway_call(self):
        gw = StubGateway()
        row = overlay.watermark_action(asset(), {"watermarkText": ""}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")
        self.assertEqual([c[0] for c in gw.calls], ["/io/read"])   # 参数错不碰像素算子

    def test_capability_missing_is_not_swallowed(self):
        gw = StubGateway(errors={"watermark": GatewayError(-32001, "capability_missing",
                                                           status=403, error="capability_missing",
                                                           capability="op.overlay")})
        row = overlay.watermark_action(asset(), {}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32001")
        self.assertIn("capability_missing", row["error"]["message"])

    def test_unreadable_source_is_32002(self):
        gw = StubGateway(errors={"io_read": GatewayError(-32002, "asset_unreadable", status=404)})
        row = overlay.watermark_action(asset(), {}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32002")

    def test_size_change_is_rejected(self):
        """叠加不是变形：宿主若回了不同尺寸，宁可失败也不交付一张偷缩的图。"""
        gw = StubGateway(on_watermark=lambda reply: dict(reply, width=1))
        row = overlay.watermark_action(asset(), {}, gw, self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32000")
        self.assertIn("尺寸", row["error"]["message"])

    def test_missing_path(self):
        row = overlay.watermark_action({"id": "a"}, {}, StubGateway(), self.tmp)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")


# ---------------------------------------------------------------- 改名计划
class RenderTemplateTest(unittest.TestCase):
    CTX = {"name": "a001", "origName": "a001", "ext": "jpg", "width": 860, "w": 860,
           "height": 2000, "h": 2000, "date": "20261022", "index": 3, "assetId": "asset-42"}

    def test_all_placeholders(self):
        out = overlay.render_template("{name}_{origName}_{w}x{h}_{width}x{height}_{date}"
                                      "_i{index}_{assetId}.{ext}", self.CTX)
        self.assertEqual(out, "a001_a001_860x2000_860x2000_20261022_i3_asset-42.jpg")

    def test_unknown_placeholder_is_loud(self):
        with self.assertRaises(ValueError) as ctx:
            overlay.render_template("{name}_{brand}", self.CTX)
        self.assertIn("brand", str(ctx.exception))

    def test_unbalanced_brace_is_loud(self):
        with self.assertRaises(ValueError):
            overlay.render_template("{name}_{width", self.CTX)

    def test_default_template(self):
        self.assertEqual(overlay.render_template(None, self.CTX), "a001_860x2000")


class SanitizeStemTest(unittest.TestCase):
    def test_path_separators_and_control_chars(self):
        self.assertEqual(overlay.sanitize_stem("a/b\\c"), "a_b_c")
        self.assertEqual(overlay.sanitize_stem("a\x00b\tc"), "a_b_c")

    def test_traversal_and_dots(self):
        self.assertEqual(overlay.sanitize_stem("../etc/passwd"), "etc_passwd")
        self.assertEqual(overlay.sanitize_stem(".hidden."), "hidden")
        self.assertEqual(overlay.sanitize_stem("a...b"), "a.b")

    def test_whitespace_and_length(self):
        self.assertEqual(overlay.sanitize_stem("  a   b  "), "a b")
        long = overlay.sanitize_stem("x" * 400)
        self.assertEqual(len(long), overlay.RENAME_MAX_STEM)

    def test_empty_after_sanitize(self):
        for bad in ("", "   ", "...", "/"):
            with self.assertRaises(ValueError, msg=repr(bad)):
                overlay.sanitize_stem(bad)


class PlanRenameTest(unittest.TestCase):
    def test_default_template_uses_size(self):
        plan = overlay.plan_rename("a001.jpg", {}, 860, 2000, mtime=1_800_000_000,
                                   index=1, asset_id="asset-42",
                                   registry=overlay.RenameRegistry())
        self.assertEqual(plan["name"], "a001_860x2000.jpg")
        self.assertEqual(plan["ext"], "jpg")
        self.assertTrue(plan["changed"])
        self.assertEqual(plan["collisionIndex"], 1)

    def test_ext_comes_from_source_and_is_appended(self):
        plan = overlay.plan_rename("A001.PNG", {"renameTemplate": "photo_{width}"}, 10, 20,
                                   mtime=0, index=1, registry=overlay.RenameRegistry())
        self.assertEqual(plan["name"], "photo_10.png")

    def test_date_comes_from_mtime_not_now(self):
        """同一张图今天跑和明天跑必须同名 —— `{date}` 取源文件 mtime。"""
        a = overlay.plan_rename("a.jpg", {"renameTemplate": "{name}-{date}"}, 10, 10,
                                mtime=1_767_225_600, index=1, registry=overlay.RenameRegistry())
        b = overlay.plan_rename("a.jpg", {"renameTemplate": "{name}-{date}"}, 10, 10,
                                mtime=1_767_225_600, index=1, registry=overlay.RenameRegistry())
        self.assertEqual(a["name"], b["name"])
        self.assertRegex(a["date"], r"^\d{8}$")

    def test_collision_numbering_is_deterministic(self):
        reg = overlay.RenameRegistry()
        names = [overlay.plan_rename("same.jpg", {"renameTemplate": "x"}, 10, 10, mtime=0,
                                     index=i + 1, registry=reg)["name"] for i in range(3)]
        self.assertEqual(names, ["x.jpg", "x-2.jpg", "x-3.jpg"])
        # 同一批换个 registry 再跑一遍：逐字相同（可复现）
        reg2 = overlay.RenameRegistry()
        again = [overlay.plan_rename("same.jpg", {"renameTemplate": "x"}, 10, 10, mtime=0,
                                     index=i + 1, registry=reg2)["name"] for i in range(3)]
        self.assertEqual(names, again)

    def test_template_with_index_avoids_collision(self):
        reg = overlay.RenameRegistry()
        names = [overlay.plan_rename("same.jpg", {"renameTemplate": "x_{index}"}, 10, 10,
                                     mtime=0, index=i + 1, registry=reg)["name"]
                 for i in range(3)]
        self.assertEqual(names, ["x_1.jpg", "x_2.jpg", "x_3.jpg"])

    def test_unchanged_when_template_renders_original(self):
        plan = overlay.plan_rename("a001.jpg", {"renameTemplate": "{name}"}, 10, 10, mtime=0,
                                   index=1, registry=overlay.RenameRegistry())
        self.assertFalse(plan["changed"])

    def test_chinese_stem_survives_template_and_sanitize(self):
        """中文原名是最常见的输入（商品主图/门店照片），不许被消毒掉或截成空。"""
        plan = overlay.plan_rename("商品主图.jpg", {"renameTemplate": "{name}-{width}x{height}"},
                                   860, 2000, mtime=0, index=1,
                                   registry=overlay.RenameRegistry())
        self.assertEqual(plan["name"], "商品主图-860x2000.jpg")
        self.assertEqual(overlay.sanitize_stem("门店 照片（正面）"), "门店 照片（正面）")
        # 超长中文也按 RENAME_MAX_STEM 截断，且结果仍是有效名字（不是空串）
        long_cn = overlay.sanitize_stem("商品主图" * 100)
        self.assertLessEqual(len(long_cn), overlay.RENAME_MAX_STEM)
        self.assertTrue(long_cn)


# ---------------------------------------------------------------- 改名动作
class RenameActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="n6c-rn-")
        self.src = os.path.join(self.tmp, "a001.jpg")
        with open(self.src, "wb") as fh:
            fh.write(b"\xff\xd8source-bytes")
        self.work = os.path.join(self.tmp, "work")
        os.makedirs(self.work)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_ok_row_produces_name_only(self):
        reg = overlay.RenameRegistry()
        gw = StubGateway()
        row = overlay.rename_action(asset(self.src), {}, gw, self.work, registry=reg)
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row)
        self.assertNotIn("artifacts", row)              # 改名不出产物
        self.assertEqual(row["fields"]["assetName"], "a001_400x300.jpg")
        self.assertEqual(row["fields"]["batchIndex"], 1)
        self.assertEqual(row["evidence"]["bytesUnchanged"], True)
        self.assertEqual(row["evidence"]["artifactsProduced"], 0)
        self.assertEqual(row["evidence"]["sourceSha256"], "a" * 64)
        # 只调了 /io/read：改名不该碰任何像素算子
        self.assertEqual([c[0] for c in gw.calls], ["/io/read"])
        # workdir 里一个文件都没有（没有静默复制/重编码）
        self.assertEqual(os.listdir(self.work), [])

    def test_batch_index_and_collisions_are_sequential(self):
        reg = overlay.RenameRegistry()
        gw = StubGateway()
        names, indices = [], []
        for _ in range(3):
            row = overlay.rename_action(asset(self.src), {"renameTemplate": "same"},
                                        gw, self.work, registry=reg)
            names.append(row["fields"]["assetName"])
            indices.append(row["fields"]["batchIndex"])
        self.assertEqual(names, ["same.jpg", "same-2.jpg", "same-3.jpg"])
        self.assertEqual(indices, [1, 2, 3])

    def test_template_error_is_32003(self):
        row = overlay.rename_action(asset(self.src), {"renameTemplate": "{brand}"},
                                    StubGateway(), self.work,
                                    registry=overlay.RenameRegistry())
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")

    def test_unreadable_source_is_32002(self):
        gw = StubGateway(errors={"io_read": GatewayError(-32002, "asset_unreadable", status=404)})
        row = overlay.rename_action(asset(self.src), {}, gw, self.work,
                                    registry=overlay.RenameRegistry())
        self.assertEqual(row["error"]["code"], "-32002")

    def test_missing_path(self):
        row = overlay.rename_action({"id": "a"}, {}, StubGateway(), self.work,
                                    registry=overlay.RenameRegistry())
        self.assertEqual(row["error"]["code"], "-32003")


# ---------------------------------------------------------------- schema / 注册表
class SchemaTest(unittest.TestCase):
    ALLOWED_KEYS = {"type", "title", "default", "min", "max", "step", "options", "unit",
                    "accept", "group", "advanced", "when", "irreversible"}
    ALLOWED_TYPES = {"integer", "number", "boolean", "enum", "string", "color",
                     "asset_ref", "range"}

    def test_properties(self):
        props = overlay.SCHEMA["properties"]
        groups = {g["key"] for g in overlay.SCHEMA["groups"]}
        self.assertTrue(props)
        for key, spec in props.items():
            self.assertLessEqual(set(spec), self.ALLOWED_KEYS, key)
            self.assertIn(spec["type"], self.ALLOWED_TYPES, key)
            self.assertIn(spec["group"], groups, key)
            self.assertTrue(spec["title"], key)
            if spec["type"] == "enum":
                self.assertTrue(spec["options"], key)
                self.assertIn(spec["default"], spec["options"], key)
            if spec["type"] not in {"integer", "number", "range"}:
                self.assertFalse({"min", "max", "step"} & set(spec), key)
            for dep, values in (spec.get("when") or {}).items():
                self.assertIn(dep, props, "%s.when 指向未声明参数 %s" % (key, dep))
                self.assertTrue(values)

    def test_schema_version_and_groups(self):
        self.assertEqual(overlay.SCHEMA["schemaVersion"], 1)
        self.assertEqual([g["key"] for g in overlay.SCHEMA["groups"]], ["overlay"])

    def test_watermark_defaults_match_code(self):
        props = overlay.SCHEMA["properties"]
        self.assertEqual(props["watermarkAnchor"]["default"], overlay.DEFAULT_ANCHOR)
        self.assertEqual(props["watermarkFontSize"]["default"], overlay.DEFAULT_FONT_SIZE)
        self.assertEqual(props["watermarkMargin"]["default"], overlay.DEFAULT_MARGIN)
        self.assertEqual(props["watermarkOpacity"]["default"], overlay.DEFAULT_OPACITY)
        self.assertEqual(props["renameTemplate"]["default"], overlay.RENAME_DEFAULT_TEMPLATE)
        self.assertEqual(props["watermarkFontSize"]["min"], overlay.FONT_SIZE_RANGE[0])
        self.assertEqual(props["watermarkFontSize"]["max"], overlay.FONT_SIZE_RANGE[1])

    def test_resources_fragment_in_sync(self):
        path = os.path.join(PACK, "resources", "params.overlay.json")
        self.assertTrue(os.path.isfile(path), "缺片段文件（跑 dump_params_overlay.py 生成）")
        with open(path, encoding="utf-8") as fh:
            on_disk = fh.read()
        self.assertEqual(on_disk, json.dumps(overlay.SCHEMA, ensure_ascii=False, indent=1) + "\n")

    def test_actions_registry(self):
        self.assertEqual(set(overlay.ACTIONS), {"watermark", "rename"})
        for fn in overlay.ACTIONS.values():
            self.assertTrue(callable(fn))
        self.assertEqual(overlay.KINDS, ("process",))


if __name__ == "__main__":
    unittest.main(verbosity=2)
