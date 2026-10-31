#!/usr/bin/env python3
"""宿主能力网关的极简客户端（stdlib only；imaging 插件共用）。

为什么自带而不用 `dampack` SDK：官方包与宿主同仓发布，装包时 SDK 不一定在包内 venv 里
（`tools/dampack_launcher.py` 只装配 `<pack>`、`<pack>/plugin`、`<pack>/vendor`、
`<pack>/resources`）；零第三方依赖是硬要求，所以这里只做「4 个端点 + 错误码映射」。

铁律（协议 §5/§6，与 `dampack.gateway` 同口径）：
    1. 地址/令牌只从 `DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN` 环境取（一次性令牌，不落盘）；
    2. **不吞 403**：能力未授权 → `GatewayError(code=-32001)`，绝不降级成「看起来成功」；
    3. **不重试**：网关是宿主进程内同步实现，重试只会拖长任务；
    4. 回环地址绕过 `HTTP(S)_PROXY`（宿主环境代理会劫持 127.0.0.1 —— 实测踩过）。

N6b（编码组）/N6c（叠加组）直接用 `from gw import Gateway`，别再各写一份。
"""
from __future__ import annotations

import http.client
import json
import os
import sys
import urllib.error
import urllib.request

DEFAULT_TIMEOUT = 120.0
_LOOPBACK = ("127.0.0.1", "localhost", "::1", "[::1]")


def log(msg):
    """诊断日志一律走 stderr（stdout 是协议流，见协议 §9 铁律）。"""
    try:
        sys.stderr.write("[imaging-gw] %s\n" % msg)
        sys.stderr.flush()
    except Exception:  # noqa: BLE001 —— 日志失败不许影响业务
        pass


def _retry_enabled():
    raw = os.environ.get("DAMPACK_GW_RETRY")
    return str(raw).strip().lower() not in ("0", "off", "false", "no")


# 不可重试端点：`/report/emit` 是**追加**语义（宿主把上报行存进 emits），重试会多记一条。
_NON_IDEMPOTENT = ("/report/emit",)

# HTTP 状态 → 协议 §5 错误码闭集（403 能力缺失 / 404 资产不可读 / 400 参数非法 / 其余 -32000）
_STATUS_CODE = {403: -32001, 404: -32002, 400: -32003}


class GatewayError(RuntimeError):
    """网关调用失败。`code` = 协议 §5 错误码；`error`/`detail` 原文供排障。"""

    def __init__(self, code, message, status=None, error=None, detail=None, capability=None):
        super().__init__(message)
        self.code = int(code)
        self.status = status
        self.error = error
        self.detail = detail
        self.capability = capability

    def __str__(self):
        parts = [f"{self.error or 'gateway_error'}({self.status})"]
        # 非 HTTP 失败（URLError/连接被拒）没有 error/status 字段：这时**必须**回 args 里的
        # 人读原因，否则结果行只剩 `gateway_error(None)` —— 排障时等于没有信息（K07 D7 口径）。
        if self.error is None and self.status is None and self.args:
            parts.append(str(self.args[0]))
        if self.detail:
            parts.append(str(self.detail))
        if self.capability:
            parts.append("capability=%s" % self.capability)
        return "：".join(parts)


