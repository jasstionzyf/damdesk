#!/usr/bin/env python3
"""imaging 插件装配单测（N6d）：manifest / 汇总 schema / 六动作分派 / 协议壳 / 取消。

跑法：
    python3 packs/plugins/imaging/tests/test_assembly.py
    cd packs/plugins/imaging/tests && python3 -m unittest -v test_assembly

本文件只验**装配**（接线对不对），算法正确性归各组的 test_geometry/test_codec/test_overlay。
四条硬口径（协议面，宿主据此拒收，不许靠「看起来能跑」）：
    1. `kinds=["process"]` → 结果行**不得**有 `verdict`（`row_verdict_not_allowed`）；
    2. `pack.info` 与 `manifest.json` 的 id/version/capabilities/deterministic/timeout_secs 逐字一致；
    3. `manifest.params` 指向的 schema 能被宿主解析器接受的**形状**（键/类型/`when`/分组/保留键）；
    4. 能力清单 = 六个动作**实际用到**的端点能力（多声明=越权，少声明=真机 403）。
"""
from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
PLUGIN = os.path.join(PACK, "plugin")
REPO = os.path.dirname(os.path.dirname(os.path.dirname(PACK)))
CONFORMANCE = os.path.join(REPO, "tools", "pack-conformance")
for p in (PACK, PLUGIN, CONFORMANCE):
    if p not in sys.path:
        sys.path.insert(0, p)

import codec                                     # noqa: E402
import geometry                                  # noqa: E402
import overlay                                   # noqa: E402
import protocol as proto                          # noqa: E402  (宿主字段表的可执行版本)

# 插件入口文件名是 `__init__.py`（包内 plugin 目录不是 Python 包，按文件路径导入）
import importlib.util                             # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "imaging_plugin", os.path.join(PLUGIN, "__init__.py"))
api = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(api)

MANIFEST_PATH = os.path.join(PACK, "manifest.json")
SCHEMA_PATH = os.path.join(PACK, "resources", "params.schema.json")

# 端点 → 能力（协议 §6 白名单的机器可读映射，只列本包用到的）
ENDPOINT_CAPABILITY = {
    "io_read": "io.read",
    "detect_bbox": "detect.u2net",
    "crop": "op.geometric",
    "letterbox": "op.geometric",
    "upscale": "op.geometric",
    "encode": "op.geometric",
    "watermark": "op.overlay",
}

PARAM_TYPES = {"integer", "number", "boolean", "enum", "string", "color",
               "asset_ref", "range"}


