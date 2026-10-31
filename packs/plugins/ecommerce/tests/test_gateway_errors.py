"""网关调用面的两类回归（K07 `task` 修复卡）：

1. **错误必须可定位**：`gw()` 的 `-32003` 文案要带「哪个 op + 实际参数 + 网关 detail」。
   旧实现只读错误体的 `error` 字段，宿主侧只能记下「参数非法（400）invalid_params」，
   拿不到任何定位信息（`reports/task-K07.md` §5.2 / D7）。
2. **小图多倍放大不许被插件侧夹掉**：`f005`（554×500，小主体）的几何解给出
   `scale≈12.7~18.6`，旧网关 8× 硬门槛让修复链直接失败。宿主现在分趟支持，
   插件要**如实把解出的倍数发出去**，并把网关自报的口径（`op`/`passes`）留在 trail 里。
"""
from __future__ import annotations

import json
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import _support

import ruleset
from behavorkit import FakeImageGateway, Image

PLUGIN = _support.load_plugin()


class _StubErrorServer:
    """按预置 `(status, payload)` 回错的极小 HTTP 服务（只服务 gw() 的错误面）。"""

    def __init__(self, status, payload, token="err-token"):
        self.status = status
        self.payload = payload
        self.token = token
        self.requests = []
        self._httpd = None

    def start(self):
        srv = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b"{}"
                srv.requests.append({"path": self.path,
                                     "payload": json.loads(raw.decode("utf-8") or "{}")})
                body = json.dumps(srv.payload, ensure_ascii=False).encode("utf-8")
                self.send_response(srv.status)
                self.send_header("content-type", "application/json; charset=utf-8")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        return self

    @property
    def base_url(self):
        host, port = self._httpd.server_address[:2]
        return "http://%s:%d" % (host, port)

    def stop(self):
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()


class GatewayErrorContextTest(unittest.TestCase):
    def call_against(self, status, payload, path, request):
        server = _StubErrorServer(status, payload).start()
        orig_url, orig_token = PLUGIN._GATEWAY_URL, PLUGIN._GATEWAY_TOKEN
        PLUGIN._GATEWAY_URL, PLUGIN._GATEWAY_TOKEN = server.base_url, server.token
        try:
            with self.assertRaises(PLUGIN.RpcError) as ctx:
                PLUGIN.gw(path, request)
        finally:
            PLUGIN._GATEWAY_URL, PLUGIN._GATEWAY_TOKEN = orig_url, orig_token
            server.stop()
        return ctx.exception

    def test_400_carries_op_params_and_detail(self):
        detail = ("upscale 目标 10304x9300（长边 10304）超上限：像素 ≤100000000、"
                  "源图 554x500 的可用上限 scale<=64.000")
        exc = self.call_against(
            400,
            {"error": "invalid_params", "detail": detail,
             "op": "/op/upscale", "params": {"scale": 18.6, "out": "/w/u.png"}},
            "/op/upscale",
            {"path": "/w/src.png", "scale": 18.6, "out": "/w/u.png"},
        )
        self.assertEqual(exc.code, -32003)
        # 三个要素缺一不可：端点、实际参数、网关 detail
        self.assertIn("/op/upscale", exc.message)
        self.assertIn("18.6", exc.message)
        self.assertIn("可用上限", exc.message)
        # 不再退化成裸错误
        self.assertNotEqual(exc.message.strip(), "参数非法（400）invalid_params")

    def test_403_carries_capability_context(self):
        exc = self.call_against(
            403,
            {"error": "capability_missing", "capability": "op.generative",
             "detail": "manifest 未声明该能力", "op": "/op/inpaint",
             "params": {"path": "/w/src.png"}},
            "/op/inpaint",
            {"path": "/w/src.png", "mask": "/w/m.png", "out": "/w/i.png"},
        )
        self.assertEqual(exc.code, -32001)
        self.assertIn("/op/inpaint", exc.message)
        self.assertIn("manifest 未声明该能力", exc.message)

    def test_legacy_error_body_without_op_still_names_the_path(self):
        # 兼容旧宿主/假网关（只有 error + detail）：至少点名调的是哪个端点
        exc = self.call_against(
            400,
            {"error": "invalid_params", "detail": "缺字段 scale"},
            "/op/upscale",
            {"path": "/w/src.png"},
        )
        self.assertEqual(exc.code, -32003)
        self.assertIn("/op/upscale", exc.message)
        self.assertIn("缺字段 scale", exc.message)


class SmallAssetMultiPassTest(unittest.TestCase):
    """`f005` 场景：小图 + 小主体，几何解出的倍数远超旧的 8× 单趟门槛。"""

    def setUp(self):
        self.root = _support.tempdir("pack-f005-")
        self.workdir = os.path.join(self.root, "work")
        os.makedirs(self.workdir, exist_ok=True)
        self._orig_url = PLUGIN._GATEWAY_URL
        self._orig_token = PLUGIN._GATEWAY_TOKEN
        PLUGIN._CANCEL.clear()

    def tearDown(self):
        PLUGIN._GATEWAY_URL = self._orig_url
        PLUGIN._GATEWAY_TOKEN = self._orig_token
        PLUGIN._CANCEL.clear()

    def test_geometry_scale_above_eight_is_sent_and_converges(self):
        # 554×500 的真实尺寸（K07 §5.2）；主体 120×100（fg≈0.043）→ ideal≈126px
        gw = FakeImageGateway(self.root, Image(554, 500, (217, 200, 337, 300))).start()
        self.addCleanup(gw.stop)
        PLUGIN._GATEWAY_URL, PLUGIN._GATEWAY_TOKEN = gw.base_url, gw.token
        asset = gw.seed_asset("f005.png")

        result = PLUGIN.repair("job-f005", {"id": "f005", "path": asset}, None,
                               {"workdir": self.workdir})
        verdicts = {v["ruleId"]: v for v in result["verdicts"]}

        upscales = []
        for item in result["trail"]:
            ops = item.get("ops") if isinstance(item, dict) else None
            for op in (ops or []):
                if op.get("op") == "upscale":
                    upscales.append(op)
            if isinstance(item, dict) and item.get("op") == "upscale":
                upscales.append(item)
        self.assertTrue(upscales, "几何解必须走到 upscale：%r" % (result["trail"],))
        scale = float(upscales[0]["scale"])
        self.assertGreater(scale, 8.0,
                           "小图必须如实发出 >8 倍（旧网关的硬门槛已由分趟承接）")
        # 网关自报口径留在 trail 里（人工核对「为什么这么大倍数还能过」）
        self.assertIn("gateway_op", upscales[0])
        self.assertIn("passes", upscales[0])

        self.assertEqual(verdicts["R1_size"]["status"], "pass")
        self.assertTrue(verdicts["R1_size"]["fixed"])
        artifact = next(a for a in result["artifacts"] if a["rule_id"] == "R1_size")
        self.assertGreaterEqual(min(artifact["width"], artifact["height"]),
                                ruleset.THRESHOLDS["R1_size"]["short_side_min"])
        self.assertTrue(os.path.isfile(artifact["path"]))


if __name__ == "__main__":
    unittest.main()