class Gateway:
    """端点客户端（实例可复用；urllib 每次新建连接）。"""

    def __init__(self, base_url=None, token=None, timeout=None):
        self.base_url = (base_url if base_url is not None
                         else os.environ.get("DAMPACK_GATEWAY_URL") or "").rstrip("/")
        self.token = token if token is not None else (os.environ.get("DAMPACK_GATEWAY_TOKEN") or "")
        raw = timeout if timeout is not None else os.environ.get("DAMPACK_TIMEOUT_SECS")
        try:
            self.timeout = float(raw) if raw else DEFAULT_TIMEOUT
        except (TypeError, ValueError):
            self.timeout = DEFAULT_TIMEOUT
        # opener 缓存（见 `_opener`）；不复用会每次调用都重建 handler 链
        self._opener_cache = None

    @classmethod
    def from_env(cls, timeout=None):
        return cls(timeout=timeout)

    @property
    def _opener(self):
        # 每个 Gateway 实例只建一次 opener：`build_opener()` 每次调用都会重新装配一遍
        # handler 链（真机上每张图 6~10 次网关调用，等于白白重建几十次）。
        if self._opener_cache is None:
            host = self.base_url.split("/")[2] if "//" in self.base_url else ""
            host = host.split(":")[0]
            if host in _LOOPBACK:
                self._opener_cache = urllib.request.build_opener(
                    urllib.request.ProxyHandler({}))
            else:
                self._opener_cache = urllib.request.build_opener()
        return self._opener_cache

    # --- 传输 ---------------------------------------------------------------
    def call(self, path, payload=None):
        if not self.base_url:
            raise GatewayError(-32000, "DAMPACK_GATEWAY_URL 未设置，插件无法调用宿主能力")
        body = json.dumps(payload or {}, ensure_ascii=False).encode("utf-8")
        request = urllib.request.Request(
            self.base_url + path, data=body, method="POST",
            headers={"content-type": "application/json; charset=utf-8",
                     "authorization": "Bearer " + self.token},
        )
        attempts = 2 if (_retry_enabled() and path not in _NON_IDEMPOTENT) else 1
        last = None
        for attempt in range(attempts):
            try:
                with self._opener.open(request, timeout=self.timeout) as resp:
                    raw = resp.read().decode("utf-8")
                    if attempt:
                        log("网关 %s 第 %d 次尝试成功（前一次传输失败：%r）"
                            % (path, attempt + 1, last))
                    return json.loads(raw) if raw.strip() else {}
            except urllib.error.HTTPError as exc:
                raw = exc.read().decode("utf-8", "replace")
                try:
                    doc = json.loads(raw) if raw.strip() else {}
                except ValueError:
                    doc = {"error": "http_%d" % exc.code, "detail": raw[:400]}
                raise GatewayError(
                    _STATUS_CODE.get(exc.code, -32000),
                    "网关 %s 失败：%s" % (path, doc.get("error") or exc.code),
                    status=exc.code, error=doc.get("error"), detail=doc.get("detail"),
                    capability=doc.get("capability"),
                ) from None
            except (urllib.error.URLError, TimeoutError, OSError,
                    http.client.HTTPException) as exc:
                # 传输层没有任何应答（连接被拒/被重置/读到半截/超时）：换新连接重试一次。
                # 与「网关说不」不同——没有 HTTP 状态、没有语义副作用（KI-29 真机形态）。
                last = exc
                if attempt + 1 >= attempts:
                    raise GatewayError(-32000, "网关不可达 %s：%r"
                                       % (self.base_url + path, exc)) from None
                log("网关 %s 传输失败（%r），换新连接重试一次" % (path, exc))
        raise GatewayError(-32000, "网关不可达 %s：%r" % (self.base_url + path, last))


    # --- 端点（协议 §6 中本包用到的 6 个） ------------------------------------
    def io_read(self, path):
        return self.call("/io/read", {"path": str(path)})

    def detect_bbox(self, path, target_ratio=None, min_instance_ratio=None):
        payload = {"path": str(path)}
        if target_ratio is not None:
            payload["target_ratio"] = target_ratio
        if min_instance_ratio is not None:
            payload["min_instance_ratio"] = min_instance_ratio
        return self.call("/detect/bbox", payload)

    def crop(self, path, box, out):
        return self.call("/op/crop", {"path": str(path), "box": [int(v) for v in box],
                                      "out": str(out)})

    def letterbox(self, path, target_ratio, out, background=None):
        payload = {"path": str(path), "target_ratio": float(target_ratio), "out": str(out)}
        if background is not None:
            payload["background"] = str(background)
        return self.call("/op/letterbox", payload)

    def upscale(self, path, scale, out):
        """`scale<=1` 时宿主走单趟重采样（`op=upscale.resample`）——resize 只缩不放靠它。"""
        return self.call("/op/upscale", {"path": str(path), "scale": float(scale),
                                         "out": str(out)})

    def encode(self, path, out, quality=None, background=None, auto_orient=None, fmt=None):
        """`/op/encode`（N6b）：按 `out` 扩展名重编码，可带质量/背景/EXIF 方向。

        `fmt` 只在需要**断言**目标格式时传（宿主会校验它与 `out` 扩展名一致，不一致 400）；
        平时留空即可——宿主的"格式唯一口径"就是 out 的扩展名。
        """
        payload: dict = {"path": str(path), "out": str(out)}
        if fmt is not None:
            payload["format"] = str(fmt)
        if quality is not None:
            payload["quality"] = int(quality)
        if background is not None:
            payload["background"] = str(background)
        if auto_orient is not None:
            payload["auto_orient"] = bool(auto_orient)
        return self.call("/op/encode", payload)
    def watermark(self, path, text, out, position=None, opacity=None,
                  font_size=None, margin=None, color=None):
        """文字水印（`op.overlay`，N6c）：叠加不改尺寸，渲染在宿主侧（内嵌 Noto Sans SC）。

        `opacity` 是 **1..100 整数百分比**（`core-media` 口径）；0..1 比例的换算由插件
        `overlay.parse_opacity()` 负责 —— 单位只在一处换算，别在客户端和服务端各猜一次。
        可选字段按「不传就不发」处理：宿主的缺省值（position=bottom_right / opacity=50 /
        font_size=48 / margin=16 / color=ffffff）才是唯一事实源。
        """
        payload: dict = {"path": str(path), "text": str(text), "out": str(out)}
        if position is not None:
            payload["position"] = str(position)
        if opacity is not None:
            payload["opacity"] = int(opacity)
        if font_size is not None:
            payload["font_size"] = int(font_size)
        if margin is not None:
            payload["margin"] = int(margin)
        if color is not None:
            payload["color"] = str(color)
        return self.call("/op/watermark", payload)
