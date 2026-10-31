"""N5a 结果行契约（SDK 侧）：构造器自检 + 处理类禁 verdict。"""
import unittest

from dampack import (
    ERR_INVALID_PARAMS,
    RpcError,
    export_file,
    failed_row,
    judge_row,
    process_row,
    reply,
    result_row,
    skipped_row,
    working_version,
)
from dampack.protocol import validate_result_row


class TestRowBuilders(unittest.TestCase):
    def test_process_row_has_result_and_artifacts_but_no_verdict(self):
        row = process_row("42", artifacts=[working_version("42", "v3")],
                          evidence={"op": "resize"}, fields={"out_width": 1600})
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["assetId"], "42")
        self.assertNotIn("verdict", row)
        self.assertEqual(row["artifacts"][0]["kind"], "working_version")
        self.assertEqual(validate_result_row(row, ("process",)), [])

    def test_process_row_cannot_smuggle_verdict(self):
        # 处理类带 verdict → 自检直接抛错（构造期就挡住，不留半截行）
        with self.assertRaises(RpcError) as ctx:
            result_row("42", "ok", kinds=["process"],
                       verdict={"status": "pass", "confidence": 1.0})
        self.assertEqual(ctx.exception.code, ERR_INVALID_PARAMS)
        self.assertIn("row_verdict_not_allowed", str(ctx.exception))

    def test_judge_row_carries_verdict_three_states(self):
        for status in ("pass", "fail", "undetermined"):
            row = judge_row("a1", rule_id="A4", status=status, confidence=0.5,
                            reason_code="A4_subject_ratio")
            self.assertEqual(row["verdict"]["status"], status)
            self.assertEqual(validate_result_row(row, ("judge",)), [])
        # skipped 是执行结果，不是审核结论
        with self.assertRaises(RpcError) as ctx:
            judge_row("a1", rule_id="A4", status="skipped")
        self.assertIn("row_verdict_status_invalid", str(ctx.exception))

    def test_confidence_out_of_range_is_refused(self):
        with self.assertRaises(RpcError) as ctx:
            judge_row("a1", rule_id="A4", status="pass", confidence=1.4)
        self.assertIn("row_verdict_confidence_invalid", str(ctx.exception))

    def test_failed_and_skipped_need_reason_fields(self):
        self.assertEqual(failed_row("7", "asset_unreadable", "解码失败")["error"]["code"],
                         "asset_unreadable")
        self.assertEqual(skipped_row("7", "原图缺失")["skipReason"], "原图缺失")
        # result=failed 缺 error → 自检挡住
        with self.assertRaises(RpcError):
            result_row("7", "failed", kinds=["process"])
        with self.assertRaises(RpcError):
            result_row("7", "skipped", kinds=["process"])

    def test_invented_result_status_is_refused(self):
        with self.assertRaises(RpcError) as ctx:
            result_row("7", "optimized", kinds=["process"])
        self.assertIn("row_result_invalid", str(ctx.exception))

    def test_artifacts_only_two_destinations(self):
        self.assertEqual(export_file("exports/a.png", "ab12")["kind"], "export_file")
        for bad in ({"kind": "own_dir", "path": "x"},
                    {"kind": "working_version", "assetId": "1"},
                    {"kind": "export_file", "path": "exports/a.png"}):
            with self.assertRaises(RpcError) as ctx:
                process_row("1", artifacts=[bad])
            self.assertIn("row_artifact_invalid", str(ctx.exception))

    def test_reply_wraps_rows(self):
        body = reply([process_row("1"), process_row("2", artifacts=[working_version("2", "v1")])])
        self.assertEqual([r["assetId"] for r in body["rows"]], ["1", "2"])

    def test_kinds_closed_set(self):
        with self.assertRaises(RpcError):
            result_row("1", "ok", kinds=["analyzer"])


if __name__ == "__main__":
    unittest.main()
