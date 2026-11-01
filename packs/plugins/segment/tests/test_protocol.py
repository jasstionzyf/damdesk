"""协议面：judge/process 结果行、产物落盘、参数校验、取消、报告归账。"""
from __future__ import annotations

import hashlib
import json
import os
import unittest

import _support
import pngio

plugin = _support.load_plugin()


def _judge(tmp, gw, path, options):
    opts = dict(options)
    opts.setdefault("workdir", os.path.join(tmp, "out"))
    return plugin.dispatch({"method": "run",
                            "params": {"job_id": "job-1",
                                       "asset": {"id": "asset-1", "path": path},
                                       "options": opts}})


class ProcessRowTest(unittest.TestCase):
    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)
        plugin._REPORTS.clear()

    def test_white_mode_emits_one_artifact_and_no_verdict(self):
        reply = _judge(self.tmp, self.gw, self.src, {"bgMode": "white", "feather": 2})
        rows = reply["rows"]
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual(row["assetId"], "asset-1")
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row, "处理类包不得伪造 verdict（N5a 红线）")
        art = row["artifacts"][0]
        self.assertEqual(art["kind"], "export_file")
        self.assertTrue(os.path.isfile(art["path"]))
        with open(art["path"], "rb") as fh:
            self.assertEqual(hashlib.sha256(fh.read()).hexdigest(), art["sha256"])
        self.assertEqual(row["evidence"]["bgMode"], "white")
        self.assertEqual(row["evidence"]["format"], "jpg")
        self.assertEqual(row["evidence"]["cropBox"][0] >= 0, True)
        self.assertGreater(row["evidence"]["featherPixels"], 0, "羽化必须真的产生部分透明像素")

    def test_crops_only_the_subject_block_and_leaves_no_intermediates(self):
        _judge(self.tmp, self.gw, self.src, {"bgMode": "white"})
        workdir = os.path.join(self.tmp, "out")
        files = sorted(os.listdir(workdir))
        self.assertEqual(len(files), 1, "一个资产只留一个产物，中间件必须删掉：%s" % files)
        self.assertTrue(files[0].endswith(".jpg"))
        # 第一次裁块 = 主体框 + 边距（远小于整图 320x240）
        src_crop = [c for c in self.gw.crop_sizes if c[0] == self.src]
        self.assertEqual(len(src_crop), 1)
        x1, y1, x2, y2 = src_crop[0][1]
        self.assertLess((x2 - x1) * (y2 - y1), 320 * 240, "必须只裁主体块，不是全图")

    def test_transparent_mode_emits_rgba_png(self):
        reply = _judge(self.tmp, self.gw, self.src, {"bgMode": "transparent", "feather": 4})
        art = reply["rows"][0]["artifacts"][0]
        self.assertTrue(art["path"].endswith(".png"))
        width, height, channels, raw = pngio.decode_path(art["path"])
        self.assertEqual(channels, 4, "透明输出必须是 RGBA")
        self.assertEqual((width, height), (320, 240), "产物是整图尺寸，不是裁块")
        alphas = raw[3::4]
        self.assertEqual(min(alphas), 0, "背景必须全透明")
        self.assertEqual(max(alphas), 255, "主体必须不透明")
        self.assertTrue(any(0 < v < 255 for v in alphas), "边缘必须有 alpha 斜坡")

    def test_color_mode_paints_requested_color(self):
        reply = _judge(self.tmp, self.gw, self.src,
                       {"bgMode": "color", "color": "#3366cc"})
        row = reply["rows"][0]
        self.assertEqual(row["evidence"]["bgColor"], "#3366cc")
        width, height, channels, _raw = pngio.decode_path(row["artifacts"][0]["path"])
        self.assertEqual((width, height), (row["evidence"]["outWidth"], row["evidence"]["outHeight"]))

    def test_same_params_same_bytes(self):
        first = _judge(self.tmp, self.gw, self.src, {"bgMode": "white", "feather": 2})
        second = _judge(self.tmp, self.gw, self.src, {"bgMode": "white", "feather": 2})
        self.assertEqual(first["rows"][0]["artifacts"][0]["sha256"],
                         second["rows"][0]["artifacts"][0]["sha256"],
                         "同参数重跑必须同字节（N14 sha256 对账）")

    def test_no_subject_is_skipped_not_faked(self):
        stub = _support.StubGateway(self.tmp, self.box, (320, 240), fg_ratio=0.0)
        _support.install_stub(plugin, stub)
        row = _judge(self.tmp, stub, self.src, {"bgMode": "white"})["rows"][0]
        self.assertEqual(row["result"], "skipped")
        self.assertEqual(row["skipReason"], "no_subject")
        self.assertNotIn("artifacts", row)

    def test_report_ledger_is_per_job(self):
        _judge(self.tmp, self.gw, self.src, {"bgMode": "white"})
        report = plugin.dispatch({"method": "report", "params": {"job_id": "job-1"}})
        self.assertEqual(report["rows"], 1)
        self.assertEqual(plugin.dispatch({"method": "report", "params": {"job_id": "nope"}})["rows"], 0)


