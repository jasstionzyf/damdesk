#!/usr/bin/env bash
# N11 架构守卫一键入口：**内置核心功能（批量打标 / 查重去重 / EXIF·OCR / 向量索引与语义检索）
# 不得被包成插件**。判据执行体是 Rust 集成测试 `crates/pack-host/tests/builtin_guard.rs`
# （5 个扫描面 + 3 条正向断言，命中打印 规则/文件:行号/命中的词）。
#
# 用法：
#   bash scripts/builtin_guard.sh             # 正向：必须 PASS（违规 0 项）
#   bash scripts/builtin_guard.sh --reverse   # 反向验证：临时把 engine-tag 塞进
#                                             # packs/plugins/realestate/manifest.json →
#                                             # 守卫必须 FAIL，然后自动还原。
#                                             退出码 0 = 「反向验证成立」（先红后绿都对）
#   bash scripts/builtin_guard.sh --keep       # 只跑正向，不清理日志目录
# 退出码：0 通过；1 不通过；2 参数错。
#
# 为什么要有反向验证：守卫只在「真的能红」的时候才算守卫。把内置引擎临时塞进 manifest
# 若仍然绿，说明扫描面是空的或 assert 是摆设 —— 那比没这条守卫更糟（假绿）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE=forward
while [ $# -gt 0 ]; do
  case "$1" in
    --reverse) MODE=reverse ;;
    --keep)    : ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1"; exit 2 ;;
  esac
  shift
done

# 统一进容器跑（宿主 gpu7 无 cargo；与 scripts/test_gate.sh 同一套容器判定）
IN_CONTAINER=0
if command -v docker >/dev/null 2>&1 && docker ps --format '{{.Names}}' | grep -qx dam-dev \
   && docker exec dam-dev true >/dev/null 2>&1; then
  IN_CONTAINER=1
fi
CROOT="/workspace"
if [ "$IN_CONTAINER" = 1 ]; then
  # 宿主仓根 → 容器 /workspace 前缀映射：从 git 公共目录反推宿主仓根（不写死任何机器路径）
  COMMON="$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null || true)"
  [ -n "$COMMON" ] || COMMON="$ROOT/.git"
  case "$COMMON" in /*) ;; *) COMMON="$ROOT/$COMMON" ;; esac
  HOST_BASE="$(dirname "$COMMON")"
  case "$ROOT" in "$HOST_BASE"/*) CROOT="/workspace${ROOT#"$HOST_BASE"}" ;; esac
fi

LOGDIR="$ROOT/reports/logs/builtin_guard"
mkdir -p "$LOGDIR"

_run() { # 跑守卫，输出 tee 到 $1；返回 cargo 退出码
  local log="$1"
  if [ "$IN_CONTAINER" = 1 ]; then
    docker exec -e CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/cargo-target}" \
      -e HTTP_PROXY= -e HTTPS_PROXY= -e http_proxy= -e https_proxy= \
      dam-dev bash -lc "cd $CROOT && cargo test -p pack-host --test builtin_guard -- --nocapture 2>&1" \
      | tee "$log"
  else
    ( cd "$ROOT" && cargo test -p pack-host --test builtin_guard -- --nocapture 2>&1 ) | tee "$log"
  fi
  return "${PIPESTATUS[0]}"
}

if [ "$MODE" = forward ]; then
  echo "──── ▶ N11 内置功能架构守卫（正向）"
  _run "$LOGDIR/forward.log"; rc=$?
  echo ""
  if [ "$rc" -eq 0 ] && grep -q '违规 0 项 → PASS' "$LOGDIR/forward.log"; then
    grep -E '^\[N11\]|^  R[1-5]|^  P[12]' "$LOGDIR/forward.log"
    echo "PASS  builtin guard（正向）—— 日志 $LOGDIR/forward.log"
    exit 0
  fi
  echo "FAIL  builtin guard（正向）—— 见 $LOGDIR/forward.log"
  grep -E '^  R[1-5] |^  P[12] |panicked' "$LOGDIR/forward.log" | head -20
  exit 1
fi

# ---------------------------------------------------------------------------
# 反向验证：先把违规塞进去，守卫必须红；再还原，守卫必须绿
# ---------------------------------------------------------------------------
MANIFEST="$ROOT/packs/plugins/realestate/manifest.json"
BACKUP="$LOGDIR/realestate-manifest.json.bak"
if [ ! -f "$MANIFEST" ]; then
  echo "FAIL 反向验证：$MANIFEST 不存在"
  exit 2
fi
cp "$MANIFEST" "$BACKUP"
restore() { cp "$BACKUP" "$MANIFEST"; }
trap restore EXIT INT TERM

python3 - "$MANIFEST" <<'PYEOF'
import sys
p = sys.argv[1]
text = open(p, encoding="utf-8").read()
needle = '"capabilities": ['
i = text.index(needle) + len(needle)
open(p, "w", encoding="utf-8").write(text[:i] + '\n    "engine-tag",' + text[i:])
PYEOF
echo "（已临时把 \"engine-tag\" 塞进 packs/plugins/realestate/manifest.json 的 capabilities）"

echo "──── ▶ N11 内置功能架构守卫（反向：必须 FAIL）"
_run "$LOGDIR/reverse.log"; rc=$?
restore
echo ""

if [ "$rc" -eq 0 ]; then
  echo "FAIL  反向验证不成立：塞了 engine-tag 守卫仍然绿 —— 扫描面空了或 assert 是摆设（日志 $LOGDIR/reverse.log）"
  exit 1
fi
hits=$(grep -c '^  R1 ' "$LOGDIR/reverse.log" || true)
if ! grep -q 'packs/plugins/realestate/manifest.json:[0-9]*.*engine-tag' "$LOGDIR/reverse.log"; then
  echo "FAIL  反向验证不成立：守卫红了，但没有报到「realestate/manifest.json:行号 → engine-tag」"
  grep -E '^  R[1-5] |panicked' "$LOGDIR/reverse.log" | head -20
  exit 1
fi
echo "反向证据（守卫确实红，且指到文件:行号）："
grep -E '^  R[1-5] |^\[N11\] 违规|panicked' "$LOGDIR/reverse.log" | head -20

echo ""
echo "──── ▶ 还原后复跑（必须重新 PASS）"
_run "$LOGDIR/forward-after.log"; rc2=$?
if [ "$rc2" -ne 0 ] || ! grep -q '违规 0 项 → PASS' "$LOGDIR/forward-after.log"; then
  echo "FAIL  还原后守卫没有恢复 PASS（日志 $LOGDIR/forward-after.log）"
  exit 1
fi
echo "PASS  反向验证成立：塞 engine-tag → FAIL（R1 命中 $hits 处 + 能力白名单）；还原 → PASS"
exit 0
