"""单测共用支撑：按「包内路径」加载插件模块 + 一个真做几何的假网关。

为什么假网关也要真裁剪/真缩放/真 PNG：本包的行为一半取决于「宿主回了什么」
（`op` 是不是 realesr、裁剪框对不对、产物尺寸）。只回标量的桩测不到真实失败面
（模型档静默退回重采样、分块拼接错位、产物尺寸差 1px 等）。
"""
from __future__ import annotations

import importlib.util
import math
import os
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PACK_DIR = os.path.dirname(HERE)
PLUGIN_DIR = os.path.join(PACK_DIR, "plugin")
REPO_ROOT = os.path.abspath(os.path.join(PACK_DIR, "..", "..", ".."))

for extra in (PLUGIN_DIR, HERE):
    if extra not in sys.path:
        sys.path.insert(0, extra)

import pngio  # noqa: E402


def load_plugin():
    spec = importlib.util.spec_from_file_location(
        "upscale_pack_plugin", os.path.join(PLUGIN_DIR, "__init__.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tempdir(prefix="upscale-test-"):
    return tempfile.mkdtemp(prefix=prefix)


def gradient_png(path, width, height):
    """可判别的渐变图（不是纯色：分块/缩放错位能被 PSNR 抓到）。"""
    raw = bytearray(width * height * 3)
    for y in range(height):
        for x in range(width):
            i = (y * width + x) * 3
            raw[i] = (x * 255) // max(1, width - 1)
            raw[i + 1] = (y * 255) // max(1, height - 1)
            raw[i + 2] = ((x ^ y) * 7) % 256
    pngio.write_png(path, width, height, 3, raw)
    return path


def psnr(a_path, b_path):
    aw, ah, ac, araw = pngio.decode_path(a_path)
    bw, bh, bc, braw = pngio.decode_path(b_path)
    w, h = min(aw, bw), min(ah, bh)
    sq = 0
    n = 0
    for y in range(h):
        ra, rb = y * aw * ac, y * bw * bc
        for i in range(w * ac):
            d = araw[ra + i] - braw[rb + i]
            sq += d * d
            n += 1
    return 99.0 if sq == 0 else 10 * math.log10(255.0 * 255.0 / (sq / n))


class StubGateway:
    """`gw()` 替身：真裁剪 + 真最近邻缩放 + 真 PNG 写盘，另可编排失败。

    可编排：
        - `model=True/False`：`/op/upscale scale=4` 是否真的走模型（决定 `op` 应答）；
        - `fail_with{endpoint: code}`：让某个端点回协议错误（-32001/-32003…）；
        - `delay`：每次 `/op/upscale` 睡多久（并发闸门测试用）；
        - `image_size`：`/io/read` 回什么尺寸（可与真实文件不同，用来演练护栏）。
    """

    def __init__(self, model=True, image_size=None, delay=0.0):
        self.model = model
        self.image_size = image_size
        self.delay = delay
        self.calls = []
        self.upscale_calls = []
        self.fail_with = {}
        self.error_factory = None      # install_stub() 换成插件的 RpcError
        self.lock_wait = []            # 每次 /op/upscale 的时间区间（并发断言用）

    def _err(self, code, message=None):
        factory = self.error_factory
        if factory is None:
            raise RuntimeError("stub 未安装：%s" % (code,))
        raise factory(code, message or "stub gateway 错误 %s" % (code,))

    def call(self, path, payload):
        """插件 `gw()` 的调用面（与 `dampack.gateway.Gateway.call` 同签名）。"""
        return self(path, payload)

    def __call__(self, path, payload):
        self.calls.append((path, dict(payload or {})))
        if path in self.fail_with:
            self._err(self.fail_with[path])
        if path == "/io/read":
            if self.image_size:
                return {"path": payload["path"], "width": self.image_size[0],
                        "height": self.image_size[1]}
            w, h, _c = pngio.detect_size(payload["path"])
            return {"path": payload["path"], "width": w, "height": h}
        if path == "/op/crop":
            return self._crop(payload)
        if path == "/op/upscale":
            return self._upscale(payload)
        if path == "/report/emit":
            return {"ok": True}
        self._err(-32000, "stub 不认端点 %s" % (path,))

    def _crop(self, payload):
        src, box, out = payload["path"], payload["box"], payload["out"]
        x1, y1, x2, y2 = [int(v) for v in box]
        w, h, channels, raw = pngio.decode_path(src)
        if not (0 <= x1 < x2 <= w and 0 <= y1 < y2 <= h):
            self._err(-32003, "stub 裁剪越界 %r（源 %dx%d）" % (box, w, h))
        cw, ch = x2 - x1, y2 - y1
        out_raw = bytearray(cw * ch * channels)
        for y in range(ch):
            start = ((y1 + y) * w + x1) * channels
            out_raw[y * cw * channels:(y + 1) * cw * channels] = raw[start:start + cw * channels]
        pngio.write_png(out, cw, ch, channels, out_raw)
        return {"path": out, "width": cw, "height": ch, "op": "crop"}

    def _upscale(self, payload):
        src, out = payload["path"], payload["out"]
        scale = float(payload["scale"])
        w, h, channels, raw = pngio.decode_path(src)
        ow, oh = max(1, round(w * scale)), max(1, round(h * scale))
        started = time.time()
        if self.delay:
            time.sleep(self.delay)
        # 真做最近邻采样（分块拼接/尺寸断言需要真像素，不是标量）
        out_raw = bytearray(ow * oh * channels)
        for y in range(oh):
            sy = min(h - 1, int(y / scale))
            for x in range(ow):
                sx = min(w - 1, int(x / scale))
                a = (sy * w + sx) * channels
                b = (y * ow + x) * channels
                out_raw[b:b + channels] = raw[a:a + channels]
        pngio.write_png(out, ow, oh, channels, out_raw)
        model_pass = self.model and abs(scale - 4.0) < 1e-9
        op = "upscale.realesr-x4v3" if model_pass else "upscale.resample"
        self.upscale_calls.append({"scale": scale, "op": op, "src": src, "out": out,
                                   "interval": [started, time.time()]})
        return {"path": out, "width": ow, "height": oh, "op": op, "scale": scale,
                "passes": 1, "steps": [{"from_long": w, "to_long": ow, "mode": op}],
                "scale_max": 8.0}


def install_stub(plugin, stub):
    """装桩：端点失败一律抛**真的** `GatewayError`（与宿主网关同一类型/错误码），
    这样插件里「网关失败 → failed 行」的分支是被真类型驱动的，不是被桩的自定义异常驱动。"""
    plugin._STUB_GATEWAY = stub
    stub.error_factory = plugin.gateway_mod.GatewayError
    return stub


def uninstall(plugin):
    plugin._STUB_GATEWAY = None
