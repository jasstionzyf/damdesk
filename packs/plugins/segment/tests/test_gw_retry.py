"""网关客户端（`gw()`）的传输层重试语义（N19 / 卡 task）。

真机形态：Windows 上网关把应答写出后，客户端有时读不到
（`getresponse` 阻塞到超时 / `RemoteDisconnected`），于是一次网关调用把整套 judge 预算
（300s）烧光，宿主只能报 `plugin_timeout`，产物静默丢失。

本文件钉住修好后的契约：
1. **只有传输层失败（没有任何 HTTP 应答）才重试**，且只重试一次；
2. 业务错误（403/400/404…）**一次都不重试**（协议 §5：不吞 403 的同一口径）；
3. `/report/emit` 是追加语义，**绝不重试**（重试会多记一条上报）；
4. 单次调用超时默认 60s（不是宿主给的 300s 预算），可被 `DAMPACK_GW_CALL_TIMEOUT_SECS` 覆盖；
5. `DAMPACK_GW_RETRY=0` 可以整体关掉重试（排障/回归对照用）。

跑法：`python3 -m unittest test_gw_retry -v`（在 packs/plugins/segment/tests 下）。
"""
from __future__ import annotations

import http.server
import json
import os
import socket
import threading
import unittest
import urllib.error

import _support  # noqa: F401  —— 装配 sys.path（plugin/ 平铺模块）


def _load():
    return _support.load_plugin()


class _ScriptedHandler(http.server.BaseHTTPRequestHandler):
    """按 `server.script` 逐次消费动作：('drop',) 断链 / ('json', status, payload)。"""

    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # noqa: D401 —— 静音，测试输出只留断言
        pass

    def do_POST(self):  # noqa: N802 —— BaseHTTPRequestHandler 约定
        srv = self.server
        srv.seen.append(self.path)
        action = srv.script.pop(0) if srv.script else ("json", 200, {"ok": True})
        length = int(self.headers.get("content-length") or 0)
        if length:
            self.rfile.read(length)
        if action[0] == "drop":
            # 不回任何字节就断开：模拟「网关写了应答但客户端读不到」（KI-29 真机形态）
            self.close_connection = True
            try:
                self.connection.close()
            except OSError:
                pass
            return
        _, status, payload = action
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _Server:
    def __init__(self, script):
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _ScriptedHandler)
        self.httpd.script = list(script)
        self.httpd.seen = []
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return "http://127.0.0.1:%d" % self.httpd.server_address[1]

    @property
    def seen(self):
        return self.httpd.seen

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class GwRetryTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plugin = _load()

    def setUp(self):
        self._env = dict(os.environ)

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._env)

    def _serve(self, script):
        srv = _Server(script)
        self.addCleanup(srv.close)
        os.environ["DAMPACK_GATEWAY_URL"] = srv.url
        os.environ["DAMPACK_GATEWAY_TOKEN"] = "t0ken"
        return srv

    # ---- ① 传输层失败 → 换新连接重试一次并成功 ---------------------------------
    def test_transport_failure_is_retried_once(self):
        srv = self._serve([("drop",), ("json", 200, {"width": 10, "height": 20})])
        out = self.plugin.gw("/io/read", {"path": "x.jpg"})
        self.assertEqual(out["width"], 10)
        self.assertEqual(srv.seen, ["/io/read", "/io/read"], "应当重试一次（共两条请求）")

    # ---- ② 两次都失败 → 结构化失败（不是无限重试） -----------------------------
    def test_two_transport_failures_raise_structured(self):
        srv = self._serve([("drop",), ("drop",), ("json", 200, {})])
        with self.assertRaises(self.plugin.RpcError) as ctx:
            self.plugin.gw("/io/read", {"path": "x.jpg"})
        self.assertEqual(ctx.exception.code, -32000)
        self.assertIn("共 2 次", ctx.exception.message)
        self.assertEqual(len(srv.seen), 2, "最多两次：不允许无限重试")

    # ---- ③ 业务错误不重试 ------------------------------------------------------
    def test_business_error_is_not_retried(self):
        srv = self._serve([("json", 403, {"error": "capability_missing"})])
        with self.assertRaises(self.plugin.RpcError) as ctx:
            self.plugin.gw("/detect/u2net", {"path": "x.jpg"})
        self.assertEqual(ctx.exception.code, -32001, "403 必须映射成能力未授权")
        self.assertEqual(len(srv.seen), 1, "业务错误一次都不许重试")

    # ---- ④ /report/emit 追加语义，绝不重试 ------------------------------------
    def test_report_emit_is_never_retried(self):
        srv = self._serve([("drop",), ("json", 200, {})])
        with self.assertRaises(self.plugin.RpcError):
            self.plugin.gw("/report/emit", {"job_id": "j", "assetId": "a"})
        self.assertEqual(len(srv.seen), 1, "/report/emit 是追加语义，重试会多记一条")

    # ---- ⑤ 开关：DAMPACK_GW_RETRY=0 关掉重试 ---------------------------------
    def test_retry_can_be_disabled(self):
        srv = self._serve([("drop",), ("json", 200, {})])
        os.environ["DAMPACK_GW_RETRY"] = "0"
        with self.assertRaises(self.plugin.RpcError):
            self.plugin.gw("/io/read", {"path": "x.jpg"})
        self.assertEqual(len(srv.seen), 1)

    # ---- ⑥ 单次调用超时：默认 60s，可覆盖，且与 judge 预算解耦 -----------------
    def test_call_timeout_default_and_override(self):
        os.environ.pop("DAMPACK_GW_CALL_TIMEOUT_SECS", None)
        self.assertEqual(self.plugin._http_timeout(), 60.0)
        os.environ["DAMPACK_GW_CALL_TIMEOUT_SECS"] = "17"
        self.assertEqual(self.plugin._http_timeout(), 17.0)
        # 旧变量不再影响单次调用（它代表的是整套 judge 预算）
        os.environ.pop("DAMPACK_GW_CALL_TIMEOUT_SECS", None)
        os.environ["DAMPACK_TIMEOUT_SECS"] = "300"
        self.assertEqual(self.plugin._http_timeout(), 60.0)

    # ---- ⑦ 连接被拒（网关没起来）也走「结构化失败」，不静默 --------------------
    def test_connection_refused_is_structured(self):
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        sock.close()
        os.environ["DAMPACK_GATEWAY_URL"] = "http://127.0.0.1:%d" % port
        os.environ["DAMPACK_GATEWAY_TOKEN"] = "t0ken"
        with self.assertRaises(self.plugin.RpcError) as ctx:
            self.plugin.gw("/io/read", {"path": "x.jpg"})
        self.assertIn("网关调用失败", ctx.exception.message)


if __name__ == "__main__":
    unittest.main()
