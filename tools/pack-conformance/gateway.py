"""conformance 假网关：按 manifest.capabilities 提供 13 个端点桩。

它**不校验任何算法正确性**（不跑 u2net/OCR/VL），只按资产文件名里的约定回固定桩数据；
验的是「插件怎么用网关」四件事：一次性令牌、能力白名单、错误映射、产物落盘。

文件名约定（仅 conformance 夹具使用）：
    <任意>_<W>x<H>.<ext>   宽高，例如 main_1600x1600.png
    fg0.30                 主体占比，例如 small_600x600_fg0.30.png
    overlay                图上有叠加文字（/detect/ocr 回 2 行）
    white12.5              边缘白度偏差 12.5（/detect/white_stats 回该 mean_dev，缺省 2.0）
    vlundet                /vl/judge 回 undetermined（模拟 VL 解析失败）
    catfail                /vl/judge 回 fail
    slow                   插件侧自己 sleep（用于 cancel 断言，与网关无关）
"""
from __future__ import annotations

import hashlib
import json
import math
import os
import re
import struct
import threading
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from protocol import GATEWAY_ENDPOINTS

_DIMS_RE = re.compile(r"(\d{2,5})x(\d{2,5})")
_FG_RE = re.compile(r"fg(\d(?:\.\d+)?)")
_WHITE_RE = re.compile(r"white(\d+(?:\.\d+)?)")
_PNG_SIG = b"\x89PNG\r\n\x1a\n"


def png_head(width, height):
    """只带 IHDR/IEND 的合法 PNG 头（够 `image_size` 读宽高；不含像素数据）。"""
    ihdr = struct.pack(">IIBBBBB", int(width), int(height), 8, 2, 0, 0, 0)
    chunk = struct.pack(">I", len(ihdr)) + b"IHDR" + ihdr
    crc = struct.pack(">I", zlib.crc32(b"IHDR" + ihdr) & 0xFFFFFFFF)
    iend = struct.pack(">I", 0) + b"IEND" + struct.pack(">I", zlib.crc32(b"IEND") & 0xFFFFFFFF)
    return _PNG_SIG + chunk + crc + iend


def _dims(path):
    """宽高**以文件头为准**（桩文件按算子声明尺寸写 IHDR），文件名约定只作兜底。

    为什么必须一致：插件侧现在按 `/io/read` 的实际尺寸走（宿主应答可能说谎），
    桩若回「应答 800×800、文件却是 1600×1600」，就等于在测假故障。
    """
    try:
        with open(path, "rb") as fh:
            head = fh.read(24)
        if len(head) >= 24 and head[:8] == _PNG_SIG and head[12:16] == b"IHDR":
            return int.from_bytes(head[16:20], "big"), int.from_bytes(head[20:24], "big")
    except OSError:
        pass
    m = _DIMS_RE.search(os.path.basename(path or ""))
    if m:
        return int(m.group(1)), int(m.group(2))
    return 1600, 1600


def _fg_ratio(path):
    m = _FG_RE.search(os.path.basename(path or ""))
    if m:
        return float(m.group(1))
    return 0.62


