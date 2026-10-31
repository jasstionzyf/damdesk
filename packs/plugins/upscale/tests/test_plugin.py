"""结果行 / 参数 / 网关错误 / 分块 / 闸门：超分包的行为面单测（桩网关，真像素）。"""
from __future__ import annotations

import errno
import os
import threading
import time
import unittest

import _support
import pngio

plugin = _support.load_plugin()


def judge(tmp, path, options, job_id="job-1", asset_id="asset-1"):
    opts = dict(options)
    opts.setdefault("workdir", os.path.join(tmp, "out"))
    return plugin.dispatch({"method": "judge",
                            "params": {"job_id": job_id,
                                       "asset": {"id": asset_id, "path": path},
                                       "options": opts}})


class Base(unittest.TestCase):
    size = (200, 120)

    def setUp(self):
        self.tmp = _support.tempdir()
        self.src = _support.gradient_png(os.path.join(self.tmp, "src.png"), *self.size)
        self.workdir = os.path.join(self.tmp, "out")
        self.stub = _support.StubGateway()
        _support.install_stub(plugin, self.stub)
        plugin._REPORTS.clear()
        plugin._CANCEL.clear()
        os.environ["DAMPACK_UPSCALE_LOCK_DIR"] = os.path.join(self.tmp, "locks")

    def tearDown(self):
        _support.uninstall(plugin)
        os.environ.pop("DAMPACK_UPSCALE_LOCK_DIR", None)


class RowContractTest(Base):
    def test_light_scale2_ok_row_has_no_verdict(self):
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "light"})["rows"][0]
        self.assertEqual(row["result"], "ok")
        self.assertNotIn("verdict", row, "处理类包不得伪造 verdict（N5a 红线）")
        self.assertEqual(row["artifacts"][0]["kind"], "export_file")
        self.assertTrue(os.path.isfile(row["artifacts"][0]["path"]))
        self.assertEqual((row["evidence"]["outWidth"], row["evidence"]["outHeight"]), (400, 240))
        self.assertTrue(row["evidence"]["sizeExact"])
        self.assertEqual(row["evidence"]["steps"][0]["op"], "upscale.resample")

    def test_full_scale2_runs_model_then_half_resample(self):
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full"})["rows"][0]
        self.assertEqual(row["result"], "ok")
        modes = [s["op"] for s in row["evidence"]["steps"]]
        self.assertEqual(modes, ["upscale.realesr-x4v3", "upscale.resample"])
        scales = [c["scale"] for c in self.stub.upscale_calls]
        self.assertEqual(scales, [4.0, 0.5], "2× 必须是「4× 模型 + 0.5 重采样」")
        self.assertEqual((row["evidence"]["outWidth"], row["evidence"]["outHeight"]), (400, 240))

    def test_full_scale4_single_model_pass(self):
        row = judge(self.tmp, self.src, {"scale": 4, "engine": "full"})["rows"][0]
        self.assertEqual(row["result"], "ok")
        self.assertEqual([s["op"] for s in row["evidence"]["steps"]], ["upscale.realesr-x4v3"])
        self.assertEqual((row["evidence"]["outWidth"], row["evidence"]["outHeight"]), (800, 480))

    def test_intermediate_files_removed(self):
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full"})["rows"][0]
        files = sorted(os.listdir(self.workdir))
        self.assertEqual(files, [os.path.basename(row["artifacts"][0]["path"])],
                         "工作版本目录里只许有一个产物（中间件用完即删）")

    def test_deterministic_rerun_same_sha(self):
        first = judge(self.tmp, self.src, {"scale": 2, "engine": "light"})["rows"][0]
        second = judge(self.tmp, self.src, {"scale": 2, "engine": "light"})["rows"][0]
        self.assertEqual(first["evidence"]["sha256"], second["evidence"]["sha256"])

    def test_denoise_forces_model_and_is_recorded(self):
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full", "denoise": 0.4})["rows"][0]
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["evidence"]["denoise"], 0.4)
        self.assertEqual(row["evidence"]["denoiseApplied"], "x4v3-builtin")

    def test_report_returns_rows(self):
        judge(self.tmp, self.src, {"scale": 2, "engine": "light"}, job_id="job-9")
        reply = plugin.dispatch({"method": "report", "params": {"job_id": "job-9"}})
        self.assertEqual(reply["count"], 1)
        self.assertEqual(reply["assets"][0]["result"], "ok")

    def test_report_emit_failure_does_not_break_product(self):
        self.stub.fail_with["/report/emit"] = -32000
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "light"})["rows"][0]
        self.assertEqual(row["result"], "ok")
        self.assertTrue(os.path.isfile(row["artifacts"][0]["path"]))