def _load(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


class StubGateway:
    """桩网关：六个动作需要的 7 个方法全在，尺寸按宿主同一口径算，产物落真文件。

    只做「接线可跑」的语义：不做真解码、不出真像素（像素级断言在各组语料用例里）。
    """

    def __init__(self, size=(2000, 1500), subject=(400, 200, 1600, 1200), in_bytes=500_000):
        self.size = size
        self.subject = subject
        self.in_bytes = in_bytes
        self.calls = []

    # --- /io/read ---
    def io_read(self, path):
        self.calls.append("io_read")
        return {"path": path, "width": self.size[0], "height": self.size[1],
                "size": self.in_bytes, "sha256": "a" * 64, "mime": "image/jpeg"}

    # --- /detect/bbox ---
    def detect_bbox(self, path, target_ratio=None, min_instance_ratio=None):
        self.calls.append("detect_bbox")
        return {"path": path, "width": self.size[0], "height": self.size[1],
                "fg_ratio": 0.4, "bbox": list(self.subject),
                "dominant_bbox": list(self.subject),
                "instances": [{"bbox": list(self.subject), "area_ratio": 0.4}]}

    def _touch(self, out, w, h, extra=None):
        os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
        with open(out, "wb") as fh:
            fh.write(b"stub")
        reply = {"path": out, "width": int(w), "height": int(h)}
        reply.update(extra or {})
        return reply

    def crop(self, path, box, out):
        self.calls.append("crop")
        return self._touch(out, box[2] - box[0], box[3] - box[1], {"op": "crop"})

    def letterbox(self, path, target_ratio, out, background=None):
        self.calls.append("letterbox")
        w, h = self.size
        if w / float(h) > target_ratio:
            return self._touch(out, w, int(round(w / float(target_ratio))),
                               {"op": "letterbox", "target_ratio": target_ratio})
        return self._touch(out, int(round(h * float(target_ratio))), h,
                           {"op": "letterbox", "target_ratio": target_ratio})

    def upscale(self, path, scale, out):
        self.calls.append("upscale")
        w = max(1, int(round(self.size[0] * float(scale))))
        h = max(1, int(round(self.size[1] * float(scale))))
        return self._touch(out, w, h, {"op": "upscale.resample"})

    def encode(self, path, out, quality=None, background=None, auto_orient=None, fmt=None):
        self.calls.append("encode")
        q = int(quality or 85)
        # 体积随质量单调上升（compress 的二分会用到；这里只保证「接线与取值语义」正确）
        out_bytes = max(1024, int(self.in_bytes * (q / 100.0) ** 1.5))
        return self._touch(out, self.size[0], self.size[1],
                           {"op": "encode", "format": (fmt or os.path.splitext(out)[1].lstrip(".")),
                            "bytes": out_bytes, "bytes_in": self.in_bytes,
                            # /op/encode 真实口径（gateway.rs）：源无透明像素 → absent；
                            # 有 alpha 且目标容器支持 → kept；不支持 → flattened
                            "source_alpha": False, "alpha": "absent",
                            "quality_applied": True, "quality": q,
                            "orientation": 1})

    def watermark(self, path, text, out, position=None, opacity=None,
                  font_size=None, margin=None, color=None):
        self.calls.append("watermark")
        return self._touch(out, self.size[0], self.size[1],
                           {"op": "watermark", "coverage": 0.02})


class _Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="imaging-asm-")
        self.asset_file = os.path.join(self.tmp, "IMG_0001.jpg")
        with open(self.asset_file, "wb") as fh:
            fh.write(b"source-bytes")           # rename 要真文件（读 mtime / basename）
        self._orig_factory = api.GATEWAY_FACTORY
        self.gw = StubGateway()
        api.GATEWAY_FACTORY = lambda **_kw: self.gw

    def tearDown(self):
        api.GATEWAY_FACTORY = self._orig_factory
        api._CANCEL.clear()
        api._JOB_SEEN.clear()
        api._REPORTS.clear()
        overlay.RENAME_REGISTRY.reset()

    def asset(self):
        return {"id": "asset-1", "path": self.asset_file, "mime": "image/jpeg",
                "width": 2000, "height": 1500}

    def options(self, action, **extra):
        opts = {"action": action, "workdir": self.tmp}
        opts.update(extra)
        return opts

    def judge(self, action, job_id="job-1", **extra):
        return api.judge({"job_id": job_id, "asset": self.asset(),
                          "rules": None, "options": self.options(action, **extra)})


