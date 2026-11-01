#!/usr/bin/env bash
# dampack 发布守卫的回归门（卡 t_b5f22e5e）——「内容变 + 版本不变」的出包步骤必须红。
# 用法：bash scripts/pack_release_guard_selftest.sh    （退出码 0 全过 / 1 有用例失败 / 2 用法错）
# 内容全在 scripts/pack_release_guard_selftest.py；这里只是一层壳，让 test_gate.sh 与人都能一条命令跑。
set -uo pipefail
cd "$(dirname "$0")/.."
command -v python3 >/dev/null 2>&1 || { echo "[FAIL] 缺 python3（守卫是纯 stdlib 脚本，不需要第三方包）" >&2; exit 2; }
exec python3 scripts/pack_release_guard_selftest.py "$@"
