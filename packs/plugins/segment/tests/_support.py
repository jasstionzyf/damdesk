"""单测共用支撑：按「包内路径」加载插件模块 + 一个真做像素的假网关。

为什么假网关也要真裁剪/真 PNG：本包的逻辑一半在「宿主回了什么」上
（掩码 0/255、裁块尺寸、通道数）。桩数据只回标量的话，测不到 90% 的真实失败面
（尺寸不一致、通道不是 RGB、掩码不是灰度…）。
"""
from __future__ import annotations

import importlib.util
import os
import sys
import tempfile

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
        "segment_pack_plugin", os.path.join(PLUGIN_DIR, "__init__.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tempdir(prefix="segment-test-"):
    return tempfile.mkdtemp(prefix=prefix)


def solid_png(path, width, height, rgb):
    raw = bytes(rgb) * (width * height)
    pngio.write_png(path, width, height, 3, raw)
    return path


def subject_png(path, width, height, bg=(200, 120, 60), fg=(30, 90, 220),
                box=None):
    """背景色 + 中央一个纯色「主体」矩形（box=(x1,y1,x2,y2)，缺省居中一半）。"""
    x1, y1, x2, y2 = box or (width // 4, height // 4, width // 4 * 3, height // 4 * 3)
    row_bg = bytes(bg) * width
    raw = bytearray(row_bg * height)
    for y in range(y1, y2):
        start = (y * width + x1) * 3
        raw[start:(y * width + x2) * 3] = bytes(fg) * (x2 - x1)
    pngio.write_png(path, width, height, 3, raw)
    return path, (x1, y1, x2, y2)


def mask_png(path, width, height, box):
    """0/255 灰度掩码（宿主 write_mask_png 的等价物）。"""
    x1, y1, x2, y2 = box
    raw = bytearray(width * height)
    for y in range(y1, y2):
        raw[y * width + x1:y * width + x2] = b"\xff" * (x2 - x1)
    pngio.write_png(path, width, height, 1, raw)
    return path


class StubGateway:
    """可编排的 `gw()` 替身：只实现本包用到的 6 个端点，但**真做裁剪/真算白度**。"""

    def __init__(self, workdir, subject_box, image_size, fg_ratio=None,
                 white_stats_available=True, white_stats_override=None):
        self.workdir = workdir
        self.width, self.height = image_size
        self.box = subject_box
        self.fg_ratio = fg_ratio
        self.calls = []
        self.crop_sizes = []      # 每次 /op/crop 的 (源, box) —— 用来断言「只裁主体块」
        self.fail_with = {}       # path -> 协议错误码
        self.white_stats_available = white_stats_available
        self.white_stats_override = white_stats_override   # 直接给定统计（测「量不到」分支）
        self.rpc_error = RuntimeError   # install_stub() 会换成插件的 RpcError

    def _err(self, code):
        return self.rpc_error(code, "stub gateway 错误 %s" % (code,))

    def _white_stats(self, payload):
        """真按像素算边框带统计（与宿主 classic `white_edge` 同口径：max/mean 通道偏差）。"""
        if not self.white_stats_available:
            raise self._err(-32001)
        if self.white_stats_override is not None:
            row = dict(self.white_stats_override)
            row.setdefault("band", payload.get("band"))
            return row
        w, h, ch, raw = pngio.decode_path(payload["path"])
        band = int(payload.get("band") or 4)
        threshold = float(payload.get("max_dev") or 8.0)
        worst = 0
        total = 0
        n = 0
        for y in range(h):
            for x in range(w):
                if not (x < band or y < band or x + band >= w or y + band >= h):
                    continue
                base = (y * w + x) * ch
                px = raw[base:base + 3]
                dev = max(abs(255 - v) for v in px)
                total += dev
                n += 1
                if dev > worst:
                    worst = dev
        mean = (total / float(n)) if n else 0.0
        return {"path": payload["path"], "width": w, "height": h,
                "engine": "stub-white-edge", "mean_dev": mean, "worst_dev": float(worst),
                "band": band, "edge_pixels": n, "threshold": threshold,
                "pass": mean <= threshold}

    def __call__(self, path, payload):
        self.calls.append((path, dict(payload or {})))
        if path in self.fail_with:
            raise self._err(self.fail_with[path])
        if path == "/io/read":
            return {"path": payload["path"], "width": self.width, "height": self.height}
        if path == "/detect/white_stats":
            return self._white_stats(payload)
        if path == "/op/resize":
            return self._resize(payload)
        if path == "/detect/u2net":
            x1, y1, x2, y2 = self.box
            ratio = self.fg_ratio
            if ratio is None:
                ratio = round(((x2 - x1) * (y2 - y1)) / float(self.width * self.height), 4)
            return {"path": payload["path"], "fg_ratio": ratio,
                    "bbox": [x1, y1, x2, y2], "dominant_bbox": [x1, y1, x2, y2],
                    "instances": [{"box": [x1, y1, x2, y2], "ratio": ratio}],
                    "mask_path": self.mask_path, "width": self.width, "height": self.height}
        if path == "/op/crop":
            src = payload["path"]
            box = payload["box"]
            out = payload["out"]
            os.makedirs(os.path.dirname(out), exist_ok=True)
            w, h, ch, raw = pngio.decode_path(src)
            x1, y1, x2, y2 = [int(v) for v in box]
            if not (0 <= x1 < x2 <= w and 0 <= y1 < y2 <= h):
                raise self._err(-32003)
            cw, chh = x2 - x1, y2 - y1
            out_raw = bytearray(cw * chh * ch)
            for y in range(chh):
                a = ((y1 + y) * w + x1) * ch
                out_raw[y * cw * ch:(y + 1) * cw * ch] = raw[a:a + cw * ch]
            pngio.write_png(out, cw, chh, ch, out_raw)
            self.crop_sizes.append((src, list(box)))
            return {"path": out, "width": cw, "height": chh, "op": "crop"}
        if path == "/report/emit":
            return {"ok": True}
        raise self._err(-32000)

    def _resize(self, payload):
        """`/op/resize` 桩：真做最近邻缩小（只缩不放），并让 stub 的尺寸/主体框随之更新
        ——否则后续 `/detect/u2net` 报的还是原尺寸，测不出「预缩放真的生效」。
        """
        src = payload["path"]
        out = payload["out"]
        cap = int(payload.get("max_side") or 0)
        if cap < 16:
            raise self._err(-32003)
        w, h, ch, raw = pngio.decode_path(src)
        long_side = max(w, h)
        if long_side > cap:
            scale = cap / float(long_side)
            nw, nh = max(1, int(round(w * scale))), max(1, int(round(h * scale)))
        else:
            nw, nh = w, h
        scaled = bytearray(nw * nh * 3)
        for y in range(nh):
            sy = min(h - 1, int(y * h / nh))
            for x in range(nw):
                sx = min(w - 1, int(x * w / nw))
                si = (sy * w + sx) * ch
                di = (y * nw + x) * 3
                scaled[di:di + 3] = raw[si:si + 3]
        os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
        pngio.write_png(out, nw, nh, 3, scaled)
        # 后续调用（u2net / crop）都按缩放后的尺寸报
        if (nw, nh) != (w, h):
            sx = nw / float(w)
            sy = nh / float(h)
            self.width, self.height = nw, nh
            x1, y1, x2, y2 = self.box
            self.box = (int(x1 * sx), int(y1 * sy), int(x2 * sx), int(y2 * sy))
        return {"path": out, "width": nw, "height": nh,
                "source_width": w, "source_height": h, "scaled": (nw, nh) != (w, h),
                "max_side": cap, "op": "resize"}

    @property
    def mask_path(self):
        return os.path.join(self.workdir, "_stub_mask.png")

    def paths(self):
        return [p for p, _ in self.calls]

    def count(self, path):
        return sum(1 for p in self.paths() if p == path)


def install_stub(module, stub):
    """把插件模块里的 `gw()` 换成桩（与电商包测试同一手法）。"""
    stub.rpc_error = module.RpcError
    module.gw = stub
    return stub


def make_fixture(prefix="seg-fix-"):
    """造一套「原图 + 掩码」并返回 (tmpdir, gateway, subject_box)。"""
    tmp = tempdir(prefix)
    width, height = 320, 240
    src = os.path.join(tmp, "src.png")
    box = (80, 60, 240, 180)
    subject_png(src, width, height, box=box)
    gw = StubGateway(tmp, box, (width, height))
    mask_png(gw.mask_path, width, height, box)
    return tmp, gw, box, src


def make_white_fixture(prefix="seg-white-"):
    """**已是白底**的商品图（白底 + 中央主体）——守卫②该跳过的那一类。"""
    tmp = tempdir(prefix)
    width, height = 320, 240
    src = os.path.join(tmp, "white_studio.png")
    box = (80, 60, 240, 180)
    subject_png(src, width, height, bg=(255, 255, 255), box=box)
    gw = StubGateway(tmp, box, (width, height))
    mask_png(gw.mask_path, width, height, box)
    return tmp, gw, box, src


def frame_png(path, width, height, frame, inner):
    """白框 + 内部整块彩色（模拟「白留边包裹彩色照片」——边框全白但背景有色块）。"""
    raw = bytearray(bytes((255, 255, 255)) * (width * height))
    for y in range(frame, height - frame):
        base = (y * width + frame) * 3
        raw[base:(y * width + width - frame) * 3] = bytes(inner) * (width - 2 * frame)
    pngio.write_png(path, width, height, 3, raw)
    return path


def make_frame_fixture(prefix="seg-frame-", frame=40):
    """白框 40px + 内部彩色块（320x240）：守卫②**必须不跳过**（背景有色块）。"""
    tmp = tempdir(prefix)
    width, height = 320, 240
    src = os.path.join(tmp, "white_frame.png")
    box = (frame, frame // 2, width - frame, height - frame // 2)
    frame_png(src, width, height, frame, (120, 90, 70))
    gw = StubGateway(tmp, box, (width, height))
    mask_png(gw.mask_path, width, height, box)
    return tmp, gw, box, src


def png_from_bytes(blob):
    return pngio.decode_bytes(blob)