class ManifestTest(_Base):
    def test_manifest_is_protocol_legal(self):
        manifest = _load(MANIFEST_PATH)
        self.assertEqual(proto.validate_manifest(manifest), [])
        self.assertEqual(manifest["kinds"], ["process"], "处理类包必须显式声明 kinds=[process]")
        self.assertEqual(manifest["api_version"], "1.1")
        self.assertTrue(manifest["deterministic"])
        self.assertEqual(manifest["params"], "resources/params.schema.json")
        for rel in manifest["resources"]:
            self.assertTrue(os.path.isfile(os.path.join(PACK, rel)), rel)
        entry = manifest["entrypoint"]
        for arg in entry.get("args") or []:
            self.assertTrue(os.path.isfile(os.path.join(PACK, arg)), arg)

    def test_pack_info_matches_manifest(self):
        manifest = _load(MANIFEST_PATH)
        info = api.pack_info({})
        for field in ("id", "version", "api_version", "deterministic", "timeout_secs"):
            self.assertEqual(info[field], manifest[field], field)
        self.assertEqual(set(info["capabilities"]), set(manifest["capabilities"]))
        self.assertEqual(info["kinds"], manifest["kinds"])
        self.assertTrue(info["rules"], "pack.info.rules 必须非空（conformance 03）")

    def test_actions_cover_the_six_groups(self):
        info = api.pack_info({})
        self.assertEqual([a["id"] for a in info["actions"]],
                         ["resize", "crop", "convert", "compress", "watermark", "rename"])
        self.assertEqual(set(api.ACTION_NAMES), set(geometry.ACTIONS) | set(codec.ACTIONS)
                         | set(overlay.ACTIONS))
        for action in info["actions"]:
            self.assertEqual(action["kinds"], ["process"])
            self.assertIn(action["group"], {"geometry", "codec", "overlay"})
            self.assertTrue(action["summary"])

    def test_rules_mirror_actions(self):
        """处理类包没有判定规则：`rules` 如实复用动作清单，不编造规则名。"""
        info = api.pack_info({})
        self.assertEqual([r["id"] for r in info["rules"]], api.ACTION_NAMES)
        self.assertTrue(all(r["deterministic"] for r in info["rules"]))


