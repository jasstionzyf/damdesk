#!/usr/bin/env bash
# 叠加组（N6c）自检：单测 → 语料验收（真网关 + 真像素）。
#
# 用法：
#   bash packs/plugins/imaging/tests/run_overlay_tests.sh            # 单测 + 语料（容器内跑 cargo 拉起网关）
#   bash packs/plugins/imaging/tests/run_overlay_tests.sh --unit     # 只跑单测（毫秒级，无网关/无语料）
#   bash packs/plugins/imaging/tests/run_overlay_tests.sh --e2e      # 只跑语料（需要网关）
#   CORPUS=/path/to/v1 bash ... --e2e                                # 指定语料根
#
# 语料验收要在**容器**里跑（宿主无 cargo；语料需对容器可见，网关只收容器内绝对路径）：
#   docker exec -e CARGO_TARGET_DIR=/cargo-target/<wt> -e DAM_MODELS_ROOT=/ dam-dev \
#     bash -lc 'cd /workspace/<worktree> && bash packs/plugins/imaging/tests/run_overlay_tests.sh'
#
# 语料不在容器里时，先把 `labels/watermark.json` + `process/watermark/` 拷进
# `reports/logs/e2e-corpus/v1/`（该目录 gitignore），再用 `CORPUS=reports/logs/e2e-corpus/v1`。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$PACK/../../.." && pwd)"
PY="${PYTHON:-python3}"
CORPUS="${CORPUS:-${DAM_CORPUS:-/data/damdesk-test-corpus/v1}}"
MODE="${1:-all}"

fail=0

run_unit() {
  echo "== ① 单测（参数/计划/行形状/改名冲突，桩网关） =="
  ( cd "$ROOT" && "$PY" "$HERE/test_overlay.py" 2>&1 | tail -3 ) || fail=1
}

run_e2e() {
  echo
  echo "== ② 语料验收（真网关 + 真像素：watermark 10 + rename 10） =="
  if [ ! -f "$CORPUS/labels/watermark.json" ]; then
    echo "SKIP：语料未就绪（$CORPUS）。先跑 damdesk-testing 的 build_plugin_corpus.py --verify"
    return 0
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "SKIP：本机没有 cargo（请在 dam-dev 容器里跑，见脚本头注释）"
    return 0
  fi
  local out="$ROOT/reports/logs/overlay_e2e.json"
  ( cd "$ROOT" && "$PY" "$HERE/corpus_overlay_e2e.py" --spawn-gateway \
        --corpus "$CORPUS" --json-out "$out" 2>&1 | tail -40 ) || fail=1
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
  echo "叠加组自检：全部通过"
else
  echo "叠加组自检：有失败项（见上）"
fi
exit "$fail"
