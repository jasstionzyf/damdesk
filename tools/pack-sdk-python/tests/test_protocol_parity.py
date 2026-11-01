"""SDK 常量/校验器与 conformance 可执行规格的**逐项一致性**（N5a 扩了结果行常量和校验）。

SDK 是发给第三方的手抄投影 —— 一旦抄错，第三方包会写出宿主看不懂的行，
所以这里不靠人眼：常量逐项比对，结果行校验用同一批样例对拍「违例集合」。
"""
import importlib.util
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.abspath(os.path.join(HERE, "..")))

import dampack.protocol as sdk  # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "conformance_protocol", os.path.join(ROOT, "tools", "pack-conformance", "protocol.py"))
conf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(conf)

SHARED_CONSTANTS = (
    "API_VERSION", "METHODS", "NOTIFICATIONS", "STATUSES", "VERDICT_FIELDS",
    "GATEWAY_ENDPOINTS", "KNOWN_CAPABILITIES", "MANIFEST_REQUIRED",
    # N5a
    "OUTPUTS", "RESULT_STATUSES", "VERDICT_STATUSES", "ROW_FIELDS", "ARTIFACT_KINDS",
    "LEGACY_HINT",
)

# 结果行样例：(标签, outputs, 行) —— 两侧校验必须给出同一组违例码
ROWS = (
    ("处理行合法", ("artifact",),
     {"assetId": "42", "result": "ok",
      "artifacts": [{"kind": "working_version", "assetId": "42", "versionId": "v1"}]}),
    ("处理行带 verdict", ("artifact",),
     {"assetId": "42", "result": "ok", "verdict": {"status": "pass"}}),
    ("判定行缺 verdict", ("verdict",), {"assetId": "1", "result": "ok"}),
    ("判定行合法", ("verdict",),
     {"assetId": "1", "ruleId": "A4", "result": "ok",
      "verdict": {"status": "fail", "confidence": 0.3, "reasonCode": "A4_x"}}),
    ("混合包资产级处理行", ("verdict", "artifact"),
     {"assetId": "1", "result": "ok"}),
    ("failed 缺 error", ("artifact",), {"assetId": "1", "result": "failed"}),
    ("skipped 缺 skipReason", ("artifact",), {"assetId": "1", "result": "skipped"}),
    ("result 越界", ("artifact",), {"assetId": "1", "result": "optimized"}),
    ("verdict 状态用 skipped", ("verdict",),
     {"assetId": "1", "result": "ok", "verdict": {"status": "skipped"}}),
    ("confidence 越界", ("verdict",),
     {"assetId": "1", "result": "ok", "verdict": {"status": "pass", "confidence": 2}}),
    ("产物自建目录", ("artifact",),
     {"assetId": "1", "result": "ok", "artifacts": [{"kind": "own_dir"}]}),
    ("evidence 非对象", ("artifact",), {"assetId": "1", "result": "ok", "evidence": []}),
    ("缺 assetId", ("artifact",), {"result": "ok"}),
    ("自造状态 + 越界 confidence", ("verdict",),
     {"assetId": "1", "result": "ok", "verdict": {"status": "maybe", "confidence": 9}}),
)


def codes(problems):
    return sorted({p.split(":", 1)[0].strip() for p in problems})


class TestProtocolParity(unittest.TestCase):
    def test_shared_constants_are_identical(self):
        for name in SHARED_CONSTANTS:
            self.assertTrue(hasattr(sdk, name), "SDK 缺常量 %s" % name)
            self.assertTrue(hasattr(conf, name), "conformance 缺常量 %s" % name)
            self.assertEqual(getattr(sdk, name), getattr(conf, name),
                             "常量 %s 两侧不一致" % name)

    def test_row_validation_matches_conformance(self):
        for label, outputs, row in ROWS:
            got = codes(sdk.validate_result_row(row, outputs))
            want = codes(conf.validate_result_row(row, outputs))
            self.assertEqual(got, want, "%s：SDK=%s conformance=%s" % (label, got, want))

    def test_valid_rows_pass_both_sides(self):
        for label, outputs, row in ROWS:
            if not row.get("verdict") and row.get("result") != "ok":
                continue
            sdk_problems = sdk.validate_result_row(row, outputs)
            if sdk_problems:
                continue
            self.assertEqual(sdk_problems, [],
                             "%s：SDK 不该报违例" % label)

    def test_manifest_outputs_validation_agrees(self):
        base = {
            "id": "com.soujpg.pack.imaging", "name": "图片处理插件", "version": "0.1.0",
            "api_version": sdk.API_VERSION,
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read"], "deterministic": True, "timeout_secs": 60,
        }
        for outputs in (None, ["verdict"], ["artifact"], ["verdict", "artifact"],
                        [], ["analyzer"], ["verdict", "verdict"]):
            manifest = dict(base)
            if outputs is not None:
                manifest["outputs"] = outputs
            sdk_problems = [p for p in sdk.validate_manifest(manifest) if "outputs" in p]
            conf_problems = [p for p in conf.validate_manifest(manifest) if "outputs" in p]
            self.assertEqual(bool(sdk_problems), bool(conf_problems),
                             "outputs=%r：SDK=%s conformance=%s"
                             % (outputs, sdk_problems, conf_problems))

    def test_legacy_kinds_manifest_is_rejected_with_hint(self):
        # v2.0：旧键 kinds → 拒装 + 映射指路（两侧同一句话）
        manifest = {
            "id": "com.soujpg.pack.legacy", "name": "旧包", "version": "1.2.0",
            "api_version": sdk.API_VERSION,
            "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
            "capabilities": ["io.read"], "deterministic": True, "timeout_secs": 60,
            "kinds": ["judge"],
        }
        for validator in (sdk.validate_manifest, conf.validate_manifest):
            problems = validator(manifest)
            self.assertTrue(problems, "kinds manifest 必须被拒")
            self.assertTrue(
                any(sdk.LEGACY_HINT in p for p in problems),
                "必须带映射指路：%s" % problems,
            )


if __name__ == "__main__":
    unittest.main()