class ParamsSchemaTest(_Base):
    def setUp(self):
        super().setUp()
        self.schema = _load(SCHEMA_PATH)

    def test_shape_is_host_parseable(self):
        """按 `crates/pack-host/src/params.rs` 的校验规则手检（宿主拒装条件逐条对齐）。"""
        self.assertEqual(self.schema["schemaVersion"], 1)
        groups = [g["key"] for g in self.schema["groups"]]
        self.assertEqual(len(groups), len(set(groups)), "分组键不得重复")
        self.assertEqual(groups[0], "basic", "动作选择必须是最前一组（界面第一屏）")
        for group in self.schema["groups"]:
            self.assertTrue(group["key"].strip())
            self.assertTrue(group["title"].strip(), "界面不许出现无名分区")
        props = self.schema["properties"]
        self.assertTrue(props)
        for key, spec in props.items():
            self.assertNotIn(key, ("workdir", "job_id"), "宿主保留键不许插件声明")
            self.assertIn(spec["type"], PARAM_TYPES, key)
            self.assertTrue(str(spec["title"]).strip(), key)
            if spec["type"] == "enum":
                self.assertTrue(spec.get("options"), "%s：enum 必须给 options" % key)
                if "default" in spec:
                    self.assertIn(spec["default"], spec["options"], key)
            if spec.get("group"):
                self.assertIn(spec["group"], groups, "%s：分组 %s 未声明" % (key, spec["group"]))
            for dep in (spec.get("when") or {}):
                self.assertIn(dep, props, "%s：when 引用了未声明参数 %s" % (key, dep))
                self.assertNotEqual(dep, key, "%s：when 不许引用自己" % key)
        for key in self.schema.get("required") or []:
            self.assertIn(key, props, "required 里的 %s 不在 properties" % key)

    def test_action_enum_matches_registry(self):
        action = self.schema["properties"]["action"]
        self.assertEqual(action["type"], "enum")
        self.assertEqual(action["options"], api.ACTION_NAMES)
        self.assertEqual(action["default"], "resize")

    def test_all_group_fragments_are_merged(self):
        """汇总文件必须逐字覆盖三组片段里的每个参数（装配只搬运，不改写）。

        漏一个 = 界面上没控件、插件收不到值；改一个 = 同一参数在 schema/运行时两处语义不一致。
        """
        for module in (geometry, codec, overlay):
            for key, spec in module.SCHEMA["properties"].items():
                self.assertIn(key, self.schema["properties"], key)
                self.assertEqual(self.schema["properties"][key], spec, key)
            for group in module.SCHEMA["groups"]:
                self.assertIn(group, self.schema["groups"])

    def test_no_cross_group_when_dependency(self):
        """卡面硬边界：不引入跨组 `when` 依赖（避免参数墙与不可解释的隐藏项）。

        具体两条：① 任何参数的 `when` 都不许引用 `action`；② `when` 引用的键必须与自己在同一组。
        """
        props = self.schema["properties"]
        for key, spec in props.items():
            when = spec.get("when") or {}
            self.assertNotIn("action", when, "%s 对 action 加了跨组门控" % key)
            for dep in when:
                self.assertEqual(props[dep].get("group"), spec.get("group"),
                                 "%s 的 when 引用了别的组的参数 %s" % (key, dep))

    def test_schema_not_drifted_from_modules(self):
        result = subprocess.run(
            [sys.executable, os.path.join(HERE, "dump_pack_schema.py"), "--check"],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class DispatchTest(_Base):
    """六个动作逐个走一遍协议壳 → 行契约（协议 §4.4）逐行验。"""

    def _row(self, action, **extra):
        reply = self.judge(action, **extra)
        rows, problems = proto.rows_from_reply(reply, api.PACK_INFO["kinds"])
        self.assertEqual(problems, [], "%s 结果行违例：%s" % (action, problems))
        self.assertEqual(len(rows), 1)
        return rows[0]

    def test_resize(self):
        row = self._row("resize", resizeMode="long_edge", resizeLongEdge=1000)
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "resize")
        self.assertEqual(row["fields"]["out"], [1000, 750])
        self.assertTrue(row["fields"]["outPath"].startswith(self.tmp))
        self.assertIn("upscale", self.gw.calls)

    def test_crop(self):
        row = self._row("crop", cropAspect="1:1", cropAnchor="center")
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "crop")
        self.assertIn("detect_bbox", self.gw.calls)
        self.assertIn("crop", self.gw.calls)
        self.assertTrue(row["evidence"]["subjectCovered"])

    def test_convert(self):
        row = self._row("convert", convertFormat="webp", convertQuality=80)
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "convert")
        self.assertTrue(row["fields"]["outPath"].endswith(".webp"))

    def test_compress_quality_mode(self):
        row = self._row("compress", compressMode="quality", compressQuality=70)
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "compress")
        self.assertIn("encode", self.gw.calls)

    def test_watermark(self):
        row = self._row("watermark", watermarkText="DAMDesk", watermarkAnchor="br")
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "watermark")
        self.assertEqual(row["fields"]["anchor"], "bottom_right")
        self.assertIn("watermark", self.gw.calls)

    def test_rename(self):
        row = self._row("rename", renameTemplate="{name}_{width}x{height}")
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row["fields"]["action"], "rename")
        self.assertEqual(row["fields"]["assetName"], "IMG_0001_2000x1500.jpg")
        self.assertNotIn("outPath", row["fields"], "rename 不出产物（只回名字）")

    def test_no_row_carries_verdict(self):
        for action in api.ACTION_NAMES:
            reply = self.judge(action, job_id="job-noverdict")
            for row in reply["rows"]:
                self.assertNotIn("verdict", row, "%s 回了 verdict（处理类被拒收）" % action)
                self.assertIn(row["result"], ("ok", "failed", "skipped"))

    def test_rows_have_action_and_group_in_fields(self):
        for action in api.ACTION_NAMES:
            row = self.judge(action, job_id="job-fields")["rows"][0]
            self.assertEqual(row["fields"]["action"], action)
            self.assertIn(row["fields"]["group"], ("geometry", "codec", "overlay"))
            self.assertIsInstance(row["fields"]["durationMs"], int)

    def test_ok_rows_register_export_file_artifact(self):
        """产物去处（协议 §4.4）：五个出文件的动作都必须登记一条 `export_file{path,sha256}`。

        N14 门禁查出来的真缺陷：装配层原来不登记 `artifacts`，宿主下游「只读 artifacts」，
        于是「动作成功、产物却在宿主侧凭空消失」——**跑成功了但拿不到工作版本**。
        `rename` 不出新文件 → 显式空数组，不许编 versionId、也不许冒认产物。
        """
        cases = {
            "resize": {},
            "crop": {},
            "convert": {"convertFormat": "webp"},
            "compress": {"compressMode": "quality", "compressQuality": 70},
            "watermark": {"watermarkText": "DAMDesk"},
        }
        for action, extra in cases.items():
            row = self._row(action, job_id="job-artifact", **extra)
            self.assertEqual(row["result"], "ok", "%s 必须成功" % action)
            artifacts = row.get("artifacts")
            self.assertIsInstance(artifacts, list, "%s 缺 artifacts" % action)
            self.assertEqual(len(artifacts), 1, "%s 产物只能一条（不许自建目录树）" % action)
            art = artifacts[0]
            self.assertEqual(art["kind"], "export_file")
            self.assertEqual(art["path"], row["fields"]["outPath"])
            self.assertEqual(art["sha256"], row["fields"]["sha256"])
            self.assertTrue(os.path.isfile(art["path"]), "%s 产物必须真落盘" % action)

    def test_rename_declares_empty_artifacts(self):
        row = self._row("rename", job_id="job-artifact-rename")
        self.assertEqual(row["result"], "ok")
        self.assertEqual(row.get("artifacts"), [],
                         "rename 只改资产名、不出字节 → artifacts 必须是空数组")


