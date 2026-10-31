"""协议壳单测：行式 JSON-RPC 帧、错误码闭集、stdout 纯净、stdio 主循环。"""
from __future__ import annotations

import io
import json
import unittest

import _support

PLUGIN = _support.load_plugin()

ERROR_CODES = {-32001, -32002, -32003, -32004, -32005, -32000, -32601, -32700}


class FrameTest(unittest.TestCase):
    def handle(self, line):
        return PLUGIN._handle_line(line)

    def test_pack_info_matches_manifest(self):
        frame = self.handle('{"jsonrpc":"2.0","id":1,"method":"pack.info","params":{}}')
        self.assertEqual(frame["id"], 1)
        info = frame["result"]
        with open(_support.PACK_DIR + "/manifest.json", encoding="utf-8") as fh:
            manifest = json.load(fh)
        for field in ("id", "version", "api_version", "deterministic", "timeout_secs"):
            self.assertEqual(info[field], manifest[field], field)
        self.assertEqual(set(info["capabilities"]), set(manifest["capabilities"]))
        self.assertTrue(info["rules"])
        self.assertTrue(all(rule.get("id") for rule in info["rules"]))

    def test_health(self):
        frame = self.handle('{"jsonrpc":"2.0","id":2,"method":"health","params":{}}')
        self.assertEqual(frame["result"]["status"], "ok")

    def test_malformed_json(self):
        frame = self.handle("{not json")
        self.assertEqual(frame["error"]["code"], -32700)
        self.assertNotIn("result", frame)

    def test_non_object_frame(self):
        frame = self.handle("[1,2,3]")
        self.assertEqual(frame["error"]["code"], -32600)

    def test_unknown_method(self):
        frame = self.handle('{"jsonrpc":"2.0","id":3,"method":"no.such","params":{}}')
        self.assertEqual(frame["error"]["code"], -32601)
        self.assertNotIn("result", frame)

    def test_cancel_request_and_notification(self):
        frame = self.handle('{"jsonrpc":"2.0","id":4,"method":"cancel",'
                            '"params":{"job_id":"j-1"}}')
        self.assertEqual(frame["result"]["canceled"], True)
        self.assertTrue(PLUGIN._cancel_flag("j-1").is_set())
        # 通知帧（无 id）不回帧
        self.assertIsNone(self.handle('{"jsonrpc":"2.0","method":"cancel",'
                                      '"params":{"job_id":"j-2"}}'))

    def test_unknown_notification_is_silent(self):
        self.assertIsNone(self.handle('{"jsonrpc":"2.0","method":"no.such"}'))

    def test_internal_error_becomes_minus_32000(self):
        original = PLUGIN.HANDLERS["health"]
        PLUGIN.HANDLERS["health"] = lambda params: (_ for _ in ()).throw(RuntimeError("boom"))
        try:
            frame = self.handle('{"jsonrpc":"2.0","id":5,"method":"health","params":{}}')
        finally:
            PLUGIN.HANDLERS["health"] = original
        self.assertEqual(frame["error"]["code"], -32000)
        self.assertIn("boom", frame["error"]["message"])
        self.assertIn(frame["error"]["code"], ERROR_CODES)

    def test_verdict_bad_params_error_code(self):
        frame = self.handle('{"jsonrpc":"2.0","id":6,"method":"judge","params":{}}')
        self.assertEqual(frame["error"]["code"], -32003)


class ServeTest(unittest.TestCase):
    def test_stdio_loop_emits_only_frames(self):
        stdin = io.StringIO("\n".join([
            '{"jsonrpc":"2.0","id":1,"method":"health","params":{}}',
            "",
            "{broken",
            '{"jsonrpc":"2.0","id":2,"method":"pack.info","params":{}}',
        ]) + "\n")
        stdout = io.StringIO()
        PLUGIN.serve(stdin=stdin, stdout=stdout)
        lines = [line for line in stdout.getvalue().splitlines() if line.strip()]
        self.assertEqual(len(lines), 3)          # 空行不回帧，其余 3 帧都有回应
        for line in lines:
            frame = json.loads(line)             # 每行都必须能解析
            self.assertEqual(frame["jsonrpc"], "2.0")
            self.assertTrue("result" in frame or "error" in frame)
            self.assertFalse("result" in frame and "error" in frame)

    def test_no_banner_on_stdout(self):
        """启动日志必须走 stderr：stdout 第一行就得是协议帧。"""
        stdin = io.StringIO('{"jsonrpc":"2.0","id":9,"method":"health","params":{}}\n')
        stdout = io.StringIO()
        PLUGIN.serve(stdin=stdin, stdout=stdout)
        first = stdout.getvalue().splitlines()[0]
        self.assertEqual(json.loads(first)["id"], 9)


class FrameEncodingTest(unittest.TestCase):
    """帧编码的字节面：一帧一行、UTF-8 原样（不转 \\uXXXX）、通知不回帧。"""

    def serve(self, payload):
        stdout = io.StringIO()
        PLUGIN.serve(stdin=io.StringIO(payload), stdout=stdout)
        return stdout.getvalue()

    def test_one_frame_per_line(self):
        out = self.serve('{"jsonrpc":"2.0","id":1,"method":"pack.info","params":{}}\n')
        self.assertEqual(out.count("\n"), 1, "一个请求只能产生一行（多行会让宿主解析错位）")
        self.assertTrue(out.endswith("\n"), "帧必须以换行结尾")

    def test_non_ascii_is_raw_utf8_not_escaped(self):
        out = self.serve('{"jsonrpc":"2.0","id":1,"method":"pack.info","params":{}}\n')
        line = out.splitlines()[0]
        self.assertIn("电商", line, "非 ASCII 必须原样 UTF-8（ensure_ascii=False）")
        self.assertNotIn("\\u", line, "不得转义成 \\uXXXX（否则中文名在日志里不可读）")
        self.assertEqual(json.loads(line)["result"]["name"], PLUGIN.PACK_INFO["name"])

    def test_notification_produces_no_frame(self):
        out = self.serve('{"jsonrpc":"2.0","method":"log","params":{"msg":"x"}}\n')
        self.assertEqual(out, "", "无 id 通知不得回帧（宿主会当成串号应答）")

    def test_each_request_gets_its_own_frame_and_id(self):
        payload = "\n".join([
            '{"jsonrpc":"2.0","id":11,"method":"health","params":{}}',
            '{"jsonrpc":"2.0","id":12,"method":"pack.info","params":{}}',
            '{"jsonrpc":"2.0","id":13,"method":"health","params":{}}',
        ]) + "\n"
        out = self.serve(payload)
        frames = [json.loads(line) for line in out.splitlines() if line.strip()]
        self.assertEqual(len(frames), 3, out)
        self.assertEqual(sorted(f["id"] for f in frames), [11, 12, 13], "id 必须原样回显")
        for frame in frames:
            self.assertEqual(frame["jsonrpc"], "2.0")
            self.assertFalse("result" in frame and "error" in frame, frame)


if __name__ == "__main__":
    unittest.main()
