#!/usr/bin/env python3
"""把三组 schema 片段汇总成 `resources/params.schema.json`（N6d 装配，可重跑）。

单一事实源 = 各组模块的 `SCHEMA`（`resources/params.*.json` 只是它们落盘的样子）：
    geometry.SCHEMA  → group=geometry（N6a：缩放/裁剪）
    codec.SCHEMA     → group=codec   （N6b：转格式/压缩）
    overlay.SCHEMA   → group=overlay （N6c：水印/改名）

本脚本只做一件事：把三组片段并进一份 schema，`group` 分区（`basic` + `geometry` / `codec` /
`overlay`），并补一个 `action` 参数（enum 六选一，默认 `resize`）放在 `basic` 组 ——
**动作靠参数选，不靠方法名**：宿主调用面只有 `judge`，多动作插件必须有显式动作参数。

两条**不做**的事（卡面硬边界 `t_979f61d2`）：
1. 不引入跨组 `when` 依赖：`action` 与各组参数之间**不**加条件门控（那会让 33 个参数变成
   「选了动作才出现」的参数墙，也违反「`when` 只在同组内生效」）；
2. 不改写组内已有的 `when`（如 `resizeMode=long_edge`）—— 片段里怎么写就怎么落。

用法：
    python3 packs/plugins/imaging/tests/dump_pack_schema.py            # 落盘
    python3 packs/plugins/imaging/tests/dump_pack_schema.py --check    # 只校验是否漂移
"""
from __future__ import annotations

import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
PLUGIN = os.path.join(PACK, "plugin")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import codec                                      # noqa: E402
import geometry                                   # noqa: E402
import overlay                                    # noqa: E402

OUT = os.path.join(PACK, "resources", "params.schema.json")

# 动作参数：顺序与 `plugin/__init__.py` 的 `ACTION_SPECS` 一致（schema 侧的单一事实源）
# `basic` 组 = 基本（动作选择），后面按组分区 geometry / codec / overlay
ACTION_GROUP = {"key": "basic", "title": "基本（选动作）"}
ACTION_PROPERTY = {
    "type": "enum",
    "title": "处理动作",
    "options": ["resize", "crop", "convert", "compress", "watermark", "rename"],
    "default": "resize",
    "group": "basic",
}

def merged():
    """三组片段 + 动作参数 → 一份完整 schema（顺序稳定，便于 diff）。"""
    groups = [dict(ACTION_GROUP)]
    seen = {"action"}
    properties = {"action": dict(ACTION_PROPERTY)}
    required = []
    for module in (geometry, codec, overlay):
        for group in module.SCHEMA.get("groups") or []:
            if group["key"] not in seen:
                seen.add(group["key"])
                groups.append(dict(group))
        for key, spec in (module.SCHEMA.get("properties") or {}).items():
            if key in properties:
                raise SystemExit("参数键 %s 在多组重复声明（装配期就炸，别让它静默生效）" % key)
            properties[key] = dict(spec)
        for key in module.SCHEMA.get("required") or []:
            if key not in required:
                required.append(key)
    return {"schemaVersion": 1, "groups": groups,
            "properties": properties, "required": required}


def render():
    return json.dumps(merged(), ensure_ascii=False, indent=1) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description="汇总 imaging 插件参数 schema")
    parser.add_argument("--check", action="store_true", help="只比对，不写盘（漂移 → 退出码 1）")
    args = parser.parse_args(argv)
    text = render()
    schema = merged()
    if args.check:
        try:
            with open(OUT, encoding="utf-8") as fh:
                current = fh.read()
        except OSError as exc:
            print("读不到 %s：%s" % (OUT, exc))
            return 1
        if current != text:
            print("漂移：%s 与各组 SCHEMA 不一致（重跑不带 --check）" % OUT)
            return 1
        print("一致：%s（%d 参数 / %d 组）"
              % (OUT, len(schema["properties"]), len(schema["groups"])))
        return 0
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(text)
    print("写入 %s（%d 参数 / %d 组）"
          % (OUT, len(schema["properties"]), len(schema["groups"])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
