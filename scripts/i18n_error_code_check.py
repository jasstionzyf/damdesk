#!/usr/bin/env python3
"""i18n-E2 门禁：错误码 ↔ 前端词典一一对应（卡 t_63c5b793）。

比对两边：
  A. `crates/app-web/src/error_codes.rs` 的 `codes::ALL` 数组（后端覆盖的稳定码全集）
  B. `web/src/i18n/dict.en-US.ts` 里的 `errors.<code>` 词条键

退出码 0 = 双向 0 缺口；1 = 有缺口（打印明细）。
幂等可重跑；只读两个文件，不依赖 cargo/pnpm。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RS = REPO / "crates" / "app-web" / "src" / "error_codes.rs"
DICT = REPO / "web" / "src" / "i18n" / "dict.en-US.ts"


def codes_from_rs(text: str) -> list[str]:
    # ALL 数组里是**常量标识符**；再解析每个 `pub const NAME: &str = "值";` 拿到码值
    m = re.search(r"pub const ALL:\s*&\[&str\]\s*=\s*&\[(.*?)\];", text, re.S)
    if not m:
        sys.exit("FATAL: error_codes.rs 里找不到 codes::ALL 数组")
    idents = re.findall(r"\b([A-Z][A-Z0-9_]*)\b", m.group(1))
    consts = dict(re.findall(r'pub const ([A-Z][A-Z0-9_]*):\s*&str\s*=\s*"([^"]+)"', text))
    unknown = [i for i in idents if i not in consts]
    if unknown:
        sys.exit(f"FATAL: ALL 引用了未定义常量: {unknown}")
    return [consts[i] for i in idents]


def dict_error_keys(text: str) -> list[str]:
    keys = re.findall(r"'(errors\.[a-z0-9_]+)'", text)
    return sorted({k.removeprefix("errors.") for k in keys})


def main() -> int:
    rs_codes = codes_from_rs(RS.read_text(encoding="utf-8"))
    dict_codes = dict_error_keys(DICT.read_text(encoding="utf-8"))

    dup_rs = sorted({c for c in rs_codes if rs_codes.count(c) > 1})
    missing_in_dict = sorted(set(rs_codes) - set(dict_codes))
    missing_in_rs = sorted(set(dict_codes) - set(rs_codes))

    print(f"error_codes.rs codes::ALL : {len(rs_codes)} codes")
    print(f"dict.en-US.ts errors.*    : {len(dict_codes)} keys")
    if dup_rs:
        print(f"DUP in ALL                : {dup_rs}")
    if missing_in_dict:
        print(f"MISSING in dict (0 漏要求) : {missing_in_dict}")
    if missing_in_rs:
        print(f"EXTRA in dict (后端无此码) : {missing_in_rs}")
    if dup_rs or missing_in_dict or missing_in_rs:
        return 1
    print("OK: 双向一一对应，0 漏 0 多 0 重复")
    return 0


if __name__ == "__main__":
    sys.exit(main())
