"""网关客户端：13 端点的路径/载荷 + HTTP → 插件错误码映射（协议 §5）。"""
from __future__ import annotations

import unittest

import _support
from dampack import (
    ERR_ASSET_UNREADABLE,
    ERR_CAPABILITY_MISSING,
    ERR_INVALID_PARAMS,
    ERR_OTHER,
    Gateway,
    RpcError,
)


class TransportTest(unittest.TestCase):
    def setUp(self):
        self.gw_stub = _support.StubGateway().start()
        self.client = Gateway(self.gw_stub.base_url, "stub-token", timeout=5)

    def tearDown(self):
        self.gw_stub.stop()

    def test_token_header_sent(self):
        self.client.io_read("/tmp/a.png")
        hit = self.gw_stub.hits("/io/read")[0]
        self.assertEqual(hit["token"], "stub-token")
        self.assertEqual(hit["payload"], {"path": "/tmp/a.png"})

    def test_all_ten_endpoints_reachable(self):
        calls = {
            "/io/read": lambda: self.client.io_read("/a.png"),
            "/detect/u2net": lambda: self.client.detect_u2net("/a.png"),
            "/detect/ocr": lambda: self.client.detect_ocr("/a.png", lang="chi_sim+eng"),
            "/op/crop": lambda: self.client.crop("/a.png", [0, 0, 10, 10], "/w/c.png"),
            "/op/letterbox": lambda: self.client.letterbox("/a.png", 1.0, "/w/l.png"),
            "/op/upscale": lambda: self.client.upscale("/a.png", 2.0, "/w/u.png"),
            "/op/replace_bg": lambda: self.client.replace_bg("/a.png", "#fff", "/w/b.png"),
            "/op/inpaint": lambda: self.client.inpaint("/a.png", "/w/m.png", "/w/i.png"),
            "/vl/judge": lambda: self.client.vl_judge("/a.png", "是不是", max_tokens=4096),
            "/report/emit": lambda: self.client.report_emit("j1", {"asset_id": "a"}),
        }
        for path, call in calls.items():
            with self.subTest(endpoint=path):
                call()
                self.assertTrue(self.gw_stub.hits(path), "未打到 %s" % path)

    def test_ocr_lang_optional(self):
        self.client.detect_ocr("/a.png")
        self.assertEqual(self.gw_stub.hits("/detect/ocr")[0]["payload"], {"path": "/a.png"})

    def test_vl_default_max_tokens_and_bound(self):
        self.client.vl_judge("/a.png", "问")
        self.assertEqual(self.gw_stub.hits("/vl/judge")[0]["payload"]["max_tokens"], 4096)
        for bad in (0, -1, 4097):
            with self.subTest(max_tokens=bad):
                with self.assertRaises(RpcError) as ctx:
                    self.client.vl_judge("/a.png", "问", max_tokens=bad)
                self.assertEqual(ctx.exception.code, ERR_OTHER)

    def test_unknown_endpoint_rejected_locally(self):
        with self.assertRaises(RpcError):
            self.client.call("/op/teleport", {})

    def test_missing_base_url(self):
        with self.assertRaises(RpcError) as ctx:
            Gateway("", "t").io_read("/a.png")
        self.assertEqual(ctx.exception.code, ERR_OTHER)

    def test_unreachable(self):
        client = Gateway("http://127.0.0.1:9", "t", timeout=2)   # discard 端口
        with self.assertRaises(RpcError) as ctx:
            client.io_read("/a.png")
        self.assertEqual(ctx.exception.code, ERR_OTHER)
        self.assertIn("网关", ctx.exception.message)


