#!/usr/bin/env bash
# L1 旅程门禁（卡 t_c117a719）：离线产品旅程 5 条链路的独立档。
#
# 为什麼不进 L0：L0 是「秒级~分钟级」快速门禁（cargo 全量 + web build/test + 分层校验），
# 旅程链路属跨服务状态机（导入/管线/判定/导出/回收站），按 t_acccdbc9 的裁决挂 L1 独立档：
# 默认门禁不跑，需要时显式跑本脚本。
#
# 用法：
#   bash scripts/journey_acceptance.sh            # 跑一遍 5 条链路 + 打印分阶段耗时
#   bash scripts/journey_acceptance.sh --repeat 3 # 连续跑 3 遍（可重复性证据）
#   bash scripts/journey_acceptance.sh --sabotage # 反向验证（破坏 -> 必须红 -> 还原）
#   DAM_TEST_CONTAINER=dam-journey-build bash scripts/journey_acceptance.sh
#
# 挂进 scripts/test_gate.sh（L0）的方式（等该脚本入库后可用）：
#   --journey) JOURNEY=1 ;;                      # 参数解析里加一行
#   [ "$JOURNEY" = 1 ] && run "journey (L1)" "bash scripts/journey_acceptance.sh"
#
# 判据：5 个用例全 PASS，且每条链路 ≤120s（脚本会自己核，超了判 FAIL）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER="${DAM_TEST_CONTAINER:-dam-dev}"
REPEAT=1
SABOTAGE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --repeat) REPEAT="${2:-3}"; shift ;;
    --repeat=*) REPEAT="${1#*=}" ;;
    --sabotage) SABOTAGE=1 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1"; exit 2 ;;
  esac
  shift
done
case "$REPEAT" in
  ''|*[!0-9]*) echo "--repeat 需要数字"; exit 2 ;;
esac

IN_CONTAINER=0
if command -v docker >/dev/null 2>&1 && docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
  IN_CONTAINER=1
fi
CROOT="/workspace"
if [ "$IN_CONTAINER" = 1 ]; then
  # 工作树容器（如 dam-journey-build）把本工作树直接挂在 /workspace → 用 env 覆盖
  case "$ROOT" in
    *) CROOT="${DAM_REPO_IN_CONTAINER:-}"
       if [ -z "$CROOT" ]; then
         COMMON="$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null || true)"
         [ -n "$COMMON" ] || COMMON="$ROOT/.git"
         case "$COMMON" in /*) ;; *) COMMON="$ROOT/$COMMON" ;; esac
         HB="$(dirname "$COMMON")"
         case "$ROOT" in "$HB"/*) CROOT="/workspace${ROOT#"$HB"}" ;; *) CROOT="/workspace" ;; esac
       fi ;;
  esac
fi

run_in() {
  if [ "$IN_CONTAINER" = 1 ]; then
    docker exec -e CARGO_TARGET_DIR=/cargo-target "$CONTAINER" bash -lc "cd $CROOT && $*"
  else
    ( cd "$ROOT" && eval "$*" )
  fi
}

T0=$(date +%s)
FAILED=0
for i in $(seq 1 "$REPEAT"); do
  echo ""
  echo "══════════ 旅程链路 第 $i / $REPEAT 遍 ══════════"
  S=$(date +%s)
  OUT=$(run_in "cargo test -p dev-e2e --test journey -- --nocapture 2>&1")
  RC=$?
  E=$(date +%s)
  echo "$OUT" | grep -E "^test |JOURNEY_.*SUMMARY|test result" || true
  echo "  本遍耗时: $((E - S))s"
  if [ "$RC" -ne 0 ]; then
    echo "  结论: FAIL"
    echo "$OUT" | grep -E "panicked" -A 3 | head -20
    FAILED=$((FAILED + 1))
    continue
  fi
  # 单条链路 ≤120s 铁律（卡片要求），从各链路自己的 SUMMARY 里核对
  while IFS= read -r line; do
    ms=$(sed -E 's/.*"elapsed_ms":([0-9]+).*/\1/' <<<"$line")
    name=$(sed -E 's/^JOURNEY_([0-9]+)_SUMMARY.*/\1/' <<<"$line")
    if [ "${ms:-0}" -gt 120000 ]; then
      echo "  链路 $name 超时: ${ms}ms > 120000ms"
      FAILED=$((FAILED + 1))
    fi
  done < <(echo "$OUT" | grep -E "^JOURNEY_[0-9]+_SUMMARY")
done

if [ "$SABOTAGE" = 1 ]; then
  echo ""
  echo "══════════ 反向验证（破坏 → 必须红 → 还原） ══════════"
  if run_in "python3 scripts/journey_sabotage.py"; then
    echo "  反向验证: PASS"
  else
    echo "  反向验证: FAIL"
    FAILED=$((FAILED + 1))
  fi
fi

T1=$(date +%s)
echo ""
echo "══════════════ L1 旅程门禁汇总 ══════════════"
echo "  重复遍数: $REPEAT   总耗时: $((T1 - T0))s   失败项: $FAILED"
if [ "$FAILED" -gt 0 ]; then
  echo "  结论: FAIL"
  exit 1
fi
echo "  结论: PASS —— 5 条链路全绿（离线、可重复），可进 L2 真机批次"
exit 0
