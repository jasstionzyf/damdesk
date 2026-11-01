#!/usr/bin/env python3
"""宿主能力网关的极简客户端（stdlib only；超分包用）。

为什么自带而不用 `dampack` SDK：官方包与宿主同仓发布，装包时 SDK 不一定在包内 venv 里
（`tools/dampack_launcher.py` 只装配 `<pack>`、`<pack>/plugin`、`<pack>/vendor`、
`<pack>/resources`）；零第三方依赖是硬要求，所以这里只做「3 个端点 + 错误码映射」。

铁律（协议 §5/§6，与 `dampack.gateway` 同口径）：
    1. 地址/令牌只从 `DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN` 环境取（一次性令牌，不落盘）；
    2. **不吞 403**：能力未授权 → `GatewayError(code=-32001)`，绝不降级成「看起来成功」；
    3. **不重试**：网关是宿主进程内同步实现，重试只会拖长任务；
    4. 回环地址绕过 `HTTP(S)_PROXY`（宿主环境代理会劫持 127.0.0.1 —— 实测踩过）。
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.request

# 模型档单张实测 2~5s（1200×900 产物），但低配机 / 首批会话加载更久，超时给足
DEFAULT_TIMEOUT = 600.0
_LOOPBACK = ("127.0.0.1", "localhost", "::1", "[::1]")

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

    @classmethod
    def from_env(cls, timeout=None):
        return cls(timeout=timeout)

    @property
    def _opener(self):
        host = self.base_url.split("/")[2] if "//" in self.base_url else ""
        host = host.split(":")[0]
        if host in _LOOPBACK:
            return urllib.request.build_opener(urllib.request.ProxyHandler({}))
        return urllib.request.build_opener()

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
        try:
            with self._opener.open(request, timeout=self.timeout) as resp:
                raw = resp.read().decode("utf-8")
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
        except urllib.error.URLError as exc:
            raise GatewayError(-32000, "网关不可达 %s：%s" % (self.base_url, exc.reason)) from None

    # --- 端点（协议 §6 中本包用到的 3 个） ------------------------------------
    def io_read(self, path):
        return self.call("/io/read", {"path": str(path)})

    def crop(self, path, box, out):
        return self.call("/op/crop", {"path": str(path), "box": [int(v) for v in box],
                                      "out": str(out)})

    def upscale(self, path, scale, out):
        """`scale<=1` = 单趟重采样；`scale==4` 且模型在盘 = x4v3（应答 `op` 字段会点明）。"""
        return self.call("/op/upscale", {"path": str(path), "scale": float(scale),
                                         "out": str(out)})


_SHARED = None


def shared():
    """进程级单例（handler 工作线程里共用，省掉每次重建 opener）。"""
    global _SHARED
    if _SHARED is None:
        _SHARED = Gateway.from_env()
    return _SHARED
