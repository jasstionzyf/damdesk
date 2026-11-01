"""宿主能力网关客户端（协议 §5 / §6：13 个端点是插件**唯一**的重活出口）。

为什么插件不许自己解码大图/出网：
    - 安全：包跑在宿主子进程里，出网与读任意路径都要过白名单；
    - 兼容：桌面/容器两形态的模型来源不同（内嵌 ort vs dam-ai HTTP），网关替包装平；
    - 体积：包内 venv 保持零第三方依赖（不解码、不背 Pillow/ONNX）。

铁律（协议 §5）：
    1. **不吞 403**：能力未授权就是 `ERR_CAPABILITY_MISSING`，不许降级成 pass；
    2. **不重试**：网关是宿主进程内的同步实现，失败重试只会拖长任务；
    3. 网关地址/令牌只从环境来（`DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN`），
       一次性令牌，插件**不得**落地到文件。

    from dampack import Gateway
    gw = Gateway.from_env()
    info = gw.io_read(asset_path)            # {path,width,height,sha256,...}
    seg  = gw.detect_u2net(asset_path)       # {fg_ratio, mask_path, ...}
    ans  = gw.vl_judge(asset_path, prompt, max_tokens=4096)
    gw.report_emit(job_id, entry)
"""
from __future__ import annotations

import json
import os
import socket
import urllib.error
import urllib.request

from .errors import RpcError, error_for_http
from .protocol import ERR_OTHER, GATEWAY_ENDPOINTS

DEFAULT_TIMEOUT = 120.0
# 产品路径的统一上限（用户裁决 2026-10-20）：思考型模型低于此会 content 空。
MAX_TOKENS_LIMIT = 4096

_LOOPBACK = ("127.0.0.1", "localhost", "::1", "[::1]")


