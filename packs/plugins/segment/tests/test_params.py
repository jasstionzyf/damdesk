"""参数 schema：键集合与宿主 `params.rs` 的白名单对齐（少写键 = 静默不生效）。"""
from __future__ import annotations

import json
import os
import unittest

import _support

SCHEMA_PATH = os.path.join(_support.PACK_DIR, "resources", "params.schema.json")

TOP_KEYS = {"schemaVersion", "groups", "properties", "required"}
GROUP_KEYS = {"key", "title", "collapsed"}
PROP_KEYS = {"type", "title", "default", "min", "max", "step", "options", "unit",
             "accept", "group", "advanced", "when", "irreversible", "affectsBody"}
TYPES = {"integer", "number", "boolean", "enum", "string", "color", "asset_ref", "range"}
RESERVED = {"workdir", "job_id"}


class ParamsSchemaTest(unittest.TestCase):
    def setUp(self):
        with open(SCHEMA_PATH, encoding="utf-8") as fh:
            self.schema = json.load(fh)

    def test_no_unknown_keys(self):
        self.assertEqual(set(self.schema) - TOP_KEYS, set(), "顶层出现未知键 = 装机期被拒")
        for group in self.schema.get("groups", []):
            self.assertEqual(set(group) - GROUP_KEYS, set(), group)
        for key, prop in self.schema["properties"].items():
            self.assertEqual(set(prop) - PROP_KEYS, set(), key)
            self.assertIn(prop["type"], TYPES, key)
            self.assertTrue(str(prop.get("title") or "").strip(), "参数必须有 title")

    def test_reserved_keys_not_declared(self):
        self.assertEqual(set(self.schema["properties"]) & RESERVED, set())
        self.assertEqual(set(self.schema.get("required") or []) & RESERVED, set())

    def test_enum_options_and_defaults(self):
        props = self.schema["properties"]
        self.assertEqual(props["bgMode"]["options"], ["white", "transparent", "color"])
        self.assertIn(props["bgMode"]["default"], props["bgMode"]["options"])
        self.assertEqual(props["feather"]["min"], 0)
        self.assertEqual(props["feather"]["max"], 8)
        self.assertEqual(props["edgeThreshold"]["advanced"], True)
        self.assertEqual(props["color"]["when"], {"bgMode": ["color"]})

    def test_defaults_match_plugin(self):
        import importlib
        plugin = _support.load_plugin()
        for key, value in plugin.DEFAULTS.items():
            if key == "color":
                self.assertEqual(self.schema["properties"]["color"]["default"], value.lower())
            else:
                self.assertEqual(self.schema["properties"][key]["default"], value, key)

    def test_declared_groups_exist(self):
        groups = {g["key"] for g in self.schema.get("groups", [])}
        for key, prop in self.schema["properties"].items():
            if "group" in prop:
                self.assertIn(prop["group"], groups, key)


if __name__ == "__main__":
    unittest.main()
