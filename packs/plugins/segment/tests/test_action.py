"""动作面（N6e）：`pack.info.actions[]` 与 `options.action` 分派。

真机卡 task 的根因就是这里：宿主动作链按 `pack.info.actions[].id` 校验入参，
而本包**一个动作都没声明** → `validate_actions` 把 `white_bg_fill` 判成「未知动作」，
`POST /api/jobs` 直接 400（方案能建、跑不了）。

四条断言（都带反向验证）：
1. `pack.info.actions[].id` = `("white_bg_fill",)`，且与 `PACK_VERSION` / manifest 同版本；
2. 链内调用（带 `action`）真的跑通并出产物；
3. 未知动作 → -32003 且点名可用动作（不静默忽略）；
4. `action` 与 `bgMode` 冲突 → -32003（不静默按其中一个执行）；
5. `action` **不进** `params.schema`（宿主会把 schema 默认值注入每次执行 —— 一旦注入，
   `bgMode=transparent` 的普通规格会被静默改成白底）。
"""
from __future__ import annotations

import json
import os
import unittest

import _support

plugin = _support.load_plugin()

MANIFEST = os.path.join(_support.PACK_DIR, "manifest.json")
SCHEMA = os.path.join(_support.PACK_DIR, "resources", "params.schema.json")
ACTIONS_RES = os.path.join(_support.PACK_DIR, "resources", "actions.json")


def _judge(tmp, path, options):
    opts = dict(options)
    opts.setdefault("workdir", os.path.join(tmp, "out"))
    return plugin.dispatch({"method": "judge",
                            "params": {"job_id": "job-action",
                                       "asset": {"id": "asset-1", "path": path},
                                       "options": opts}})["rows"][0]


class DeclaredActionsTest(unittest.TestCase):
    def test_pack_info_declares_white_bg_fill(self):
        self.assertEqual([a["id"] for a in plugin.PACK_INFO["actions"]],
                         ["white_bg_fill"])
        action = plugin.PACK_INFO["actions"][0]
        self.assertEqual(action["kinds"], ["process"])
        self.assertEqual(action["group"], "basic")
        self.assertTrue(str(action["summary"]).strip(), "动作要有「做什么」的一句人话")

    def test_action_list_lives_in_resources_for_host_derivation(self):
        """动作清单只有一处声明：`resources/actions.json`。

        宿主 `spec_seed` 读同一份文件来派生「官方默认方案」（读 `pack.info` 要拉起子进程，
        而方案清单是 `/api/specs` 这种读接口要用的廉价数据）——两边读的必须是同一份，
        所以这里钉死「pack.info.actions[] == resources/actions.json」。
        """
        with open(ACTIONS_RES, encoding="utf-8") as fh:
            declared = json.load(fh)["actions"]
        self.assertEqual(declared, plugin.PACK_INFO["actions"],
                         "pack.info.actions[] 必须就是 resources/actions.json 的内容")
        self.assertEqual(tuple(a["id"] for a in declared), plugin.ACTION_IDS)
        for action in declared:
            self.assertTrue(str(action.get("name") or "").strip(),
                            "动作要有中文名：宿主用它当默认方案名")
            self.assertEqual(action.get("kinds"), ["process"])

    def test_version_matches_manifest(self):
        with open(MANIFEST, encoding="utf-8") as fh:
            manifest = json.load(fh)
        # 宿主握手（`verify_pack_info`）强制 pack.info 与 manifest 三个字段一致
        self.assertEqual(plugin.PACK_INFO["version"], manifest["version"])
        self.assertEqual(plugin.PACK_INFO["id"], manifest["id"])

    def test_action_not_declared_in_params_schema(self):
        with open(SCHEMA, encoding="utf-8") as fh:
            schema = json.load(fh)
        self.assertNotIn("action", schema["properties"],
                         "schema 里声明 action 会被宿主当默认值注入每次执行")


class ActionDispatchTest(unittest.TestCase):
    def setUp(self):
        self.tmp, self.gw, self.box, self.src = _support.make_fixture()
        _support.install_stub(plugin, self.gw)
        plugin._REPORTS.clear()

    def test_chain_action_produces_artifact(self):
        row = _judge(self.tmp, self.src, {"action": "white_bg_fill", "bgMode": "white"})
        self.assertEqual(row["result"], "ok")
        self.assertEqual(len(row["artifacts"]), 1)
        self.assertTrue(os.path.isfile(row["artifacts"][0]["path"]))

    def test_unknown_action_is_rejected_by_name(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            _judge(self.tmp, self.src, {"action": "blur"})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("blur", ctx.exception.message)
        self.assertIn("white_bg_fill", ctx.exception.message)

    def test_action_conflicting_with_bg_mode_is_rejected(self):
        with self.assertRaises(plugin.RpcError) as ctx:
            _judge(self.tmp, self.src, {"action": "white_bg_fill", "bgMode": "transparent"})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("冲突", ctx.exception.message)

    def test_transparent_flow_still_works_without_action(self):
        row = _judge(self.tmp, self.src, {"bgMode": "transparent"})
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["artifacts"][0]["kind"], "export_file")


if __name__ == "__main__":
    unittest.main()
