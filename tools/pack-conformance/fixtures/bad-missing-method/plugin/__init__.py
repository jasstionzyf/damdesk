#!/usr/bin/env python3
"""违规变体 2：缺方法（未实现 `report` 与 `cancel`）。

真插件忘了 cancel 会导致宿主的超时铁律里「cancel → 杀进程」只能走杀进程，
report 缺失则交付报告无法按 job 归账。
"""
from __future__ import annotations

import importlib.util
import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_GOOD = os.path.abspath(os.path.join(_HERE, "..", "..", "good", "plugin", "__init__.py"))
_spec = importlib.util.spec_from_file_location("conf_good_missing", _GOOD)
good = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(good)

PACK_INFO = dict(good.PACK_INFO, id="conformance.bad-missing-method",
                 name="Bad: missing methods")
good.HANDLERS["pack.info"] = lambda params: dict(PACK_INFO)

del good.HANDLERS["report"]
del good.HANDLERS["cancel"]

if __name__ == "__main__":
    good.serve()