class ParamTest(unittest.TestCase):
    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)

    def _run(self, options):
        return _judge(self.tmp, self.gw, self.src, options)

    def test_rejects_unknown_param(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            self._run({"bgMode": "white", "radius": 3})
        self.assertEqual(ctx.exception.code, -32003)

    def test_rejects_bad_bg_mode(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            self._run({"bgMode": "purple"})
        self.assertEqual(ctx.exception.code, -32003)

    def test_rejects_out_of_range_feather_and_threshold(self):
        for options in ({"feather": 9}, {"feather": -1}, {"edgeThreshold": 256}):
            with self.assertRaises(plugin.RpcError) as ctx:
                self._run(options)
            self.assertEqual(ctx.exception.code, -32003, options)

    def test_rejects_bad_color_hex(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            self._run({"bgMode": "color", "color": "red"})
        self.assertEqual(ctx.exception.code, -32003)

    def test_missing_asset_path(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "run",
                             "params": {"job_id": "j", "asset": {}, "options": {}}})
        self.assertEqual(ctx.exception.code, -32003)

    def test_asset_unreadable_maps_to_32002(self):
        self.gw.fail_with["/io/read"] = -32002
        with self.assertRaises(plugin.RpcError) as ctx:
            self._run({"bgMode": "white"})
        self.assertEqual(ctx.exception.code, -32002)

    def test_unknown_method(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "no.such.method", "params": {}})
        self.assertEqual(ctx.exception.code, -32601)

    def test_cancel_stops_the_run(self):
        plugin._cancel_flag("job-cancel").set()
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "run",
                             "params": {"job_id": "job-cancel",
                                        "asset": {"id": "a", "path": self.src},
                                        "options": {"bgMode": "white",
                                                    "workdir": os.path.join(self.tmp, "o")}}})
        self.assertEqual(ctx.exception.code, -32004)
        plugin._cancel_flag("job-cancel").clear()


class PackInfoTest(unittest.TestCase):
    def test_info_matches_manifest(self):
        with open(os.path.join(_support.PACK_DIR, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        info = plugin.dispatch({"method": "pack.info", "params": {}})
        for key in ("id", "name", "version", "api_version", "capabilities",
                    "deterministic", "timeout_secs"):
            self.assertEqual(info[key], manifest[key], key)
        self.assertEqual(manifest["outputs"], ["artifact"])
        self.assertEqual(info["rules"], [], "处理类包没有规则面")
        health = plugin.dispatch({"method": "health", "params": {}})
        self.assertEqual(health["status"], "ok")
        # 宿主握手拿 health.api_version 与 manifest 严格比对（缺了就 plugin_load_failed）
        self.assertEqual(health["api_version"], manifest["api_version"])


if __name__ == "__main__":
    unittest.main()