class Gateway:
    """网关客户端。实例可复用（无连接池，urllib 每次新建连接）。"""

    def __init__(self, base_url=None, token=None, timeout=None):
        self.base_url = (base_url if base_url is not None
                         else os.environ.get("DAMPACK_GATEWAY_URL") or "").rstrip("/")
        self.token = token if token is not None else os.environ.get("DAMPACK_GATEWAY_TOKEN") or ""
        raw = timeout if timeout is not None else os.environ.get("DAMPACK_TIMEOUT_SECS")
        self.timeout = _as_timeout(raw)

    @classmethod
    def from_env(cls, timeout=None):
        return cls(timeout=timeout)

    # --- 传输 ---------------------------------------------------------------
    @property
    def _opener(self):
        """回环地址必须绕过代理（宿主环境的 HTTP(S)_PROXY 会劫持 127.0.0.1）。"""
        host = self.base_url.split("/")[2] if "//" in self.base_url else ""
        host = host.split(":")[0]
        if host in _LOOPBACK:
            return urllib.request.build_opener(urllib.request.ProxyHandler({}))
        return urllib.request.build_opener()

    def call(self, path, payload=None):
        """POST 一个端点，返回解析后的 JSON（失败一律抛 `RpcError`）。"""
        if path not in GATEWAY_ENDPOINTS:
            raise RpcError(ERR_OTHER, "未知网关端点 %r（协议 §5 只有 10 个）" % (path,))
        if not self.base_url:
            raise RpcError(ERR_OTHER, "DAMPACK_GATEWAY_URL 未设置，插件无法调用宿主能力")
        body = json.dumps(payload or {}, ensure_ascii=False).encode("utf-8")
        request = urllib.request.Request(
            self.base_url + path, data=body, method="POST",
            headers={"content-type": "application/json; charset=utf-8",
                     "authorization": "Bearer " + self.token},
        )
        try:
            with self._opener.open(request, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8")
                return json.loads(raw) if raw.strip() else {}
        except urllib.error.HTTPError as exc:
            err, detail, op, params = _detail(exc)
            raise error_for_http(exc.code, detail or err, op=op or path, params=params,
                                 endpoint=path)
        except urllib.error.URLError as exc:
            raise RpcError(ERR_OTHER, "网关不可达：%s" % (exc.reason,))
        except (TimeoutError, socket.timeout, OSError) as exc:
            raise RpcError(ERR_OTHER, "网关调用失败：%r" % (exc,))
        except ValueError as exc:
            raise RpcError(ERR_OTHER, "网关应答不是 JSON：%s" % (exc,))

    # --- 13 个端点 ----------------------------------------------------------
    def io_read(self, path):
        return self.call("/io/read", {"path": path})

    def detect_u2net(self, path):
        return self.call("/detect/u2net", {"path": path})

    def detect_bbox(self, path, target_ratio=None):
        payload = {"path": path}
        if target_ratio is not None:
            payload["target_ratio"] = target_ratio
        return self.call("/detect/bbox", payload)

    def detect_white_stats(self, path, band=None, max_dev=None):
        payload = {"path": path}
        if band is not None:
            payload["band"] = band
        if max_dev is not None:
            payload["max_dev"] = max_dev
        return self.call("/detect/white_stats", payload)

    def detect_ocr(self, path, lang=None):
        payload = {"path": path}
        if lang:
            payload["lang"] = lang
        return self.call("/detect/ocr", payload)

    def crop(self, path, box, out):
        return self.call("/op/crop", {"path": path, "box": list(box), "out": out})

    def letterbox(self, path, target_ratio, out, background=None):
        payload = {"path": path, "target_ratio": float(target_ratio), "out": out}
        if background:
            payload["background"] = background
        return self.call("/op/letterbox", payload)

    def upscale(self, path, scale, out):
        return self.call("/op/upscale", {"path": path, "scale": float(scale), "out": out})

    def watermark(self, path, text, out, position=None, opacity=None,
                  font_size=None, margin=None, color=None):
        """文字水印（`op.overlay`）：叠加不改尺寸，渲染在宿主侧（内嵌字体）。

        `opacity` 是 **1..100 整数百分比**（宿主口径）；可选字段按「不传就不发」处理，
        宿主缺省值才是唯一事实源。
        """
        payload = {"path": path, "text": text, "out": out}
        if position is not None:
            payload["position"] = position
        if opacity is not None:
            payload["opacity"] = int(opacity)
        if font_size is not None:
            payload["font_size"] = int(font_size)
        if margin is not None:
            payload["margin"] = int(margin)
        if color is not None:
            payload["color"] = color
        return self.call("/op/watermark", payload)

    def replace_bg(self, path, color, out):
        return self.call("/op/replace_bg", {"path": path, "color": color, "out": out})

    def inpaint(self, path, mask, out):
        return self.call("/op/inpaint", {"path": path, "mask": mask, "out": out})

    def vl_judge(self, path, prompt, max_tokens=None):
        tokens = MAX_TOKENS_LIMIT if max_tokens is None else int(max_tokens)
        if tokens <= 0 or tokens > MAX_TOKENS_LIMIT:
            raise RpcError(ERR_OTHER, "max_tokens=%r 越界（1..%d）" % (max_tokens, MAX_TOKENS_LIMIT))
        return self.call("/vl/judge", {"path": path, "prompt": prompt, "max_tokens": tokens})

    def report_emit(self, job_id, entry):
        return self.call("/report/emit", {"job_id": job_id, "entry": entry})


def _detail(exc):
    """解析网关错误体 → (error, detail, op, params)。

    协议 §5 只规定错误码映射；错误文案里的 `op` / `params` / `detail` 是宿主
    定位问题时唯一的信息来源，不许丢（K07 D7：裸 `-32003` 没法定位）。
    """
    try:
        payload = json.loads(exc.read().decode("utf-8"))
    except Exception:  # noqa: BLE001 —— 错误体不是 JSON 时不该掩盖原始状态码
        return "", "", "", None
    if not isinstance(payload, dict):
        return "", "", "", None
    return (str(payload.get("error") or ""), str(payload.get("detail") or ""),
            str(payload.get("op") or ""), payload.get("params"))


def _as_timeout(raw):
    if raw in (None, ""):
        return DEFAULT_TIMEOUT
    try:
        return max(5.0, float(raw))
    except (TypeError, ValueError):
        return DEFAULT_TIMEOUT
