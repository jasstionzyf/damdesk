#!/usr/bin/env python3
"""违规变体 3：错误码乱用。

- 参数非法回 `-32000`（应为 `-32003`）；
- 网关 403 capability_missing 被吞成 `-32000`（宿主无法分辨「能力缺失」与「插件 bug」）；
- 未知方法回 `-1`（自造码，不是 JSON-RPC 的 `-32601`）。
"""
from __future__ import annotations

import importlib.util
import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_GOOD = os.path.abspath(os.path.join(_HERE, "..", "..", "good", "plugin", "__init__.py"))
_spec = importlib.util.spec_from_file_location("conf_good_codes", _GOOD)
good = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(good)

PACK_INFO = dict(good.PACK_INFO, id="conformance.bad-error-code-misuse",
                 name="Bad: error code misuse")
good.HANDLERS["pack.info"] = lambda params: dict(PACK_INFO)

good.UNKNOWN_METHOD_CODE = -1

_orig_gw = good.gw


def gw(path, payload):
    try:
        return _orig_gw(path, payload)
    except good.RpcError as exc:
        if exc.code == -32001:
            raise good.RpcError(-32000, exc.message)
        raise


good.gw = gw


def judge(params):
    asset = params.get("asset") or {}
    if not asset.get("path"):
        raise good.RpcError(-32000, "asset.path required")   # 应为 -32003
    return good.judge(params.get("job_id") or "", asset,
                      params.get("rules"), params.get("options") or {})


good.HANDLERS["judge"] = judge

if __name__ == "__main__":
    good.serve()
