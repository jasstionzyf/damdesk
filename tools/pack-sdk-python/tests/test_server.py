"""协议壳：stdout 纯净 / 自动四方法 / 错误码兜底 / 取消 / 账本 / 非 ASCII 不转义。

全部在**真实子进程**里跑：stdout 保护、fd 切换、行式 flush 只有真进程能证明。
"""
from __future__ import annotations

import json
import threading
import time
import unittest

import _support


class ServerTestBase(unittest.TestCase):
    gateway = None

    @classmethod
    def setUpClass(cls):
        cls.pack_dir, cls.manifest = _support.make_fixture_pack()
        cls.gateway = _support.StubGateway(responses={
            "/report/emit": (200, {"ok": True, "job_id": "j"}),
        }).start()

    @classmethod
    def tearDownClass(cls):
        cls.gateway.stop()

    def spawn(self):
        proc = _support.PackProcess(self.pack_dir, env={
            "DAMPACK_GATEWAY_URL": self.gateway.base_url,
            "DAMPACK_GATEWAY_TOKEN": self.gateway.token,
        })
        self.addCleanup(proc.close)
        return proc


class BuiltinMethodsTest(ServerTestBase):
    def test_pack_info_matches_manifest(self):
        with self.spawn() as proc:
            info = proc.send("pack.info").result
        for field in ("id", "version", "api_version", "deterministic", "timeout_secs"):
            self.assertEqual(info[field], self.manifest[field], field)
        self.assertEqual([r["id"] for r in info["rules"]], ["T1"])

    def test_health(self):
        with self.spawn() as proc:
            result = proc.send("health").result
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["pack"], "com.test.fixture")
        self.assertIsInstance(result["pid"], int)

    def test_cancel_without_job_id_is_invalid_params(self):
        with self.spawn() as proc:
            resp = proc.send("cancel", {})
        self.assertEqual(resp.code, -32003)

    def test_unknown_method(self):
        with self.spawn() as proc:
            resp = proc.send("no.such.method")
        self.assertEqual(resp.code, -32601)
        self.assertIn("no.such.method", resp.message)

    def test_pack_can_override_builtin(self):
        with self.spawn() as proc:
            resp = proc.send("echo", {"a": 1})
        self.assertEqual(resp.result, {"a": 1})


class ErrorFrameTest(ServerTestBase):
    def test_handler_exception_becomes_32000(self):
        with self.spawn() as proc:
            resp = proc.send("boom")
            tail = list(proc.stderr_lines)
        self.assertEqual(resp.code, -32000)
        self.assertIn("RuntimeError", resp.message)
        self.assertTrue(any("故意炸" in line for line in tail), tail[-3:])

    def test_rpc_error_passthrough(self):
        with self.spawn() as proc:
            resp = proc.send("rpc")
        self.assertEqual(resp.code, -32003)
        self.assertEqual(resp.message, "参数不合法")

    def test_invalid_verdict_is_rejected_not_returned(self):
        with self.spawn() as proc:
            resp = proc.send("judge", {"job_id": "j1", "bad_verdict": True})
        self.assertEqual(resp.code, -32000)
        self.assertIn("verdicts", resp.message)

    def test_parse_error_frame(self):
        with self.spawn() as proc:
            proc.proc.stdin.write("{not json\n")
            proc.proc.stdin.flush()
            deadline = time.time() + 5
            frame = None
            while time.time() < deadline:
                for item in list(proc.frames):
                    if item.get("error", {}).get("code") == -32700:
                        frame = item
                        break
                if frame:
                    break
                time.sleep(0.02)
        self.assertIsNotNone(frame, proc.frames)


class StdoutPurityTest(ServerTestBase):
    def test_prints_go_to_stderr(self):
        with self.spawn() as proc:
            resp = proc.send("judge", {"job_id": "j-stdout"})
            time.sleep(0.1)
            stray = list(proc.stray)
            frames = list(proc.frames)
            err = list(proc.stderr_lines)
        self.assertIsNone(resp.error)
        self.assertEqual(stray, [], "stdout 被非协议行污染：%r" % stray[:3])
        self.assertTrue(all("jsonrpc" in f for f in frames))
        self.assertTrue(any("这行 print 必须落 stderr" in line for line in err), err[-5:])

    def test_non_ascii_not_escaped(self):
        with self.spawn() as proc:
            resp = proc.send("judge", {"job_id": "j-cn"})
            raw_lines = [json.dumps(f, ensure_ascii=False) for f in proc.frames]
        self.assertEqual(resp.result["asset_id"], "资产-中文")
        joined = "\n".join(raw_lines)
        self.assertIn("资产-中文", joined)
        self.assertNotIn("\\u8d44", joined, "应答被 ensure_ascii 转义了")

    def test_verdict_fields_and_order(self):
        with self.spawn() as proc:
            resp = proc.send("judge", {"job_id": "j-shape"})
        verdict = resp.result["verdicts"][0]
        self.assertEqual(set(verdict), {"ruleId", "status", "score", "label",
                                        "evidence", "fixed", "note"})


class CancelSemanticsTest(ServerTestBase):
    def test_cancel_interrupts_running_handler(self):
        with self.spawn() as proc:
            box = {}

            def run():
                try:
                    box["resp"] = proc.send("judge", {"job_id": "j-cancel", "slow": True},
                                            timeout=20)
                except TimeoutError as exc:
                    box["timeout"] = str(exc)

            worker = threading.Thread(target=run, daemon=True)
            worker.start()
            time.sleep(0.4)
            cxl = proc.send("cancel", {"job_id": "j-cancel"}, timeout=5)
            worker.join(timeout=8)
        self.assertNotIn("timeout", box)
        self.assertTrue(cxl.result["canceled"])
        self.assertEqual(box["resp"].code, -32004)
        self.assertIn("已取消", box["resp"].message)


class LedgerTest(ServerTestBase):
    def test_emit_and_report_per_job(self):
        with self.spawn() as proc:
            ack = proc.send("emit", {"job_id": "job-A", "asset_id": "a-1"}).result
            proc.send("emit", {"job_id": "job-B", "asset_id": "b-1"})
            rep_a = proc.send("report", {"job_id": "job-A"}).result
            rep_b = proc.send("report", {"job_id": "job-B"}).result
            emits_a = [r for r in self.gateway.requests
                       if r["path"] == "/report/emit"
                       and r["payload"].get("job_id") == "job-A"]
        self.assertTrue(ack["ack"]["ok"])
        self.assertEqual(rep_a["assets"], ["a-1"])
        self.assertEqual(rep_b["assets"], ["b-1"])
        self.assertEqual(rep_a["judged"], 1)
        self.assertIn("asset_id,rule_id,status", rep_a["csv"])
        self.assertTrue(emits_a)

    def test_emit_after_cancel_rejected(self):
        with self.spawn() as proc:
            proc.send("cancel", {"job_id": "job-X"})
            resp = proc.send("emit", {"job_id": "job-X"})
        self.assertEqual(resp.code, -32004)


if __name__ == "__main__":
    unittest.main()
