#!/usr/bin/env bash
# 房产包自检入口：单测（46 例）+ 协议一致性（conformance 12/12）+ 语料矩阵冒烟。
#
#   bash packs/plugins/realestate/tests/run_tests.sh          # 单测 + conformance
#   MATRIX=live bash packs/plugins/realestate/tests/run_tests.sh   # 再加真 VL 语料矩阵
#
# 退出码非 0 即失败；conformance 只在仓内存在时跑（包可独立分发）。
set -euo pipefail

PACK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$PACK_DIR/../../.." && pwd)"

echo "== 1/3 单测（ruleset / imagedec / 插件全链路）=="
( cd "$PACK_DIR" && python3 -m unittest discover -s tests -p 'test_*.py' )

echo
echo "== 2/3 协议一致性（conformance）=="
if [[ -f "$REPO_DIR/tools/pack-conformance/run.py" ]]; then
  python3 "$REPO_DIR/tools/pack-conformance/run.py" --plugin "$PACK_DIR"
else
  echo "跳过：仓内没有 tools/pack-conformance（包独立分发场景）"
fi

echo
echo "== 3/3 语料矩阵冒烟（stub VL；真 VL 用 MATRIX=live）=="
PROVIDER="${MATRIX:-stub}"
CORPUS="${CORPUS:-/data}"
if [[ -d "$CORPUS" ]]; then
  python3 "$PACK_DIR/tests/matrix_corpus.py" --provider "$PROVIDER" \
    --corpus "$CORPUS" --groups prop-suite
else
  echo "跳过：语料目录不存在（$CORPUS）"
fi

echo
echo "OK: 房产包自检全绿"
