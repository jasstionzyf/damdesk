"""协议壳：health/pack.info/未知方法/解析错误/取消 + 「stdout 只能是协议流」。"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import unittest

import _support

plugin = _support.load_plugin()


def _sdk_protocol():
    """官方 SDK 的协议投影（与宿主同源）：用它校验 manifest 与 pack.info，防漂移。"""
    sdk = os.path.join(_support.REPO_ROOT, "tools", "pack-sdk-python")
    if sdk not in sys.path:
        sys.path.insert(0, sdk)
    from dampack import protocol as sdk_proto  # noqa: WPS433
    return sdk_proto


class ManifestTest(unittest.TestCase):
    def test_manifest_matches_sdk_rules(self):
        sdk = _sdk_protocol()
        with open(os.path.join(_support.PACK_DIR, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        self.assertEqual(sdk.validate_manifest(manifest), [])
        self.assertEqual(manifest["outputs"], ["artifact"], "超分包是处理类（outputs=artifact），不产审核结论")

    def test_pack_info_matches_manifest(self):
        with open(os.path.join(_support.PACK_DIR, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        info = plugin.dispatch({"method": "pack.info", "params": {}})
        # 与 conformance 01 同口径：处理类包 rules 允许为空（"verdict" 类才要求非空）
        self.assertIsInstance(info["rules"], list)
        self.assertEqual(info["rules"], [], "超分包没有规则面（唯一动作是放大）")
        for key in ("id", "name", "version", "api_version", "deterministic", "timeout_secs"):
            self.assertEqual(info[key], manifest[key], "pack.info 与 manifest 不许漂移：%s" % key)
        self.assertEqual(sorted(info["capabilities"]), sorted(manifest["capabilities"]))


class ShellTest(unittest.TestCase):
    def test_health_carries_api_version(self):
        reply = plugin.dispatch({"method": "health", "params": {}})
        self.assertEqual(reply["status"], "ok")
        self.assertEqual(reply["api_version"], "2.0")

    def test_unknown_method(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "no.such", "params": {}})
        self.assertEqual(ctx.exception.code, -32601)
        self.assertIn("run", ctx.exception.message)

    def test_cancel_is_idempotent(self):
        first = plugin.dispatch({"method": "cancel", "params": {"job_id": "j"}})
        second = plugin.dispatch({"method": "cancel", "params": {"job_id": "j"}})
        self.assertTrue(first["canceled"] and second["canceled"])
        self.assertTrue(plugin._cancel_flag("j").is_set())

    def test_report_unknown_job_is_empty(self):
        plugin._REPORTS.clear()
        reply = plugin.dispatch({"method": "report", "params": {"job_id": "nope"}})
        self.assertEqual((reply["rows"], reply["assets"]), (0, []))

    def test_judge_requires_job_id(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "run", "params": {"asset": {"path": "/x.png"}}})
        self.assertEqual(ctx.exception.code, -32003)

    def test_judge_requires_asset_path(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            plugin.dispatch({"method": "run",
                             "params": {"job_id": "j", "asset": {}, "options": {}}})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("asset.path", ctx.exception.message)


class StdioPurityTest(unittest.TestCase):
    """真子进程：stdout 每一行都必须是 JSON 帧（任何库 print 都不许漏进来）。"""

    def test_only_json_frames_on_stdout(self):
        """逐条写、逐条读（宿主就是这么用的）：顺便证明 stdout 只有协议帧。"""
        entry = os.path.join(_support.PLUGIN_DIR, "__init__.py")
        proc = subprocess.Popen([sys.executable, entry], stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, bufsize=1,
                                env={**os.environ, "DAMPACK_GATEWAY_URL": "http://127.0.0.1:1"})
        try:
            ids = []
            for req_id, method in [(1, "health"), (2, "pack.info"), (3, "no.such")]:
                proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": req_id,
                                             "method": method, "params": {}}) + "\n")
                proc.stdin.flush()
                line = proc.stdout.readline()
                frame = json.loads(line)   # 非 JSON 会在这里直接炸
                ids.append(frame["id"])
                if method == "no.such":
                    self.assertEqual(frame["error"]["code"], -32601)
            self.assertEqual(ids, [1, 2, 3])
        finally:
            proc.stdin.close()
            proc.wait(timeout=30)

    def test_parse_error_frame(self):
        entry = os.path.join(_support.PLUGIN_DIR, "__init__.py")
        proc = subprocess.run([sys.executable, entry], input="{not json\n", capture_output=True,
                              text=True, timeout=60)
        reply = json.loads(proc.stdout.strip().splitlines()[0])
        self.assertEqual(reply["error"]["code"], -32700)

    def test_uncaught_exception_stays_inside_closed_code_set(self):
        """协议 §5「禁自造码」：兜底必须 -32000（曾用 JSON-RPC 的 -32603，闭集外 = 违例）。"""
        import io
        import threading
        import time

        def boom(_params):
            raise RuntimeError("模拟未捕获异常")

        plugin.HANDLERS["selftest.boom"] = boom
        stdin = io.StringIO(json.dumps({"jsonrpc": "2.0", "id": 7,
                                        "method": "selftest.boom", "params": {}}) + "\n")
        out = io.StringIO()
        saved_notify = plugin.NOTIFY
        try:
            shell = threading.Thread(target=plugin.serve, args=(stdin, out), daemon=True)
            shell.start()
            shell.join(timeout=20)
            for _ in range(100):
                if out.getvalue().strip():
                    break
                time.sleep(0.02)
        finally:
            plugin.HANDLERS.pop("selftest.boom", None)
            plugin.NOTIFY = saved_notify
        frame = json.loads(out.getvalue().strip().splitlines()[0])
        self.assertEqual(frame["error"]["code"], -32000)
        self.assertIn("内部错误", frame["error"]["message"])
        self.assertNotIn("result", frame)


if __name__ == "__main__":
    unittest.main(verbosity=2)
