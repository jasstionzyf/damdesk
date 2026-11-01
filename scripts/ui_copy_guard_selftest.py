#!/usr/bin/env python3
"""ui_copy_guard 自测（i18n-G，卡 t_25f3b8c8）。

为什么需要它：守卫扩面（i18n 词典值 / errors.<code> 词条也纳入扫描）如果只靠「跑一遍仓库是绿的」
证明，永远无法区分「守卫真的在扫」与「守卫把目录跳过了」。这里用临时夹具逐条反向验证：

  1. 词典**值**里出现「插件」→ 必须 FAIL（证明值被扫）
  2. 词典 **errors.<code> 词条值**里出现 verdict → 必须 FAIL（证明错误码词条同标准）
  3. 词典 **key** 里出现「插件」但值干净 → 必须 PASS（证明 key 豁免、不逼人改译名）
  4. 源码 JSX 文本出现「行业包」→ 必须 FAIL（守卫基本盘未退化）
  5. 扫描根不存在 → 退出码 2（守卫自身失效不能算通过）

退出码：0 = 全部符合预期；1 = 有反例不符（守卫失效或过度严格）。
用法：python3 scripts/ui_copy_guard_selftest.py
"""
from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GUARD = ROOT / "scripts" / "ui_copy_guard.py"

DICT_WITH_BAD_VALUE = """export default {
  // key 是中文原文；这里 violations 在值里
  '批量处理': '批量 插件 运行',
} as Record<string, string>
"""

DICT_WITH_BAD_ERROR_CODE = """export default {
  'errors.no_vector_index': 'No verdict index for the current model',
} as Record<string, string>
"""

DICT_WITH_BAD_KEY_ONLY = """export default {
  '插件与方案目录在这里改': 'Package and standards are managed here',
} as Record<string, string>
"""

TSX_WITH_BAD_TEXT = """export function Demo() {
  return <p>行业包未安装</p>
}
"""


def run(src: Path | None) -> tuple[int, str]:
    cmd = [sys.executable, str(GUARD), "--quiet"]
    if src is not None:
        cmd += ["--src", str(src)]
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.returncode, (r.stdout + r.stderr).strip()


def check_missing_src(name: str) -> bool:
    """扫描根不存在 → 必须 rc=2（守卫自己失效时不能静默算通过）。"""
    with tempfile.TemporaryDirectory() as td:
        gone = Path(td) / "not-there"
        rc, out = run(gone)
    ok = rc == 2
    print(f"{'PASS' if ok else 'FAIL'}  {name}  (rc={rc}, expect=2)")
    if not ok:
        print("      output:", out.replace("\n", "\n      ")[:400])
    return ok


def check(name: str, files: dict[str, str], expect_rc: int) -> bool:
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        for rel, content in files.items():
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(content, encoding="utf-8")
        rc, out = run(root)
    ok = rc == expect_rc
    print(f"{'PASS' if ok else 'FAIL'}  {name}  (rc={rc}, expect={expect_rc})")
    if not ok:
        print("      output:", out.replace("\n", "\n      ")[:400])
    return ok


def main() -> int:
    results = [
        check("词典值含「插件」必须 FAIL", {"i18n/dict.en-US.ts": DICT_WITH_BAD_VALUE}, 1),
        check("errors.<code> 词条值含 verdict 必须 FAIL", {"i18n/dict.en-US.ts": DICT_WITH_BAD_ERROR_CODE}, 1),
        check("词典 key 含「插件」但值干净必须 PASS（key 豁免）", {"i18n/dict.en-US.ts": DICT_WITH_BAD_KEY_ONLY}, 0),
        check("源码 JSX 文本含「行业包」必须 FAIL（基本盘）", {"pages/Demo.tsx": TSX_WITH_BAD_TEXT}, 1),
        check_missing_src("扫描根不存在必须 rc=2（守卫失效不能算过）"),
    ]
    bad = results.count(False)
    print(f"\n{len(results) - bad}/{len(results)} 通过")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