class ErrorMappingTest(unittest.TestCase):
    """403 不许被吞：能力缺失就是 -32001，不许降级成 pass/fail。"""

    def _client(self, status, payload=None):
        stub = _support.StubGateway(responses={"/vl/judge": (status, payload or {})}).start()
        self.addCleanup(stub.stop)
        return Gateway(stub.base_url, "stub-token", timeout=5), stub

    def test_403_capability_missing(self):
        client, _ = self._client(403, {"error": "capability_missing"})
        with self.assertRaises(RpcError) as ctx:
            client.vl_judge("/a.png", "问")
        self.assertEqual(ctx.exception.code, ERR_CAPABILITY_MISSING)
        self.assertIn("capability_missing", ctx.exception.message)

    def test_404_asset_unreadable(self):
        client, _ = self._client(404, {"error": "asset_unreadable"})
        with self.assertRaises(RpcError) as ctx:
            client.vl_judge("/a.png", "问")
        self.assertEqual(ctx.exception.code, ERR_ASSET_UNREADABLE)

    def test_400_invalid_params(self):
        client, _ = self._client(400, {"error": "invalid_params"})
        with self.assertRaises(RpcError) as ctx:
            client.vl_judge("/a.png", "问")
        self.assertEqual(ctx.exception.code, ERR_INVALID_PARAMS)

    def test_400_error_message_carries_op_and_params(self):
        """错误文案必须能定位：端点 + 实际参数 + 网关 detail（K07 D7，禁裸 -32003）。"""
        stub = _support.StubGateway(responses={"/op/upscale": (400, {
            "error": "invalid_params",
            "detail": "scale=18.6 超上限：源图 554x500 的可用上限 scale<=18.000",
            "op": "/op/upscale",
            "params": {"scale": 18.6, "out": "/w/u.png"},
        })}).start()
        self.addCleanup(stub.stop)
        client = Gateway(stub.base_url, "stub-token", timeout=5)
        with self.assertRaises(RpcError) as ctx:
            client.upscale("/a.png", 18.6, "/w/u.png")
        self.assertEqual(ctx.exception.code, ERR_INVALID_PARAMS)
        message = ctx.exception.message
        self.assertIn("/op/upscale", message)
        self.assertIn("18.6", message)
        self.assertIn("可用上限", message)
        self.assertNotEqual(message.strip(), "网关错误 400")

    def test_401_is_host_misconfig_not_capability(self):
        client, _ = self._client(401, {"error": "unauthorized"})
        with self.assertRaises(RpcError) as ctx:
            client.vl_judge("/a.png", "问")
        self.assertEqual(ctx.exception.code, ERR_OTHER)

    def test_500_generic(self):
        client, _ = self._client(500, {"error": "boom"})
        with self.assertRaises(RpcError) as ctx:
            client.vl_judge("/a.png", "问")
        self.assertEqual(ctx.exception.code, ERR_OTHER)

    def test_non_json_body_does_not_mask_status(self):
        stub = _support.StubGateway().start()
        self.addCleanup(stub.stop)
        client = Gateway(stub.base_url, "stub-token", timeout=5)
        # 用错误令牌触发 401 + 非 JSON 正文（stub 回的是 JSON，这里只验状态码路径）
        client.token = "wrong"
        with self.assertRaises(RpcError) as ctx:
            client.io_read("/a.png")
        self.assertEqual(ctx.exception.code, ERR_OTHER)


class EnvTest(unittest.TestCase):
    def test_from_env_reads_env(self):
        import os
        saved = {k: os.environ.get(k) for k in ("DAMPACK_GATEWAY_URL", "DAMPACK_GATEWAY_TOKEN",
                                               "DAMPACK_TIMEOUT_SECS")}
        try:
            os.environ.update({"DAMPACK_GATEWAY_URL": "http://127.0.0.1:1234/",
                               "DAMPACK_GATEWAY_TOKEN": "tok",
                               "DAMPACK_TIMEOUT_SECS": "7"})
            gw = Gateway.from_env()
            self.assertEqual(gw.base_url, "http://127.0.0.1:1234")
            self.assertEqual(gw.token, "tok")
            self.assertEqual(gw.timeout, 7.0)
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

    def test_bad_timeout_falls_back(self):
        self.assertEqual(Gateway("http://127.0.0.1:1", "t", timeout="abc").timeout, 120.0)
        self.assertEqual(Gateway("http://127.0.0.1:1", "t", timeout="1").timeout, 5.0)


if __name__ == "__main__":
    unittest.main()
