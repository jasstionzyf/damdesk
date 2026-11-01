#!/usr/bin/env bash
# 几何组（N6a）自检：单测 → 语料验收（真网关）。
#
# 用法：
#   bash packs/plugins/imaging/tests/run_geometry_tests.sh              # 单测 + 语料（容器内跑 cargo 拉起网关）
#   bash packs/plugins/imaging/tests/run_geometry_tests.sh --unit       # 只跑单测（毫秒级，无网关/无语料）
#   bash packs/plugins/imaging/tests/run_geometry_tests.sh --e2e        # 只跑语料（需要网关）
#   CORPUS=/path/to/v1 bash ... --e2e                                   # 指定语料根
#
# 语料验收要在**容器**里跑（宿主无 cargo，模型根 `/models` 由容器挂载）：
#   docker exec -e CARGO_TARGET_DIR=/cargo-target -e DAM_MODELS_ROOT=/ dam-dev \
#     bash -lc 'cd /workspace/<repo/worktree> && bash packs/plugins/imaging/tests/run_geometry_tests.sh'
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$PACK/../../.." && pwd)"
PY="${PYTHON:-python3}"
CORPUS="${CORPUS:-${DAM_CORPUS:-}}"
MODE="${1:-all}"

fail=0

run_unit() {
  echo "== ① 单测（纯几何对 labels 真值 + 动作层对桩网关） =="
  ( cd "$ROOT" && "$PY" "$HERE/test_geometry.py" 2>&1 | tail -4 ) || fail=1
}

run_e2e() {
  echo
  echo "== ② 语料验收（真网关 + 真像素：resize 10 + smart_crop 10） =="
  if [ ! -f "$CORPUS/FROZEN" ]; then
    echo "SKIP：语料未就绪（设 CORPUS 或 DAM_CORPUS，当前：${CORPUS:-未设置}）。先跑 damdesk-testing 的 build_plugin_corpus.py --verify"
    return 0
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "SKIP：本机没有 cargo（请在 dam-dev 容器里跑，见脚本头注释）"
    return 0
  fi
  local out="$ROOT/reports/logs/geom_e2e.json"
  ( cd "$ROOT" && "$PY" "$HERE/corpus_geometry_e2e.py" --spawn-gateway \
        --corpus "$CORPUS" --json-out "$out" 2>&1 | tail -30 ) || fail=1
  echo "逐图结果：$out"
}

case "$MODE" in
  --unit) run_unit ;;
  --e2e)  run_e2e ;;
  all)    run_unit; run_e2e ;;
  *) echo "unknown arg: $MODE（--unit | --e2e | 省略=全部）"; exit 2 ;;
esac

echo
if [ "$fail" -eq 0 ]; then
  echo "几何组自检：全部通过"
else
  echo "几何组自检：有失败项（见上）"
fi
exit "$fail"