class CapabilityTest(_Base):
    def test_declared_capabilities_equal_used_capabilities(self):
        used = set()
        for action in api.ACTION_NAMES:
            self.gw.calls.clear()
            self.judge(action, job_id="job-cap-%s" % action)
            for name in self.gw.calls:
                used.add(ENDPOINT_CAPABILITY[name])
        self.assertEqual(used, set(api.CAPABILITIES))
        self.assertEqual(set(api.CAPABILITIES), set(_load(MANIFEST_PATH)["capabilities"]))


class ErrorTest(_Base):
    def test_missing_action(self):
        with self.assertRaises(api.RpcError) as ctx:
            api.judge({"job_id": "j", "asset": self.asset(), "options": {"workdir": self.tmp}})
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("options.action", ctx.exception.message)

    def test_unknown_action(self):
        with self.assertRaises(api.RpcError) as ctx:
            self.judge("sharpen")
        self.assertEqual(ctx.exception.code, -32003)
        self.assertIn("sharpen", ctx.exception.message)

    def test_missing_asset_path(self):
        with self.assertRaises(api.RpcError) as ctx:
            api.judge({"job_id": "j", "asset": {}, "options": self.options("resize")})
        self.assertEqual(ctx.exception.code, -32003)

    def test_repair_is_honestly_unimplemented(self):
        frame = api._handle_line(json.dumps(
            {"jsonrpc": "2.0", "id": 9, "method": "repair", "params": {}}))
        self.assertEqual(frame["error"]["code"], -32601)
        self.assertIn("process", frame["error"]["message"])

    def test_unknown_method(self):
        frame = api._handle_line(json.dumps(
            {"jsonrpc": "2.0", "id": 10, "method": "fix_everything", "params": {}}))
        self.assertEqual(frame["error"]["code"], -32601)

    def test_bad_json(self):
        frame = api._handle_line("{not json")
        self.assertEqual(frame["error"]["code"], -32700)

    def test_gateway_error_becomes_failed_row_not_crash(self):
        from gw import GatewayError
        calls = {"n": 0}

        def boom(*_args, **_kwargs):
            raise GatewayError(-32001, "capability_missing", status=403,
                              error="capability_missing", capability="op.overlay")

        self.gw.watermark = boom                      # type: ignore[assignment]
        reply = self.judge("watermark")
        row = reply["rows"][0]
        self.assertEqual(row["result"], "failed")
        self.assertEqual(row["error"]["code"], "-32001")
        self.assertTrue(calls is not None)