class FakeGateway:
    """宿主能力网关的桩实现。"""

    def __init__(self, capabilities, workroot):
        self.capabilities = list(capabilities or [])
        self.workroot = os.path.abspath(workroot)
        self.token = "conf-token-" + hashlib.sha256(self.workroot.encode()).hexdigest()[:16]
        self.requests = []      # 每次命中：{path, capability, allowed, status, payload}
        self.denied = []        # 被 403 拒的路径
        self.emits = []         # /report/emit 的 (job_id, entry)
        self._lock = threading.Lock()
        self._httpd = None

    # --- 生命周期 -------------------------------------------------------
    def start(self):
        gw = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):  # 静音
                pass

            def _send(self, code, payload):
                body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("content-type", "application/json; charset=utf-8")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except Exception:
                    payload = {}
                token = (self.headers.get("authorization") or "").replace("Bearer ", "").strip()
                cap = GATEWAY_ENDPOINTS.get(self.path)
                allowed = cap is not None and cap in gw.capabilities
                record = {"path": self.path, "capability": cap, "allowed": allowed,
                          "status": 0, "payload": payload}
                with gw._lock:
                    gw.requests.append(record)

                if token != gw.token:
                    record["status"] = 401
                    return self._send(401, {"error": "unauthorized"})
                if cap is None:
                    record["status"] = 404
                    return self._send(404, {"error": "no_such_endpoint", "path": self.path})
                if not allowed:
                    record["status"] = 403
                    with gw._lock:
                        gw.denied.append(self.path)
                    return self._send(403, {"error": "capability_missing", "capability": cap})

                try:
                    status, result = gw.dispatch(self.path, payload)
                except _GatewayError as exc:
                    record["status"] = exc.status
                    return self._send(exc.status, {"error": exc.reason, "detail": exc.detail})
                record["status"] = status
                return self._send(status, result)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._httpd.gw = self
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        return self

    @property
    def base_url(self):
        host, port = self._httpd.server_address[:2]
        return "http://%s:%d" % (host, port)

    def stop(self):
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()

    # --- 端点桩 ---------------------------------------------------------
    def dispatch(self, path, payload):
        if path == "/io/read":
            return self._io_read(payload)
        if path == "/detect/u2net":
            return self._u2net(payload)
        if path == "/detect/bbox":
            return self._bbox(payload)
        if path == "/detect/ocr":
            return self._ocr(payload)
        if path == "/detect/white_stats":
            return self._white_stats(payload)
        if path in ("/op/crop", "/op/resize", "/op/letterbox", "/op/upscale", "/op/watermark",
                    "/op/replace_bg", "/op/inpaint"):
            return self._op(path, payload)
        if path == "/vl/judge":
            return self._vl(payload)
        if path == "/report/emit":
            return self._emit(payload)
        raise _GatewayError(404, "no_such_endpoint", path)

    def _io_read(self, payload):
        path = payload.get("path")
        if not isinstance(path, str) or not path:
            raise _GatewayError(400, "invalid_params", "path")
        if not os.path.isfile(path):
            raise _GatewayError(404, "asset_unreadable", path)
        with open(path, "rb") as fh:
            data = fh.read(65536)
        w, h = _dims(path)
        return 200, {
            "path": path, "size": os.path.getsize(path),
            "sha256": hashlib.sha256(data).hexdigest(),
            "mime": "image/png", "width": w, "height": h,
        }

    def _u2net(self, payload):
        path = payload.get("path")
        if not os.path.isfile(str(path or "")):
            raise _GatewayError(404, "asset_unreadable", str(path))
        w, h = _dims(path)
        mask = os.path.join(self.workroot, "mask-%s.png" % hashlib.sha256(str(path).encode()).hexdigest()[:8])
        self._write_stub(mask, 8, 8)
        return 200, {"path": path, "fg_ratio": _fg_ratio(path), "mask_path": mask,
                     "width": w, "height": h}

    def _bbox(self, payload):
        """掩码→主体框桩：框 = 短边 sqrt(fg) 比例的居中方框；给 target_ratio 时回窗。"""
        path = payload.get("path")
        if not os.path.isfile(str(path or "")):
            raise _GatewayError(404, "asset_unreadable", str(path))
        w, h = _dims(path)
        fg = _fg_ratio(path)
        side = int(round(min(w, h) * math.sqrt(fg)))
        side = max(8, min(side, min(w, h)))
        bbox = [(w - side) // 2, (h - side) // 2, (w + side) // 2, (h + side) // 2]
        result = {"path": path, "width": w, "height": h, "fg_ratio": fg,
                  "engine": "stub-u2net", "grid": 320,
                  "bbox": bbox, "dominant_bbox": list(bbox), "window": None}
        target = payload.get("target_ratio")
        if target is not None:
            try:
                target = float(target)
            except Exception:
                raise _GatewayError(400, "invalid_params", "target_ratio")
            if not 0.0 < target <= 1.0:
                raise _GatewayError(400, "invalid_params", "target_ratio")
            area = fg * w * h
            win_side = int(round(math.sqrt(area / target)))
            win_side = max(8, min(win_side, min(w, h)))
            cx, cy = (bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0
            x1 = int(round(max(0, min(cx - win_side / 2.0, w - win_side))))
            y1 = int(round(max(0, min(cy - win_side / 2.0, h - win_side))))
            box = [x1, y1, x1 + win_side, y1 + win_side]
            result["window"] = {"box": box,
                                "ratio": round(area / float(win_side * win_side), 4),
                                "target_ratio": target}
        return 200, result

    def _white_stats(self, payload):
        """白边统计桩：文件名 `white12.5` → mean_dev 12.5（缺省 2.0）。"""
        path = payload.get("path")
        if not os.path.isfile(str(path or "")):
            raise _GatewayError(404, "asset_unreadable", str(path))
        m = _WHITE_RE.search(os.path.basename(str(path)))
        mean_dev = float(m.group(1)) if m else 2.0
        band = payload.get("band")
        band = 4 if band is None else band
        try:
            band = int(band)
        except Exception:
            raise _GatewayError(400, "invalid_params", "band")
        if band < 1:
            raise _GatewayError(400, "invalid_params", "band")
        try:
            threshold = float(payload.get("max_dev", 8))
        except Exception:
            raise _GatewayError(400, "invalid_params", "max_dev")
        w, h = _dims(path)
        return 200, {"path": path, "width": w, "height": h,
                     "engine": "stub-white-edge", "mean_dev": mean_dev,
                     "worst_dev": mean_dev, "band": band,
                     "edge_pixels": 2 * band * (w + h), "threshold": threshold,
                     "pass": mean_dev <= threshold}

    def _ocr(self, payload):
        path = payload.get("path")
        if not os.path.isfile(str(path or "")):
            raise _GatewayError(404, "asset_unreadable", str(path))
        lines = []
        if "overlay" in os.path.basename(str(path)):
            lines = [
                {"text": "限时 5 折", "box": [40, 30, 240, 90], "score": 0.97},
                {"text": "包邮", "box": [40, 100, 160, 150], "score": 0.95},
            ]
        return 200, {"path": path, "lines": lines, "engine": "stub-ocr"}

    def _op(self, path, payload):
        src = payload.get("path")
        out = payload.get("out")
        if not isinstance(out, str) or not out:
            raise _GatewayError(400, "invalid_params", "out")
        out_abs = os.path.abspath(out)
        if not out_abs.startswith(self.workroot + os.sep):
            raise _GatewayError(400, "out_of_workroot", out)
        if not os.path.isfile(str(src or "")):
            raise _GatewayError(404, "asset_unreadable", str(src))
        w, h = _dims(src)
        if path == "/op/crop":
            box = payload.get("box") or [0, 0, w, h]
            try:
                w, h = max(1, int(box[2]) - int(box[0])), max(1, int(box[3]) - int(box[1]))
            except Exception:
                raise _GatewayError(400, "invalid_params", "box")
        elif path == "/op/resize":
            # 只缩不放：桩按 max_side 等比算尺寸（与宿主 core-media::resize_to 同语义）
            try:
                cap = int(payload.get("max_side") or 0)
            except Exception:
                raise _GatewayError(400, "invalid_params", "max_side")
            if cap < 16:
                raise _GatewayError(400, "invalid_params", "max_side")
            long_side = max(w, h)
            if long_side > cap:
                scale = cap / float(long_side)
                w, h = max(1, int(round(w * scale))), max(1, int(round(h * scale)))
        elif path == "/op/letterbox":
            ratio = float(payload.get("target_ratio") or 1.0)
            side = max(w, h)
            w, h = int(round(side * max(1.0, ratio))), side
        elif path == "/op/upscale":
            scale = float(payload.get("scale") or 1.0)
            w, h = int(round(w * scale)), int(round(h * scale))
        elif path == "/op/watermark":
            # 叠加不改尺寸：桩网关只校验参数显式性（空 text / 越界 opacity 一律 400），
            # 产物按源尺寸写桩 —— conformance 验的是「插件发对了请求」，不是渲染质量。
            text = payload.get("text")
            if not isinstance(text, str) or not text.strip():
                raise _GatewayError(400, "invalid_params", "text")
            if payload.get("opacity") is not None:
                try:
                    opacity = int(payload["opacity"])
                except Exception:
                    raise _GatewayError(400, "invalid_params", "opacity")
                if not 1 <= opacity <= 100:
                    raise _GatewayError(400, "invalid_params", "opacity")
        self._write_stub(out_abs, w, h)
        return 200, {"path": out_abs, "width": w, "height": h, "op": path.rsplit("/", 1)[-1],
                     "target_ratio": payload.get("target_ratio")}

    def _vl(self, payload):
        path = payload.get("path")
        if not os.path.isfile(str(path or "")):
            raise _GatewayError(404, "asset_unreadable", str(path))
        name = os.path.basename(str(path))
        if "vlundet" in name:
            single = {"path": path, "undetermined": True,
                      "reason": "vl_parse_failed", "raw": ""}
        elif "catfail" in name:
            single = {"path": path, "undetermined": False,
                      "raw": "{\"status\":\"fail\"}",
                      "rules": [{"status": "fail", "label": "not_apparel", "score": 0.31}]}
        else:
            single = {"path": path, "undetermined": False,
                      "raw": "{\"status\":\"pass\"}",
                      "rules": [{"status": "pass", "label": "apparel", "score": 0.92}]}
        # 协议 1.2 增量：`samples` > 1 时回 `raws`（桩每次都回同一份答案 —— 翻转保护由
        # 包自己判一致；桩不该假装抖动，否则 conformance 结论不可复现）
        try:
            samples = int(payload.get("samples") or 1)
        except (TypeError, ValueError):
            samples = 1
        if samples > 1:
            single = dict(single, samples=samples, raws=[dict(single)] * samples)
        return 200, single

    def _emit(self, payload):
        job_id = payload.get("job_id")
        entry = payload.get("entry")
        if not isinstance(job_id, str) or not job_id:
            raise _GatewayError(400, "invalid_params", "job_id")
        with self._lock:
            self.emits.append((job_id, entry))
        return 200, {"ok": True, "job_id": job_id}

    def _write_stub(self, path, width=1600, height=1600):
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "wb") as fh:
            fh.write(png_head(width, height))

    # --- 断言辅助 -------------------------------------------------------
    def hits(self, path=None):
        with self._lock:
            return [r for r in self.requests if path is None or r["path"] == path]

    def emits_for(self, job_id):
        with self._lock:
            return [e for e in self.emits if e[0] == job_id]


class _GatewayError(Exception):
    def __init__(self, status, reason, detail=""):
        super().__init__(reason)
        self.status = status
        self.reason = reason
        self.detail = detail
