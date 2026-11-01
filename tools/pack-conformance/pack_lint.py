#!/usr/bin/env python3
"""pack_lint — 行业包 manifest 静态门禁（conformance 的轻量前置，不发子进程）。

用法：
    python3 tools/pack-conformance/pack_lint.py <pack_dir> [pack_dir ...]

门禁面（任何一条不过 → 退出码 1）：
  1. manifest.json 存在且是合法 JSON；
  2. `validate_manifest` 全过 —— 含 v2.0 硬门：api_version 必须恰为 2.0、
     旧键 `kinds` 一律拒装并给映射指路（LEGACY_HINT）；
  3. **outputs 必须显式声明**：缺键 / 空数组 / 非数组 / 含未知值 / 重复值 →
     报错非零退出。v2.0 没有「没写 = 默认判定类」这回事，包作者必须自己写明
     这包出 verdict 还是 artifact（或两者），宿主按它分流出判定面/处理面。

退出码：0 = 全部通过；1 = 有违例（逐条打印原文）；2 = 用法 / IO 错误。

与 conformance 的分工：pack_lint 只看 manifest 静态面（CI 里对每个 PR 的包
先跑它拦低级错误），协议行为面（传输/方法/错误码/取消/产物）归
`run.py` 的 16 条断言。两侧校验同源于 `protocol.py`，不许各写一份口径。
"""
from __future__ import annotations

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import protocol as proto  # noqa: E402


def lint_pack(pack_dir):
    """lint 单个包目录 → (violations: list[str], manifest or None)。"""
    if not os.path.isdir(pack_dir):
        return ["目录不存在：%s" % pack_dir], None
    path = os.path.join(pack_dir, "manifest.json")
    if not os.path.isfile(path):
        return ["缺 manifest.json"], None
    try:
        with open(path, encoding="utf-8") as fh:
            manifest = json.load(fh)
    except Exception as exc:
        return ["manifest.json 不是合法 JSON：%r" % (exc,)], None
    if not isinstance(manifest, dict):
        return ["manifest 不是 JSON 对象"], None

    problems = list(proto.validate_manifest(manifest))

    # 「强制显式 outputs」是本工具存在的理由，单独点名（不藏在 validate 清单里）：
    # 缺键必须给出一句作者一看就知道怎么改的话，而不是只说"缺字段"。
    if "kinds" in manifest:
        problems.insert(0, "outputs 未声明（用了旧键 kinds）：v2.0 强制显式 outputs，%s"
                       % proto.LEGACY_HINT)
    elif "outputs" not in manifest:
        problems.insert(0, "outputs 未显式声明：v2.0 强制必填（可用值 %s，"
                       "判定类 [\"verdict\"] / 处理类 [\"artifact\"]）"
                       % list(proto.OUTPUTS))
    return problems, manifest


def main(argv=None):
    parser = argparse.ArgumentParser(description="DAMDesk 行业包 manifest 门禁（pack_lint）")
    parser.add_argument("pack_dirs", nargs="+", help="包目录（含 manifest.json），可传多个")
    args = parser.parse_args(argv)

    overall = 0
    for pack_dir in args.pack_dirs:
        problems, manifest = lint_pack(pack_dir)
        label = "%s (id=%s)" % (pack_dir, (manifest or {}).get("id", "?"))
        if problems:
            overall = 1
            print("LINT FAIL %s" % label)
            for p in problems:
                print("  - %s" % p)
        else:
            print("LINT OK   %s outputs=%s"
                  % (label, (manifest or {}).get("outputs")))
    if overall:
        print("\npack_lint：存在违例（exit 1）")
    return overall


if __name__ == "__main__":
    sys.exit(main())
