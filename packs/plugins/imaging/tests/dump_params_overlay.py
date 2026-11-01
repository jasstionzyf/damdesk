#!/usr/bin/env python3
"""把 `overlay.SCHEMA` 落成 `resources/params.overlay.json`（N6d 汇总进 params.schema.json 的片段源）。

单一事实源是 `plugin/overlay.py` 的 `SCHEMA`；本脚本只做落盘，
`tests/test_overlay.py::SchemaTest.test_resources_fragment_in_sync` 会校验两者一致（防漂移）。

    python3 packs/plugins/imaging/tests/dump_params_overlay.py
"""
from __future__ import annotations

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
PLUGIN = os.path.join(PACK, "plugin")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import overlay                                     # noqa: E402

OUT = os.path.join(PACK, "resources", "params.overlay.json")


def render():
    return json.dumps(overlay.SCHEMA, ensure_ascii=False, indent=1) + "\n"


def main():
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(render())
    print("写入 %s（%d 个参数）" % (OUT, len(overlay.SCHEMA["properties"])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
