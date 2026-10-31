#!/usr/bin/env python3
"""编码组单测（N6b）：纯策略（convert/compress 计划、质量二分、降采样阶梯）+ 动作层对桩网关。

真像素/真编码的验收在 `corpus_codec_e2e.py`（真网关 + 语料 v1），这里只钉**决策**：
    - convert：目标格式/alpha 策略/质量/方向怎么定，参数非法怎么响亮失败；
    - compress：目标体积模式的质量二分（宿主编码器的字节随 q 单调，桩网关按同一形状造），
      降采样阶梯的止步条件（长边下限 / 最大级数），达不到体积时"失败而不是糊图"。

跑法：
    python3 packs/plugins/imaging/tests/test_codec.py
    cd packs/plugins/imaging/tests && python3 -m unittest -v test_codec
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

import codec                                       # noqa: E402
from gw import GatewayError                        # noqa: E402

CORPUS = os.environ.get("DAM_CORPUS", "/data/damdesk-test-corpus/v1")


def _load_labels(name):
    path = os.path.join(CORPUS, "labels", "%s.json" % name)
    if not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


# ---------------------------------------------------------------- 桩网关
class StubGateway:
    """按**宿主同一形状**造字节数：`bytes ≈ k * quality`（单调），可选按长边缩放。

    真宿主里 jpeg/avif 的编码字节随 quality 单调增长，二分法的正确性依赖这一点；
    桩网关把这个形状显式写出来，就能在不碰真图片的情况下验证搜索策略。
    """

    def __init__(self, size=(4000, 3000), in_bytes=8_000_000, per_quality=3000,
                 alpha="absent", fail=None):
        self.size = size
        self.in_bytes = in_bytes
        self.per_quality = per_quality
        self.alpha = alpha
        self.fail = fail or {}
        self.calls = []
        self.written = {}

    def _maybe_fail(self, key):
        if key in self.fail:
            raise self.fail[key]

    def io_read(self, path):
        self.calls.append(("/io/read", {"path": path}))
        self._maybe_fail("/io/read")
        return {"path": path, "width": self.size[0], "height": self.size[1],
                "size": self.in_bytes, "sha256": "0" * 64}

    def _encode_bytes(self, src, quality):
        # 降采样档（.png 中间产物）按面积比例缩字节；原图用 in_bytes 当作 q=100 的量级
        long_edge = self.written.get(src, self.size)[0]
        area_ratio = (long_edge / float(max(self.size))) ** 2
        base = self.in_bytes * area_ratio
        return int(base * (0.06 + 0.94 * (int(quality) / 100.0) ** 3))

    def encode(self, path, out, quality=None, background=None, auto_orient=None, fmt=None):
        self.calls.append(("/op/encode", {"path": path, "out": out, "quality": quality,
                                          "background": background, "auto_orient": auto_orient,
                                          "format": fmt}))
        self._maybe_fail("/op/encode")
        q = 85 if quality is None else int(quality)
        size = self.written.get(path, self.size)
        got_fmt = os.path.splitext(str(out))[1].lstrip(".").lower() or "jpg"
        data = b"stub-encode-%s-%s" % (str(q).encode(), b"x" * 32)
        with open(out, "wb") as fh:
            fh.write(data)
        self.written[out] = size
        return {"path": out, "width": size[0], "height": size[1], "format": got_fmt,
                "bytes": self._encode_bytes(path, q), "bytes_in": self.in_bytes,
                "source_alpha": self.alpha != "absent", "alpha": self.alpha,
                "quality_applied": got_fmt in ("jpg", "avif"), "quality": q,
                "orientation": 1, "op": "encode"}

    def upscale(self, path, scale, out):
        self.calls.append(("/op/upscale", {"path": path, "scale": scale, "out": out}))
        self._maybe_fail("/op/upscale")
        w = max(1, int(round(self.size[0] * float(scale))))
        h = max(1, int(round(self.size[1] * float(scale))))
        self.written[out] = (w, h)
        with open(out, "wb") as fh:
            fh.write(b"stub-resample")
        return {"path": out, "width": w, "height": h, "op": "upscale.resample"}


def _mk_asset(tmpdir, name="src.jpg"):
    path = os.path.join(tmpdir, name)
    with open(path, "wb") as fh:
        fh.write(b"source-bytes")
    return {"id": "a1", "path": path}


# ---------------------------------------------------------------- convert 计划
class PlanConvertTest(unittest.TestCase):
    def test_defaults(self):
        plan = codec.plan_convert({"path": "/x/a.png", "width": 10, "height": 20, "bytes": 5})
        self.assertEqual(plan["fmt"], "jpg")
        self.assertEqual(plan["srcFmt"], "png")
        self.assertEqual(plan["quality"], 85)
        self.assertEqual(plan["alphaMode"], "flatten", "jpg 存不了 alpha → 合成")
        self.assertTrue(plan["qualityApplies"])
        self.assertFalse(plan["sameFormat"])

    def test_alpha_kept_for_alpha_capable_targets(self):
        for fmt in ("png", "webp", "avif"):
            plan = codec.plan_convert({"path": "/x/a.jpg", "width": 1, "height": 1, "bytes": 1},
                                      {"convertFormat": fmt})
            self.assertEqual(plan["alphaMode"], "keep", fmt)
        plan = codec.plan_convert({"path": "/x/a.png", "width": 1, "height": 1, "bytes": 1},
                                  {"convertFormat": "png", "convertKeepAlpha": False})
        self.assertEqual(plan["alphaMode"], "flatten", "用户主动关掉保留 → 合成")
        self.assertTrue(plan["sameFormat"])

    def test_lossless_targets_do_not_claim_quality(self):
        for fmt in ("png", "webp"):
            plan = codec.plan_convert({"path": "/x/a.jpg", "width": 1, "height": 1, "bytes": 1},
                                      {"convertFormat": fmt, "convertQuality": 30})
            self.assertFalse(plan["qualityApplies"], fmt)

    def test_invalid_params(self):
        base = {"path": "/x/a.jpg", "width": 1, "height": 1, "bytes": 1}
        for bad in ({"convertFormat": "gif"}, {"convertQuality": 0}, {"convertQuality": 101},
                    {"convertBackground": "red"}, {"convertBackground": "#12345"}):
            with self.assertRaises(ValueError, msg=bad):
                codec.plan_convert(base, bad)

    def test_corpus_labels_expectations_are_what_we_plan(self):
        """语料 convert 10 张：labels.expect.out_format 必须与"按文件名意图调用的参数"一致。"""
        labels = _load_labels("convert")
        if labels is None:
            self.skipTest("无语料 labels")
        self.assertEqual(len(labels["items"]), 10)
        params_for = {"webp": {"convertFormat": "webp"}, "png": {"convertFormat": "png"}}
        for it in labels["items"]:
            want = it["expect"]["out_format"]
            self.assertIn(want, params_for, "语料只覆盖 webp/png 两种转换目标：%s" % it["file"])
            plan = codec.plan_convert({"path": it["file"], "width": 1, "height": 1, "bytes": 1},
                                      params_for[want])
            self.assertEqual(plan["fmt"], want, it["file"])


# ---------------------------------------------------------------- 质量二分
class _Probe:
    """可调用 + 记录调用序列（二分收敛次数要能断言）。"""

    def __init__(self, sizes):
        self.sizes = sizes
        self.calls = []

    def __call__(self, q):
        self.calls.append(q)
        return self.sizes(q)


class BisectQualityTest(unittest.TestCase):
    def _probe(self, sizes):
        return _Probe(sizes)

    def test_finds_max_quality_under_target(self):
        probe = self._probe(lambda q: int(q * 100))
        res = codec.bisect_quality(probe, 40, 95, 6000)
        self.assertEqual(res["quality"], 60, "q*100<=6000 → 最大 q=60")
        self.assertLessEqual(res["bytes"], 6000)
        self.assertLessEqual(len(probe.calls), 8, "二分必须在 ~6 次内收敛：%s" % probe.calls)

    def test_floor_overflow_reports_none(self):
        probe = self._probe(lambda q: 70 * 1000)     # 任何质量都超标
        res = codec.bisect_quality(probe, 40, 95, 6000)
        self.assertIsNone(res["quality"])
        self.assertEqual(res["floor"], 40)
        self.assertEqual(len(probe.calls), 1, "底线都不达标时只该试一次")

    def test_first_ok_saves_one_probe(self):
        probe = self._probe(lambda q: int(q * 100))
        res = codec.bisect_quality(probe, 60, 95, 8000, first_ok=6000)
        self.assertEqual(res["quality"], 80)
        self.assertNotIn(60, probe.calls, "已测过的底线不该再测一遍")

    def test_monotone_boundary_exact(self):
        for target, want in ((4000, 40), (4050, 40), (4099, 40), (4100, 41), (9500, 95)):
            probe = self._probe(lambda q: q * 100)
            res = codec.bisect_quality(probe, 40, 95, target)
            self.assertEqual(res["quality"], want, "target=%d" % target)


# ---------------------------------------------------------------- 降采样阶梯
class DownscaleLadderTest(unittest.TestCase):
    def test_first_step_is_original_and_monotone(self):
        ladder = codec.downscale_ladder((4000, 3000), 640)
        self.assertEqual(ladder[0], (1.0, 4000))
        longs = [le for _, le in ladder]
        self.assertEqual(longs, sorted(longs, reverse=True))
        self.assertTrue(all(le >= 640 for le in longs), longs)
        self.assertLessEqual(len(ladder), 1 + codec.MAX_DOWNSCALE_STEPS)

    def test_stops_at_min_long_edge(self):
        # min_long_edge=3000：0.85 档 3400 仍在限内，再下一档 2890 < 3000 → 停
        ladder = codec.downscale_ladder((4000, 3000), 3000)
        self.assertEqual([le for _, le in ladder], [4000, 3400])

    def test_portrait_uses_long_edge(self):
        ladder = codec.downscale_ladder((3000, 4000), 640)
        self.assertEqual(ladder[0], (1.0, 4000))

    def test_invalid_size(self):
        with self.assertRaises(ValueError):
            codec.downscale_ladder((0, 0), 640)


class NextDownscaleTest(unittest.TestCase):
    def test_ratio_based_step_lands_near_target(self):
        # 4000 长边 / 1MB / 目标 200KB → sqrt(0.2)=0.447，再留 5% → ≈1700
        nxt = codec.next_downscale_long_edge(4000, 1_000_000, 200_000, 640)
        self.assertTrue(1650 <= nxt <= 1750, nxt)
        self.assertLess(nxt, 4000)

    def test_never_grows_and_always_shrinks(self):
        for bytes_now, target in ((1_000_000, 900_000), (1_000_000, 1_000_000),
                                  (1_000_000, 5_000_000)):
            nxt = codec.next_downscale_long_edge(4000, bytes_now, target, 640)
            if nxt is not None:
                self.assertLessEqual(nxt, int(4000 * 0.95), "必须至少缩 5%")
                self.assertGreaterEqual(nxt, 640, "不破长边下限")

    def test_returns_none_below_floor(self):
        self.assertIsNone(codec.next_downscale_long_edge(700, 1_000_000, 1000, 640),
                          "缩到 640 以下都不够 → 放弃（响亮失败）")

    def test_converges_in_a_few_steps(self):
        cur, sizes = 6016, []
        for _ in range(8):
            nxt = codec.next_downscale_long_edge(cur, 4_000_000, 200_000, 640)
            if nxt is None:
                break
            sizes.append(nxt)
            cur = nxt
        self.assertLessEqual(len(sizes), 3, "按体积反算不该跨 3 档以上：%s" % sizes)


# ---------------------------------------------------------------- compress 计划
class PlanCompressTest(unittest.TestCase):
    def test_target_mode_defaults(self):
        plan = codec.plan_compress({"path": "/x/a.jpg", "width": 10, "height": 10,
                                    "bytes": 8_000_000})
        self.assertEqual(plan["mode"], "target_bytes")
        self.assertEqual(plan["targetBytes"], codec.DEFAULT_TARGET_BYTES)
        self.assertEqual(plan["minQuality"], 60)
        self.assertEqual(plan["format"], "jpg", "preserve → 跟随源格式")
        self.assertIsNone(plan["skip"])

    def test_already_within_target_is_skipped(self):
        plan = codec.plan_compress({"path": "/x/a.jpg", "width": 1, "height": 1, "bytes": 1000})
        self.assertEqual(plan["skip"], "already_within_target")

    def test_quality_mode(self):
        plan = codec.plan_compress({"path": "/x/a.png", "width": 1, "height": 1, "bytes": 10},
                                   {"compressMode": "quality", "compressQuality": 70})
        self.assertEqual(plan["quality"], 70)
        self.assertEqual(plan["format"], "png")

    def test_format_override_and_invalid(self):
        plan = codec.plan_compress({"path": "/x/a.png", "width": 1, "height": 1, "bytes": 10},
                                   {"compressFormat": "jpg"})
        self.assertEqual(plan["format"], "jpg")
        for bad in ({"compressMode": "magic"}, {"compressFormat": "gif"},
                    {"compressTargetBytes": 10}, {"compressMinQuality": 99}):
            with self.assertRaises(ValueError, msg=bad):
                codec.plan_compress({"path": "/x/a.jpg", "width": 1, "height": 1, "bytes": 10}, bad)


# ---------------------------------------------------------------- 动作层（桩网关）
class ConvertActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.work = os.path.join(self.tmp, "work")
        os.makedirs(self.work)
        self.asset = _mk_asset(self.tmp)

    def test_ok_row_shape(self):
        gw = StubGateway()
        row = codec.convert_action(self.asset, {"convertFormat": "webp"}, gw, self.work)
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row, "处理类行不许有 verdict（N5a）")
        self.assertEqual(row["fields"]["format"], "webp")
        self.assertTrue(row["fields"]["outPath"].endswith(".convert.webp"))
        self.assertTrue(os.path.isfile(row["fields"]["outPath"]))
        self.assertEqual(row["evidence"]["formatMatchesPlan"], True)
        # 请求里必须带质量与方向策略（宿主据此执行）
        enc = [c for c in gw.calls if c[0] == "/op/encode"][-1][1]
        self.assertEqual(enc["quality"], 85)
        self.assertTrue(enc["auto_orient"])

    def test_alpha_flattened_when_requested_keep(self):
        gw = StubGateway(alpha="flattened")
        row = codec.convert_action(self.asset, {"convertFormat": "png"}, gw, self.work)
        self.assertEqual(row["result"], "failed", "要求保留 alpha 但宿主合成 → 必须失败")
        self.assertIn("-32000", row["error"]["code"])

    def test_gateway_error_maps_to_code(self):
        gw = StubGateway(fail={"/op/encode": GatewayError(-32002, "读不到", status=404)})
        row = codec.convert_action(self.asset, {"convertFormat": "jpg"}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32002")

    def test_invalid_param_is_loud(self):
        gw = StubGateway()
        row = codec.convert_action(self.asset, {"convertFormat": "bmp"}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")
        self.assertIn("convertFormat", row["error"]["message"])

    def test_missing_asset_path(self):
        row = codec.convert_action({}, {}, StubGateway(), self.work)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32003")


class CompressActionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.work = os.path.join(self.tmp, "work")
        os.makedirs(self.work)
        self.asset = _mk_asset(self.tmp)

    def test_target_bytes_reachable_without_downscale(self):
        # 桩模型 bytes(q) = in_bytes*(0.06+0.94*(q/100)^3)；300KB 源在 q=60 就能进 200KB
        gw = StubGateway(size=(2000, 1500), in_bytes=300_000)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 200_000}, gw, self.work)
        self.assertEqual(row["result"], "ok", row)
        self.assertLessEqual(row["fields"]["outBytes"], 200_000)
        self.assertGreaterEqual(row["fields"]["quality"], 60, "质量底线不可破")
        self.assertFalse(row["fields"]["downscaled"])
        self.assertNotIn("verdict", row)
        self.assertTrue(row["evidence"]["withinTarget"])
        self.assertTrue(row["evidence"]["qualityFloorRespected"])

    def test_target_bytes_needs_downscale(self):
        # 原尺寸任何质量都超标，缩到一定长边后才能达标 → 必须走 /op/upscale
        gw = StubGateway(size=(4000, 3000), in_bytes=8_000_000, per_quality=1)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 200_000,
                                                 "compressMinQuality": 60}, gw, self.work)
        self.assertEqual(row["result"], "ok", row)
        self.assertTrue(row["fields"]["downscaled"], "必须降采样才能达标")
        self.assertTrue(any(c[0] == "/op/upscale" for c in gw.calls))
        self.assertLessEqual(row["fields"]["outBytes"], 200_000)
        self.assertGreaterEqual(row["fields"]["outLongEdge"], 640, "降采样长边下限")
        # 中间产物用完即删：work 目录里只留最终产物
        leftovers = [f for f in os.listdir(self.work) if f.startswith(".")]
        self.assertEqual(leftovers, [], "降采样中间产物必须删掉：%s" % leftovers)

    def test_prefers_quality_over_size(self):
        """质量优先：native 只有 q<质量目标 才达标 → 缩图，而不是把质量压到底线。"""
        gw = StubGateway(size=(2000, 1500), in_bytes=4_000_000)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 400_000,
                                                 "compressMinQuality": 60,
                                                 "compressPreferredQuality": 80}, gw, self.work)
        self.assertEqual(row["result"], "ok", row)
        self.assertTrue(row["fields"]["downscaled"], "必须靠降采样达标")
        self.assertGreaterEqual(row["fields"]["quality"], 80,
                               "质量目标不可用来换体积：q=%s" % row["fields"]["quality"])
        self.assertLessEqual(row["fields"]["outBytes"], 400_000)
        self.assertEqual(row["evidence"]["preferredQuality"], 80)

    def test_floor_clamp_keeps_best_quality_above_floor(self):
        """缩到长边下限仍到不了质量目标 → 用「底线之上的最大质量」收，绝不砸到下限。"""
        gw = StubGateway(size=(2000, 1500), in_bytes=4_000_000)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 200_000,
                                                 "compressMinQuality": 60,
                                                 "compressPreferredQuality": 80}, gw, self.work)
        self.assertEqual(row["result"], "ok", row)
        self.assertTrue(row["fields"]["downscaled"])
        self.assertEqual(row["fields"]["outLongEdge"], 640, "夹到长边下限")
        self.assertGreaterEqual(row["fields"]["quality"], 60, "不会低于质量底线")
        self.assertGreater(row["fields"]["quality"], 60, "也不会直接砸到下限了事")
        self.assertLessEqual(row["fields"]["outBytes"], 200_000)

    def test_unreachable_target_fails_instead_of_blurring(self):
        gw = StubGateway(size=(4000, 3000), in_bytes=50_000_000, per_quality=1)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 10 * 1024,
                                                 "compressMinQuality": 60}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertIn("不可达", row["error"]["message"])
        self.assertGreaterEqual(len(row["fields"]["attempts"]), 1)
        # 失败时不许留下产物
        self.assertEqual([f for f in os.listdir(self.work) if not f.startswith(".")], [])

    def test_downscale_disabled_reports_floor_conflict(self):
        gw = StubGateway(size=(4000, 3000), in_bytes=8_000_000, per_quality=1)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 200_000,
                                                 "compressAllowDownscale": False}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertFalse(any(c[0] == "/op/upscale" for c in gw.calls), "关掉降采样就不许缩图")

    def test_lossless_format_hint(self):
        gw = StubGateway(size=(2000, 1500), in_bytes=4_000_000, per_quality=1)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 10 * 1024,
                                                 "compressFormat": "png"}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertIn("无损容器", row["error"]["message"])

    def test_quality_mode_single_encode(self):
        gw = StubGateway(size=(2000, 1500), in_bytes=4_000_000)
        row = codec.compress_action(self.asset, {"compressMode": "quality",
                                                 "compressQuality": 70}, gw, self.work)
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["quality"], 70)
        self.assertEqual(len([c for c in gw.calls if c[0] == "/op/encode"]), 1,
                         "质量档模式只该编一次")

    def test_already_within_target_skips(self):
        gw = StubGateway(size=(800, 600), in_bytes=100_000)
        row = codec.compress_action(self.asset, {"compressTargetBytes": 200_000}, gw, self.work)
        self.assertEqual(row["result"], "skipped")
        self.assertEqual(row["skipReason"], "already_within_target")
        self.assertFalse(any(c[0] == "/op/encode" for c in gw.calls), "不超标就别动它")

    def test_gateway_unreadable(self):
        gw = StubGateway(fail={"/io/read": GatewayError(-32002, "不存在", status=404)})
        row = codec.compress_action(self.asset, {}, gw, self.work)
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32002")


# ---------------------------------------------------------------- schema 防漂移
class SchemaTest(unittest.TestCase):
    def test_resources_fragment_in_sync(self):
        path = os.path.join(PACK, "resources", "params.codec.json")
        self.assertTrue(os.path.isfile(path),
                        "缺 %s：先跑 tests/dump_params_schema_codec.py" % path)
        with open(path, encoding="utf-8") as fh:
            on_disk = json.load(fh)
        self.assertEqual(on_disk, codec.SCHEMA,
                         "resources/params.codec.json 与 codec.SCHEMA 漂移了：重跑 dump 脚本")

    def test_schema_shape(self):
        props = codec.SCHEMA["properties"]
        self.assertEqual(codec.SCHEMA["schemaVersion"], 1)
        self.assertEqual(codec.SCHEMA["groups"][0]["key"], "codec")
        for name, prop in props.items():
            self.assertIn("type", prop, name)
            self.assertIn("title", prop, name)
            self.assertIn("group", prop, name)
            self.assertEqual(prop["group"], "codec", name)
            if prop["type"] == "enum":
                self.assertTrue(prop.get("options"), name)
        # 默认值必须落在自己的 options/min/max 里（宿主安装期会按 schema 校验）
        for name, prop in props.items():
            if "default" not in prop:
                continue
            if prop["type"] == "enum":
                self.assertIn(prop["default"], prop["options"], name)
            elif prop["type"] == "integer":
                self.assertGreaterEqual(prop["default"], prop.get("min", -10**9), name)
                self.assertLessEqual(prop["default"], prop.get("max", 10**9), name)

    def test_actions_registered(self):
        self.assertEqual(sorted(codec.ACTIONS), ["compress", "convert"])
        self.assertEqual(codec.KINDS, ("process",))


if __name__ == "__main__":
    unittest.main(verbosity=2)
