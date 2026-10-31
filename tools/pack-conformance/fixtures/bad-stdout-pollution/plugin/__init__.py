#!/usr/bin/env python3
"""违规变体 1：stdout 污染（banner + 每请求 debug 打到 fd1）。

协议要求 stdout 只走协议行；本变体在启动与每次 judge 时直接写 fd1，
正确宿主会把这行当成「非协议输出」拒绝。
"""
from __future__ import annotations

import importlib.util
import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_GOOD = os.path.abspath(os.path.join(_HERE, "..", "..", "good", "plugin", "__init__.py"))
_spec = importlib.util.spec_from_file_location("conf_good_stdout", _GOOD)
good = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(good)

PACK_INFO = dict(good.PACK_INFO, id="conformance.bad-stdout-pollution",
                 name="Bad: stdout pollution")
good.HANDLERS["pack.info"] = lambda params: dict(PACK_INFO)

os.write(1, b"[dampack] loading conformance.bad-stdout-pollution v1.0.0\n")

_orig_judge = good.judge


def judge(job_id, asset, rules=None, options=None):
    os.write(1, ("[debug] judge job=%s\n" % (job_id,)).encode("utf-8"))
    return _orig_judge(job_id, asset, rules, options)


good.HANDLERS["judge"] = lambda p: judge(p.get("job_id") or "", p.get("asset"),
                                        p.get("rules"), p.get("options") or {})

if __name__ == "__main__":
    good.serve()
