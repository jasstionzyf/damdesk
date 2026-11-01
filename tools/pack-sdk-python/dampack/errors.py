"""插件侧错误：一个异常类 + HTTP→JSON-RPC 错误码映射（协议 §5）。

铁律：**不吞 403、不重试、不把网关错误改写成 pass/fail**。
宿主据此决定 verdict 落 `undetermined` 还是整条任务失败。
"""
from __future__ import annotations

import json

from .protocol import (
    ERR_ASSET_UNREADABLE,
    ERR_CANCELED,
    ERR_CAPABILITY_MISSING,
    ERR_INVALID_PARAMS,
    ERR_METHOD_NOT_FOUND,
    ERR_OTHER,
    ERR_PARSE,
    ERR_TIMEOUT,
    PLUGIN_ERROR_CODES,
)


class RpcError(Exception):
    """带 JSON-RPC 错误码的插件异常（`-32000..-32005`）。

    用法：`raise RpcError(ERR_INVALID_PARAMS, "asset.path 必填")`。
    `message` 会原样出现在错误帧里（用户可见），所以写人话、别塞堆栈。
    """

    def __init__(self, code, message):
        if code not in PLUGIN_ERROR_CODES:
            raise ValueError("未知插件错误码：%r（见 dampack.protocol）" % (code,))
        super().__init__("%s: %s" % (code, message))
        self.code = int(code)
        self.message = str(message)

    def frame(self):
        return {"code": self.code, "message": self.message}

    def __repr__(self):
        return "RpcError(%d, %r)" % (self.code, self.message)


# 网关 HTTP 状态 → 插件错误码（协议 §5 的唯一一张表，别在各包内重写）
HTTP_ERROR_CODES = {
    400: ERR_INVALID_PARAMS,
    401: ERR_OTHER,               # 令牌失效/未设置：是宿主配置问题，不是包的无能
    403: ERR_CAPABILITY_MISSING,  # 能力未授权（manifest.capabilities 缺项）
    404: ERR_ASSET_UNREADABLE,    # 资产不可读
    408: ERR_TIMEOUT,
    429: ERR_OTHER,
    500: ERR_OTHER,
    501: ERR_CAPABILITY_MISSING,  # 端点未实现（网关 v1 缺该能力）
    503: ERR_OTHER,
}


def error_for_http(status, detail="", op=None, params=None, endpoint=None):
    """把网关 HTTP 错误翻译成 `RpcError`。

    `detail` 是网关回的 `detail`（人读原因），`op` 是出错端点，`params` 是实际请求参数。
    定位信息一律拼进 `message`——宿主把它原样落进 `evidence.repair_error`，
    丢掉上下文等于没有证据（K07 D7：不许出现裸 `参数非法（400）invalid_params`）。
    """
    code = HTTP_ERROR_CODES.get(int(status), ERR_OTHER)
    ctx = []
    where = op or endpoint
    if where:
        ctx.append("[%s]" % (where,))
    if params:
        try:
            shown = json.dumps(params, ensure_ascii=False, sort_keys=True)
        except (TypeError, ValueError):
            shown = str(params)
        if len(shown) > 300:
            shown = shown[:300] + "…"
        ctx.append(shown)
    if detail:
        ctx.append(str(detail))
    suffix = (" " + " ".join(ctx)) if ctx else ""
    return RpcError(code, "网关错误 %d%s" % (int(status), suffix))
