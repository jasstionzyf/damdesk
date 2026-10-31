#!/usr/bin/env python3
"""违规变体 4：health 永不响应（握手挂死）。

宿主懒启动插件后先做 `health` 握手（2s 上限）；本变体把 health 挂住，
宿主必须判握手失败 → 冷却，而不是无限等待。
"""
from __future__ import annotations

import importlib.util
import os
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
_GOOD = os.path.abspath(os.path.join(_HERE, "..", "..", "good", "plugin", "__init__.py"))
_spec = importlib.util.spec_from_file_location("conf_good_health", _GOOD)
good = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(good)

PACK_INFO = dict(good.PACK_INFO, id="conformance.bad-no-health",
                 name="Bad: health never answers")
good.HANDLERS["pack.info"] = lambda params: dict(PACK_INFO)


def health(params):
    time.sleep(30)
    return {"status": "ok"}


good.HANDLERS["health"] = health

if __name__ == "__main__":
    good.serve()
