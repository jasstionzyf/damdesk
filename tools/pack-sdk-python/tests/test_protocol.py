"""协议常量与校验：SDK 的投影必须与 conformance 的可执行规格逐字一致。"""
from __future__ import annotations

import importlib.util
import os
import unittest

import _support                                   # noqa: F401 —— 注入 sys.path
import protocol as conf_proto                     # tools/pack-conformance/protocol.py
from dampack import protocol as sdk_proto


class ConstantParityTest(unittest.TestCase):
    """任何一个常量抄错，宿主与 SDK 对同一帧的理解就会分叉。"""

    def test_constants_match_conformance(self):
        conf = conf_proto
        for name in ("API_VERSION", "METHODS", "NOTIFICATIONS", "STATUSES",
                     "VERDICT_FIELDS", "GATEWAY_ENDPOINTS", "KNOWN_CAPABILITIES",
                     "PLUGIN_ERROR_CODES", "ERR_CAPABILITY_MISSING",
                     "ERR_ASSET_UNREADABLE", "ERR_INVALID_PARAMS", "ERR_CANCELED",
                     "ERR_TIMEOUT", "ERR_OTHER", "ERR_METHOD_NOT_FOUND", "ERR_PARSE"):
            with self.subTest(constant=name):
                self.assertEqual(getattr(sdk_proto, name), getattr(conf, name))

    def test_gateway_endpoints_are_fourteen(self):
        # 12 → 13：v1.1 增量 `/op/encode`（N6b 编码组；能力沿用 op.geometric）
        # 13 → 14：v1.1 增量 `/op/watermark`（N6c 叠加组；新能力 op.overlay）
        self.assertEqual(len(sdk_proto.GATEWAY_ENDPOINTS), 14)
        self.assertEqual(conf_proto.GATEWAY_ENDPOINTS, sdk_proto.GATEWAY_ENDPOINTS)


class ManifestValidationTest(unittest.TestCase):
    GOOD = {
        "id": "com.example.demo", "name": "示例", "version": "1.0.0",
        "api_version": "2.0",
        "outputs": ["verdict"],
        "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
        "capabilities": ["io.read", "vl.judge"], "deterministic": True,
        "timeout_secs": 60,
    }

    def test_good_manifest_passes(self):
        self.assertEqual(sdk_proto.validate_manifest(dict(self.GOOD)), [])

    def test_missing_field(self):
        bad = dict(self.GOOD)
        bad.pop("deterministic")
        self.assertIn("缺字段 deterministic", sdk_proto.validate_manifest(bad))

    def test_wrong_api_version(self):
        bad = dict(self.GOOD, api_version="1.1")
        problems = sdk_proto.validate_manifest(bad)
        self.assertTrue(any("api_version" in p for p in problems), problems)

    def test_legacy_kinds_rejected_with_hint(self):
        bad = dict(self.GOOD)
        bad.pop("outputs")
        bad["kinds"] = ["judge"]
        problems = sdk_proto.validate_manifest(bad)
        self.assertTrue(any(sdk_proto.LEGACY_HINT in p for p in problems), problems)

    def test_unknown_capability(self):
        bad = dict(self.GOOD, capabilities=["io.read", "op.time_travel"])
        problems = sdk_proto.validate_manifest(bad)
        self.assertTrue(any("未知能力" in p for p in problems), problems)

    def test_bad_timeout(self):
        for value in (0, 3601, True, "60", 60.5):
            with self.subTest(timeout=value):
                bad = dict(self.GOOD, timeout_secs=value)
                problems = sdk_proto.validate_manifest(bad)
                self.assertTrue(any("timeout_secs" in p for p in problems), problems)

    def test_non_object(self):
        self.assertEqual(sdk_proto.validate_manifest([]), ["manifest 不是 JSON 对象"])


class PackInfoTest(unittest.TestCase):
    def test_pack_info_from_is_valid(self):
        info = sdk_proto.pack_info_from(
            pack_id="com.example.demo", name="示例", version="1.0.0",
            rules=[{"id": "X1"}], capabilities=["io.read"], deterministic=True,
            timeout_secs=60)
        self.assertEqual(sdk_proto.validate_pack_info(info), [])

    def test_pack_info_drift_detected(self):
        info = sdk_proto.pack_info_from(
            pack_id="com.example.demo", name="示例", version="1.0.0",
            rules=[{"id": "X1"}], capabilities=["io.read"], deterministic=True,
            timeout_secs=60)
        manifest = dict(ManifestValidationTest.GOOD, timeout_secs=90)
        problems = sdk_proto.validate_pack_info(info, manifest=manifest)
        self.assertTrue(any("timeout_secs" in p for p in problems), problems)

    def test_empty_rules_rejected(self):
        info = sdk_proto.pack_info_from(
            pack_id="x", name="x", version="1.0.0", rules=[], capabilities=[],
            deterministic=True, timeout_secs=60)
        self.assertTrue(any("rules" in p for p in sdk_proto.validate_pack_info(info)))


if __name__ == "__main__":
    unittest.main()