class FailureTest(Base):
    def test_model_missing_reports_capability_not_silent_resample(self):
        self.stub.model = False          # 网关无模型 → 一步重采样
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full"})["rows"][0]
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], -32001)
        self.assertIn("Real-ESRGAN", row["error"]["message"])
        self.assertIn("upscale.resample", row["error"]["message"], "错误里要点名实际 op")

    def test_gateway_403_maps_to_capability(self):
        self.stub.fail_with["/op/upscale"] = -32001
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full"})["rows"][0]
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], -32001)
        self.assertIn("网关", row["error"]["message"])

    def test_output_limit_reports_max_scale(self):
        self.stub.image_size = (6000, 6000)     # 4× → 24000 长边超上限
        row = judge(self.tmp, self.src, {"scale": 4, "engine": "light"})["rows"][0]
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], -32003)
        self.assertIn("maxScale", row["evidence"]["limit"])
        self.assertLess(row["evidence"]["limit"]["maxScale"], 4)

    def test_cancel_before_start(self):
        plugin._cancel_flag("job-c").set()
        with self.assertRaises(plugin.RpcError) as ctx:
            judge(self.tmp, self.src, {"scale": 2, "engine": "light"}, job_id="job-c")
        self.assertEqual(ctx.exception.code, -32004)

    def test_memory_exhausted_is_a_structured_row_not_a_crash(self):
        """OOM 必须落 failed 行（-32000 + 可读建议），不是把整批炸成一条内部错误。"""
        original = plugin.tiling.new_canvas

        def boom(*_a, **_kw):
            raise MemoryError("模拟画布分配失败")

        plugin.tiling.new_canvas = boom
        try:
            row = judge(self.tmp, self.src, {"scale": 4, "engine": "full", "tile": "384"})["rows"][0]
        finally:
            plugin.tiling.new_canvas = original
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], -32000, "闭集外的自造码不算结构化错误")
        self.assertIn("内存不足", row["error"]["message"])
        self.assertIn("tile", row["error"]["message"], "错误里要给能照做的建议")
        self.assertEqual(row["evidence"]["resource"]["kind"], "memory")
        self.assertNotIn("verdict", row)

    def test_disk_full_is_a_structured_row(self):
        original = plugin.pngio.write_png

        def boom(*_a, **_kw):
            raise OSError(errno.ENOSPC, "No space left on device")

        plugin.pngio.write_png = boom
        try:
            row = judge(self.tmp, self.src, {"scale": 4, "engine": "full", "tile": "384"})["rows"][0]
        finally:
            plugin.pngio.write_png = original
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], -32000)
        self.assertIn("磁盘写满", row["error"]["message"])
        self.assertEqual(row["evidence"]["resource"]["kind"], "disk")