class CancelTest(_Base):
    def test_cancel_then_judge_is_32004(self):
        frame = api._handle_line(json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": "cancel", "params": {"job_id": "job-c"}}))
        self.assertTrue(frame["result"]["canceled"])
        with self.assertRaises(api.RpcError) as ctx:
            self.judge("resize", job_id="job-c")
        self.assertEqual(ctx.exception.code, -32004)

    def test_cancel_notification_frame_has_no_reply(self):
        """宿主发的是**通知**（无 id）：置位后不回帧，否则宿主会当成未请求的响应。"""
        self.assertIsNone(api._handle_line(json.dumps(
            {"jsonrpc": "2.0", "method": "cancel", "params": {"job_id": "job-c2"}})))


class RenameRegistryTest(_Base):
    def test_duplicate_names_get_suffix_within_job(self):
        first = self.judge("rename", job_id="job-r", renameTemplate="{name}")["rows"][0]
        second = self.judge("rename", job_id="job-r", renameTemplate="{name}")["rows"][0]
        self.assertEqual(first["fields"]["assetName"], "IMG_0001.jpg")
        self.assertEqual(second["fields"]["assetName"], "IMG_0001-2.jpg")
        self.assertEqual(second["fields"]["collisionIndex"], 2)

    def test_new_job_resets_registry(self):
        self.judge("rename", job_id="job-r1", renameTemplate="{name}")
        fresh = self.judge("rename", job_id="job-r2", renameTemplate="{name}")["rows"][0]
        self.assertEqual(fresh["fields"]["assetName"], "IMG_0001.jpg",
                         "新 job 必须重新发号（否则尾号跨 job 累加）")


class ProtocolShellTest(_Base):
    """协议壳：stdio 行 → 帧，stdout 只有协议行（conformance 02 的本地等价物）。"""

    def _run(self, frames):
        stdin = io.StringIO("\n".join(json.dumps(f, ensure_ascii=False) for f in frames) + "\n")
        stdout = io.StringIO()
        api.serve(stdin=stdin, stdout=stdout)
        return [json.loads(line) for line in stdout.getvalue().splitlines() if line.strip()]

    def test_pack_info_health_judge_report(self):
        # 每帧一个线程（长任务不能挡 cancel）：同一会话里 judge 与 report 可能并发，
        # 所以按「先跑完 judge，再问 report」两批断言 —— 这正是宿主的实际调用序。
        first = self._run([
            {"jsonrpc": "2.0", "id": 1, "method": "pack.info", "params": {}},
            {"jsonrpc": "2.0", "id": 2, "method": "health", "params": {}},
            {"jsonrpc": "2.0", "id": 3, "method": "judge",
             "params": {"job_id": "job-shell", "asset": self.asset(),
                        "options": self.options("resize", resizeLongEdge=800)}},
        ])
        first.sort(key=lambda f: f["id"])
        self.assertEqual([f["id"] for f in first], [1, 2, 3])
        self.assertEqual(first[1]["result"]["status"], "ok")
        self.assertEqual(first[2]["result"]["rows"][0]["fields"]["action"], "resize")
        second = self._run([{"jsonrpc": "2.0", "id": 4, "method": "report",
                             "params": {"job_id": "job-shell"}}])
        report = second[0]["result"]
        self.assertEqual(report["count"], 1)
        self.assertEqual(report["entries"][0]["action"], "resize")

    def test_stdout_only_carries_protocol_lines(self):
        out = self._run([{"jsonrpc": "2.0", "id": 1, "method": "health", "params": {}}])
        self.assertEqual(len(out), 1)
        self.assertEqual(set(out[0]), {"jsonrpc", "id", "result"})


if __name__ == "__main__":
    unittest.main(verbosity=2)
