"""N19：大图预缩放（`maxSide`）+ 逐段耗时（`stages`）—— 两条都是「真机超时」逼出来的。

为什么这两件事必须一起钉：

1. **预缩放**是本包唯一的「按钱省时间」手段（像素活全是 O(像素) 的纯 Python）；
   开了它，抓块/合成/编码都按缩放后的尺寸做，产物尺寸随之变小 —— 这个语义必须显式，
   而且要真的走宿主 `/op/resize`（包内不解码原图，协议 §6）。
2. **逐段耗时**是超时排障的唯一入口：宿主只会说「超过 N ms」，卡在哪一段（宿主解码 /
   u2net / 纯 Python 合成 / 落盘）必须由包自己记 —— 包括**进程被 kill 时**（故 JSONL
   边跑边落，而不是只在结果行里汇总）。
"""
from __future__ import annotations

import json
import os
import unittest

import _support
import pngio

plugin = _support.load_plugin()

STAGE_KEYS = set(plugin.STAGE_KEYS)


def _run(tmp, gw, path, options):
    opts = dict(options)
    opts.setdefault("workdir", os.path.join(tmp, "out"))
    return plugin.dispatch({"method": "judge",
                            "params": {"job_id": "job-1",
                                       "asset": {"id": "asset-1", "path": path},
                                       "options": opts}})


class PrescaleTest(unittest.TestCase):
    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)
        plugin._REPORTS.clear()

    def test_default_does_not_scale_and_reports_source_size(self):
        row = _run(self.tmp, self.gw, self.src, {"bgMode": "white"})["rows"][0]
        self.assertNotIn("/op/resize", self.gw.paths(), "默认不缩放：一个多余端点都不许调")
        ev = row["evidence"]
        self.assertFalse(ev["preScaled"])
        self.assertEqual((ev["sourceWidth"], ev["sourceHeight"]), (320, 240))
        self.assertEqual((ev["width"], ev["height"]), (320, 240))
        self.assertEqual(ev["maxSide"], 0)

    def test_max_side_scales_through_host_and_keeps_ratio(self):
        # 320x240 + maxSide=160 → 160x120；u2net / 裁块都按缩放后的图做
        row = _run(self.tmp, self.gw, self.src, {"bgMode": "white", "maxSide": 160})["rows"][0]
        self.assertIn("/op/resize", self.gw.paths(), "大图保护必须走宿主 /op/resize")
        ev = row["evidence"]
        self.assertTrue(ev["preScaled"])
        self.assertEqual((ev["sourceWidth"], ev["sourceHeight"]), (320, 240))
        self.assertEqual((ev["width"], ev["height"]), (160, 120))
        self.assertEqual((ev["outWidth"], ev["outHeight"]), (160, 120), "产物尺寸 = 工作尺寸")
        self.assertLessEqual(max(ev["outWidth"], ev["outHeight"]), 160)
        # 产物真的按缩放后的尺寸落盘（应答自证不算，像素要核；桩的 /op/crop 一律写 PNG 字节）
        art = row["artifacts"][0]
        w, h, _ch, _raw = pngio.decode_path(art["path"])
        self.assertEqual((w, h), (160, 120))
        # 中间件（*-scaled.png）必须删干净：一个资产只留一个产物
        files = sorted(os.listdir(os.path.join(self.tmp, "out")))
        self.assertEqual(len(files), 1, "缩放的中间件不许留在工作目录：%s" % files)
        # 源图裁块用的是缩放后的副本，不是原图（另两次裁块是掩码 + 编码，走别的源文件）
        cropped_sources = {c[0] for c in self.gw.crop_sizes}
        self.assertNotIn(self.src, cropped_sources, "预缩放后不许再拿原图裁块")
        self.assertTrue(
            any(p.endswith("-scaled.png") for p in cropped_sources),
            "必须从缩放副本裁块：%s" % (cropped_sources,))

    def test_max_side_larger_than_long_edge_is_a_noop(self):
        row = _run(self.tmp, self.gw, self.src, {"bgMode": "white", "maxSide": 4000})["rows"][0]
        self.assertNotIn("/op/resize", self.gw.paths(), "长边已在上限内 → 不做无意义缩放")
        self.assertFalse(row["evidence"]["preScaled"])

    def test_max_side_out_of_range_is_rejected_loudly(self):
        for bad in (-1, 8193, 100000):
            with self.assertRaises(plugin.RpcError) as ctx:
                plugin.normalize_options({"bgMode": "white", "maxSide": bad})
            self.assertEqual(ctx.exception.code, -32003, bad)
        with self.assertRaises(plugin.RpcError):
            plugin.normalize_options({"bgMode": "white", "maxSide": "big"})

    def test_schema_declares_max_side_as_advanced(self):
        path = os.path.join(_support.PACK_DIR, "resources", "params.schema.json")
        with open(path, encoding="utf-8") as fh:
            schema = json.load(fh)
        prop = schema["properties"]["maxSide"]
        self.assertEqual(prop["type"], "integer")
        self.assertEqual(prop["default"], 0, "默认必须是「不缩放」，不能替用户改产物尺寸")
        self.assertTrue(prop["advanced"], "大图保护是性能开关，属于高级参数")
        self.assertEqual(prop["min"], 0)
        self.assertIn(prop["group"], {g["key"] for g in schema["groups"]})


class StageTimingTest(unittest.TestCase):
    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)
        plugin._REPORTS.clear()
        self._orig = os.environ.pop(plugin.STAGE_LOG_ENV, None)

    def tearDown(self):
        if self._orig is not None:
            os.environ[plugin.STAGE_LOG_ENV] = self._orig
        else:
            os.environ.pop(plugin.STAGE_LOG_ENV, None)

    def test_evidence_carries_every_stage(self):
        ev = _run(self.tmp, self.gw, self.src, {"bgMode": "white"})["rows"][0]["evidence"]
        stages = ev["stages"]
        self.assertEqual(set(stages), STAGE_KEYS, "逐段耗时要全（缺一段 = 排障靠猜）")
        for key, ms in stages.items():
            self.assertIsInstance(ms, int, key)
            self.assertGreaterEqual(ms, 0, key)
        self.assertLessEqual(sum(stages.values()), ev["elapsedMs"] + 50,
                             "各段之和不得远大于总耗时（口径必须自洽）")

    def test_stage_log_streams_each_stage_so_timeout_keeps_evidence(self):
        """超时 = 进程被 kill、应答不回来 —— 只有边跑边落的行能看出卡在哪一段。"""
        log_path = os.path.join(self.tmp, "stages.jsonl")
        os.environ[plugin.STAGE_LOG_ENV] = log_path
        _run(self.tmp, self.gw, self.src, {"bgMode": "white"})
        lines = [json.loads(l) for l in open(log_path, encoding="utf-8").read().splitlines() if l.strip()]
        stage_events = [l for l in lines if l.get("event") == "stage"]
        result_events = [l for l in lines if l.get("event") == "result"]
        self.assertEqual({e["stage"] for e in stage_events}, STAGE_KEYS)
        self.assertEqual(len(result_events), 1)
        self.assertEqual(result_events[0]["assetId"], "asset-1")
        # 每条 stage 行都必须自带 totalMs（崩在中间时用它算「跑了多久」）
        self.assertTrue(all(isinstance(e["totalMs"], int) for e in stage_events))


if __name__ == "__main__":
    unittest.main()