class ParamsTest(Base):
    def test_scale_must_be_2_or_4(self):
        for bad in (3, 0, 8, "x"):
            with self.assertRaises(plugin.RpcError) as ctx:
                plugin.normalize_options({"scale": bad, "workdir": self.workdir})
            self.assertEqual(ctx.exception.code, -32003)
            self.assertIn("2 或 4", ctx.exception.message)

    def test_unknown_engine(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.normalize_options({"engine": "gpu", "workdir": self.workdir})
        self.assertEqual(ctx.exception.code, -32003)

    def test_tile_requires_full_engine(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.normalize_options({"engine": "light", "tile": "384", "workdir": self.workdir})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("tile", ctx.exception.message)

    def test_denoise_requires_full_engine(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.normalize_options({"engine": "light", "denoise": 0.5, "workdir": self.workdir})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("denoise", ctx.exception.message)

    def test_denoise_range(self):
        with self.assertRaises(plugin.RpcError):
            plugin.normalize_options({"denoise": 1.5, "workdir": self.workdir})

    def test_unknown_tile_value(self):
        with self.assertRaises(plugin.RpcError):
            plugin.normalize_options({"tile": "9999", "workdir": self.workdir})

    def test_defaults(self):
        opts = plugin.normalize_options({"workdir": self.workdir})
        self.assertEqual((opts["scale"], opts["engine"], opts["tile"], opts["denoise"]),
                         (2, "full", "auto", 0.0))


class TilingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = _support.tempdir()
        self.size = (900, 600)
        self.src = _support.gradient_png(os.path.join(self.tmp, "big.png"), *self.size)
        self.stub = _support.StubGateway()
        _support.install_stub(plugin, self.stub)
        plugin._REPORTS.clear()
        os.environ["DAMPACK_UPSCALE_LOCK_DIR"] = os.path.join(self.tmp, "locks")

    def tearDown(self):
        _support.uninstall(plugin)
        os.environ.pop("DAMPACK_UPSCALE_LOCK_DIR", None)

    def test_explicit_tile_splits_and_size_is_exact(self):
        row = judge(self.tmp, self.src, {"scale": 2, "engine": "full", "tile": "384"})["rows"][0]
        self.assertEqual(row["result"], "ok", row.get("error"))
        self.assertEqual(row["evidence"]["tiles"], 6, "900x600 + 384 块 = 3×2")
        self.assertEqual((row["evidence"]["outWidth"], row["evidence"]["outHeight"]), (1800, 1200))
        crops = [c for c in self.stub.calls if c[0] == "/op/crop"]
        self.assertEqual(len(crops), 6)

    def test_tiled_output_equals_untiled_when_sampler_is_local(self):
        """桩网关的缩放是最近邻（局部算子）→ 分块不该引入任何像素差。"""
        untiled = judge(self.tmp, self.src, {"scale": 2, "engine": "full"},
                        asset_id="untiled")["rows"][0]
        tiled = judge(self.tmp, self.src, {"scale": 2, "engine": "full", "tile": "384"},
                      asset_id="tiled")["rows"][0]
        self.assertEqual(untiled["result"], "ok")
        self.assertEqual(tiled["result"], "ok")
        self.assertEqual(untiled["evidence"]["sha256"], tiled["evidence"]["sha256"],
                         "拼缝必须落在边距算出的像素上：本地算子下应与整图逐字节一致")

    def test_auto_tiles_only_when_over_model_cap(self):
        small = judge(self.tmp, self.src, {"scale": 2, "engine": "full"}, asset_id="small")["rows"][0]
        self.assertIsNone(small["evidence"]["tileBlock"], "900x600 < 4MP 不该分块")
        self.assertNotIn("tiles", small["evidence"])

    def test_tile_crop_boxes_have_margin_and_clip_to_image(self):
        judge(self.tmp, self.src, {"scale": 2, "engine": "full", "tile": "384"}, asset_id="a")
        boxes = [tuple(c[1]["box"]) for c in self.stub.calls if c[0] == "/op/crop"]
        self.assertIn((0, 0, 392, 392), boxes, "首块向外扩 8px 边距")
        self.assertIn((760, 0, 900, 392), boxes, "最右块裁剪框夹到图宽（768−8 起）")
        self.assertIn((760, 376, 900, 600), boxes, "右下角块两个方向都夹到图边")


class CpuGateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = _support.tempdir()
        self.lockdir = os.path.join(self.tmp, "locks")

    def test_single_slot_serialises_threads(self):
        cpulock = plugin.cpulock
        active = []
        peak = []

        def work():
            with cpulock.CpuGate(slots=1, lock_dir=self.lockdir) as gate:
                active.append(1)
                peak.append(len(active))
                _ = gate.wait_ms
                time.sleep(0.15)
                active.pop()

        threads = [threading.Thread(target=work) for _ in range(3)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(max(peak), 1, "1 槽时同机并发必须是 1")

    def test_two_slots_allow_two(self):
        cpulock = plugin.cpulock
        active = []
        peak = []

        def work():
            with cpulock.CpuGate(slots=2, lock_dir=self.lockdir):
                active.append(1)
                peak.append(len(active))
                time.sleep(0.15)
                active.pop()

        threads = [threading.Thread(target=work) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(max(peak), 2)
        self.assertLessEqual(max(peak), 2)

    def test_wait_ms_recorded_when_queued(self):
        cpulock = plugin.cpulock
        holder = cpulock.CpuGate(slots=1, lock_dir=self.lockdir)
        holder.__enter__()
        try:
            gate = cpulock.CpuGate(slots=1, lock_dir=self.lockdir)
            timer = threading.Timer(0.2, lambda: holder.__exit__(None, None, None))
            timer.start()
            gate.__enter__()
            gate.__exit__(None, None, None)
            timer.cancel()
            self.assertGreaterEqual(gate.wait_ms, 100, "排过队就必须有 waitMs")
        finally:
            try:
                holder.__exit__(None, None, None)
            except Exception:  # noqa: BLE001
                pass

    def test_notify_called_while_waiting(self):
        cpulock = plugin.cpulock
        notes = []
        holder = cpulock.CpuGate(slots=1, lock_dir=self.lockdir)
        holder.__enter__()
        try:
            gate = cpulock.CpuGate(slots=1, lock_dir=self.lockdir,
                                   notify=notes.append, job_id="job-w")
            timer = threading.Timer(2.3, lambda: holder.__exit__(None, None, None))
            timer.start()
            gate.__enter__()
            gate.__exit__(None, None, None)
            timer.cancel()
        finally:
            try:
                holder.__exit__(None, None, None)
            except Exception:  # noqa: BLE001
                pass
        self.assertTrue(any(n.get("step") == "waiting" for n in notes),
                        "等待期间必须发 progress{step:waiting}：%r" % (notes,))


class PngioTest(unittest.TestCase):
    def test_detect_size_matches_decode(self):
        tmp = _support.tempdir()
        path = _support.gradient_png(os.path.join(tmp, "g.png"), 37, 19)
        self.assertEqual(pngio.detect_size(path), (37, 19, 3))

    def test_detect_size_rejects_non_png(self):
        tmp = _support.tempdir()
        path = os.path.join(tmp, "x.png")
        with open(path, "wb") as fh:
            fh.write(b"not a png at all")
        with self.assertRaises(pngio.PngError):
            pngio.detect_size(path)


if __name__ == "__main__":
    unittest.main(verbosity=2)
