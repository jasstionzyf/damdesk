"""参数 schema 的单测（K07-I）：平铺参数 → thresholds 的映射，以及插件侧二次校验。

宿主按 `resources/params.schema.json` 发来的是**平铺**的生效值（schema v1 没有嵌套对象
类型），插件自己负责把它折成 `ruleset` 认的 `thresholds` —— 这几个用例钉的就是这条映射，
以及「跨键约束只有包自己知道」的那条二次校验（协议 §7：宿主校过，插件也必须自己校）。
"""
from __future__ import annotations

import json
import unittest

import _support

pack = _support.load_plugin()   # 包模块（plugin/__init__.py）

PACK_DIR = _support.PACK_DIR


class SchemaFileTest(unittest.TestCase):
    def schema(self):
        with open(f"{PACK_DIR}/resources/params.schema.json", encoding="utf-8") as fh:
            return json.load(fh)

    def test_manifest_points_at_existing_schema(self):
        with open(f"{PACK_DIR}/manifest.json", encoding="utf-8") as fh:
            manifest = json.load(fh)
        rel = manifest.get("params")
        self.assertEqual(rel, "resources/params.schema.json")
        self.assertIn(rel, manifest["resources"], "schema 也在资源清单里（装机时校验存在）")

    def test_schema_v1_shape(self):
        schema = self.schema()
        self.assertEqual(schema["schemaVersion"], 1)
        props = schema["properties"]
        for key in ("shortSideMin", "whiteMaxDev", "subjectRatioMin",
                    "subjectRatioMax", "attribute", "allowInpaint"):
            self.assertIn(key, props)
            self.assertTrue(props[key]["title"], f"{key} 必须有 title（界面不许出现无名参数）")
        # 宿主保留键不许出现
        for reserved in ("workdir", "job_id"):
            self.assertNotIn(reserved, props)

    def test_flat_keys_are_the_ones_plugin_maps(self):
        """schema 声明的每个数字键都必须在插件的映射表里（否则声明了却不生效）。"""
        schema = self.schema()
        mapped = set(pack._FLAT_THRESHOLD_PARAMS)
        for key in ("shortSideMin", "whiteMaxDev", "subjectRatioMin", "subjectRatioMax"):
            self.assertIn(key, mapped, f"schema 声明了 {key}，插件必须有映射")
            self.assertIn(key, schema["properties"])


class FlatParamsMappingTest(unittest.TestCase):
    def test_flat_param_overrides_threshold(self):
        th = pack._merge_flat_thresholds(None, {"shortSideMin": 2000})
        self.assertEqual(th["R1_size"]["short_side_min"], 2000)

    def test_no_params_keeps_pack_defaults(self):
        self.assertIsNone(pack._merge_flat_thresholds(None, {}))

    def test_flat_wins_over_explicit_thresholds(self):
        explicit = {"R1_size": {"short_side_min": 1200, "extra": 1}}
        th = pack._merge_flat_thresholds(explicit, {"shortSideMin": 2000})
        self.assertEqual(th["R1_size"]["short_side_min"], 2000, "schema 平铺值优先")
        self.assertEqual(th["R1_size"]["extra"], 1, "显式 thresholds 的其它键保留")

    def test_unrelated_keys_do_not_leak_into_thresholds(self):
        th = pack._merge_flat_thresholds(None, {"attribute": True, "allowInpaint": False})
        self.assertIsNone(th, "非阈值参数不该进 thresholds")


class PluginSideRevalidationTest(unittest.TestCase):
    def test_cross_key_violation_is_rejected(self):
        with self.assertRaises(pack.RpcError) as ctx:
            pack._check_params({"subjectRatioMin": 0.9, "subjectRatioMax": 0.5})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("subjectRatioMin", str(ctx.exception))

    def test_valid_range_passes(self):
        pack._check_params({"subjectRatioMin": 0.5, "subjectRatioMax": 0.9})

    def test_wrong_numeric_type_is_rejected(self):
        with self.assertRaises(pack.RpcError) as ctx:
            pack._check_params({"shortSideMin": "1600"})
        self.assertEqual(ctx.exception.code, -32003)


if __name__ == "__main__":
    unittest.main()
