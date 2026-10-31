"""单测共用支撑：把插件模块按「包内路径」加载进来，并造路径/工作目录。"""
from __future__ import annotations

import importlib.util
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PACK_DIR = os.path.dirname(HERE)
PLUGIN_DIR = os.path.join(PACK_DIR, "plugin")
REPO_ROOT = os.path.abspath(os.path.join(PACK_DIR, "..", "..", ".."))

for extra in (PLUGIN_DIR, HERE, os.path.join(REPO_ROOT, "tools", "pack-conformance")):
    if extra not in sys.path:
        sys.path.insert(0, extra)


def load_plugin():
    """按文件路径加载 `plugin/__init__.py`（包目录不是 python package，只能按路径载）。"""
    spec = importlib.util.spec_from_file_location(
        "ecommerce_pack_plugin", os.path.join(PLUGIN_DIR, "__init__.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tempdir(prefix="pack-test-"):
    return tempfile.mkdtemp(prefix=prefix)


class StubGateway:
    """可编排的 gw() 替身：按端点返回预置结果，记录调用序列。"""

    def __init__(self, info=None, vl=None, u2net=None, ocr=None, ops=None,
                 errors=None, white=None, bbox=None, white_dev=None, vl_samples=None):
        self.info = info or {"width": 1600, "height": 1600}
        self.vl = vl if vl is not None else {}
        # K07-FIX-H 翻转保护：按采样序号给不同答案（键=prompt 片段，值=答案列表）
        self.vl_samples = vl_samples or {}
        self.u2net = u2net if u2net is not None else {"fg_ratio": 0.62}
        self.ocr = ocr if ocr is not None else {"lines": []}
        self.ops = ops or {}
        self.errors = errors or {}
        # 白边统计（协议 1.1）：缺省「纯白背景」；测试可覆盖成偏白/带色偏
        self.white = white if white is not None else {"mean_dev": 2.0, "worst_dev": 2.0,
                                                     "band": 4, "edge_pixels": 100}
        # 主体框（协议 1.1）：缺省「无窗」→ 几何退回 bbox/居中档
        self.bbox = bbox if bbox is not None else {}
        # K07-FIX-D：给了就当宿主能算边缘白度（`/io/read` 带 `measure:["white"]` 时回填）
        self.white_dev = white_dev
        self.calls = []

    def __call__(self, path, payload):
        self.calls.append((path, dict(payload or {})))
        if path in self.errors:
            raise self.errors[path]
        if path == "/io/read":
            out = dict(self.info, path=payload.get("path"))
            if self.white_dev is not None and "white" in (payload.get("measure") or []):
                out["white_dev"] = float(self.white_dev)
                out["white_band"] = 4
            return out
        if path == "/detect/u2net":
            return dict(self.u2net, path=payload.get("path"))
        if path == "/detect/bbox":
            base = {"fg_ratio": self.u2net.get("fg_ratio"), "bbox": None,
                    "dominant_bbox": None, "window": None,
                    "width": self.info.get("width"), "height": self.info.get("height")}
            base.update(self.bbox)
            return dict(base, path=payload.get("path"))
        if path == "/detect/white_stats":
            return dict(self.white, path=payload.get("path"))
        if path == "/detect/ocr":
            return dict(self.ocr, path=payload.get("path"))
        if path == "/vl/judge":
            return self._answer(payload)
        if path == "/report/emit":
            return {"ok": True}
        if path in self.ops:
            return self.ops[path]
        # 几何操作默认回「同尺寸产物」
        return {"path": payload.get("out") or payload.get("path"),
                "width": self.info.get("width"), "height": self.info.get("height")}

    def _answer(self, payload):
        prompt = payload.get("prompt") or ""
        samples = int(payload.get("samples") or 1)
        raws = []
        for i in range(max(1, samples)):
            reply = None
            for key, scripted in self.vl_samples.items():
                if key in prompt:
                    seq = scripted if isinstance(scripted, (list, tuple)) else [scripted]
                    reply = seq[i % len(seq)]
                    break
            if reply is None:
                reply = self._static(prompt)
            raws.append(reply)
        out = dict(raws[0])
        if samples > 1:
            out["raws"] = raws
        return out

    def _static(self, prompt):
        for key, value in self.vl.items():
            if key in prompt:
                return value
        return {"undetermined": False, "raw": '{"status":"pass"}',
                "rules": [{"status": "pass", "label": "vl", "score": 0.9}]}

    def paths(self):
        return [p for p, _ in self.calls]

    def count(self, path):
        return sum(1 for p in self.paths() if p == path)
