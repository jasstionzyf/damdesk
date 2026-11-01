#!/usr/bin/env bash
# damdesk 测试门禁 L0（快速层）：任何 E2E / 真机测试之前必须先跑通。
# 用法：
#   bash scripts/test_gate.sh            # 全量 L0（deps + 资源 + web + cargo + 分层校验）
#   bash scripts/test_gate.sh --fast     # 跳过 cargo（前端 + 分层校验，秒级）
#   bash scripts/test_gate.sh --no-web   # 跳过前端单测/构建（cargo 仍需要 web/dist，会按需补建）
#   bash scripts/test_gate.sh --skip-heavy # 轻量档：cargo 跳过 c3_corpus_500（u2net 500 张真图重模型
#                                        # suite，单次约 6min，与大多数改动无关）；纯数据层/接口层卡用这档，
#                                        # 报告里必须写明「c3 未跑（--skip-heavy）」，收尾卡仍跑全量档。
#   bash scripts/test_gate.sh --coverage # 覆盖率档：用覆盖率跑替代 cargo/web 单测，并判「改动文件 100%」
#                                        # （内部调 scripts/test_coverage.sh，耗时分钟级；提交前/收尾跑）
# 退出码：0 全绿；1 任一步失败（FAIL 行 + 该步错误行/末尾输出一起贴出）。
# 输出尾部有 PASS/FAIL 汇总表（含每步耗时）；单步完整日志落 reports/logs/test_gate/。
# 前置自愈（干净 worktree 直接可跑）：web/node_modules 缺失 → pnpm install；
#   crates/app-tauri/resources/models/{4 个模型} 缺失 → 软链到共享模型目录（env DAM_MODELS_SRC；容器内默认 /models）。
# 铁律：L0 全绿才允许进入 L1 集成测试 / L2 真机 E2E；L0 总耗时超过 10 分钟必须拆分（--coverage 档另算，见报告）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FAST=0; NO_WEB=0; NO_CARGO=0; COVERAGE=0; SKIP_HEAVY=0; COV_BASE="${DAM_COV_BASE:-main}"
while [ $# -gt 0 ]; do
  case "$1" in
    --fast) FAST=1 ;;
    --no-web) NO_WEB=1 ;;
    --no-cargo) NO_CARGO=1 ;;
    --skip-heavy) SKIP_HEAVY=1 ;;
    --coverage) COVERAGE=1 ;;
    --cov-base) shift; COV_BASE="${1:-main}" ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1"; exit 2 ;;
  esac
  shift
done
[ "$FAST" = 1 ] && NO_CARGO=1

# 构建/测试统一进容器（宿主 gpu7 无 pnpm/node；cargo 也走容器以免污染宿主）
IN_CONTAINER=0
if command -v docker >/dev/null 2>&1 && docker ps --format '{{.Names}}' | grep -qx dam-dev \
   && docker exec dam-dev true >/dev/null 2>&1; then
  IN_CONTAINER=1
fi
# 容器内路径映射（宿主仓库 → /workspace；worktree 挂在其下，前缀映射自动跟随）
CROOT="/workspace"
if [ "$IN_CONTAINER" = 1 ]; then
  # 宿主仓根 → 容器 /workspace 前缀映射：从 git 公共目录反推宿主仓根（不写死任何机器路径）
  COMMON="$(git -C "$ROOT" rev-parse --git-common-dir 2>/dev/null || true)"
  [ -n "$COMMON" ] || COMMON="$ROOT/.git"
  case "$COMMON" in /*) ;; *) COMMON="$ROOT/$COMMON" ;; esac
  HOST_BASE="$(dirname "$COMMON")"
  case "$ROOT" in "$HOST_BASE"/*) CROOT="/workspace${ROOT#"$HOST_BASE"}" ;; esac
fi

# tauri 打包资源：6 个模型（含 DT5 的 OCR det/rec）在 gitignore 里，干净 worktree 从共享模型目录软链过来
if [ "$IN_CONTAINER" = 1 ]; then MODELS_SRC="${DAM_MODELS_SRC:-/models}"; else MODELS_SRC="${DAM_MODELS_SRC:-}"; fi
MODELS="siglip2-base-p224-onnx dinov3-vits16-onnx u2net realesr-general-x4v3 ppocrv6-medium-det ppocrv6-medium-rec"

LOGDIR="$ROOT/reports/logs/test_gate"
mkdir -p "$LOGDIR"
rm -f "$LOGDIR"/*.log 2>/dev/null

RESULTS=(); FAILED=0

_sh() { # 在容器内或宿主执行一条命令（$1 = 命令串）；GATE_LOCAL=1 强制走宿主
  if [ "${GATE_LOCAL:-0}" = 1 ]; then
    ( cd "$ROOT" && bash -lc "$1" )
  elif [ "$IN_CONTAINER" = 1 ]; then
    docker exec -e CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/cargo-target}" dam-dev bash -lc "cd $CROOT && $1"
  else
    ( cd "$ROOT" && bash -lc "$1" )
  fi
}

run() { # run <name> <cmd...>
  local name="$1"; shift
  local key log t0 t1 rc
  key=$(printf '%s' "$name" | tr ' ' '_')
  log="$LOGDIR/$key.log"
  t0=$(date +%s)
  echo ""
  echo "──────── ▶ $name"
  _sh "$*" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  t1=$(date +%s)
  if [ "$rc" -eq 0 ]; then
    RESULTS+=("$(printf '%-22s %-5s %ss' "$name" "PASS" "$((t1-t0))")")
  else
    RESULTS+=("$(printf '%-22s %-5s %ss  (exit %s, 日志 %s)' "$name" "FAIL" "$((t1-t0))" "$rc" "${log#"$ROOT"/}")")
    echo ""
    echo "  ✗ $name 失败，该步错误行（无 error 行则取末尾 20 行）："
    local errs
    errs=$(grep -nE '^(error|Error|FAIL|panicked|test result: FAILED|Caused by:|  ✗)' "$log" 2>/dev/null | head -n 12)
    if [ -n "$errs" ]; then
      echo "$errs" | sed 's/^/    | /'
      echo "    | …完整日志: ${log#"$ROOT"/}"
    else
      tail -n 20 "$log" | sed 's/^/    | /'
    fi
    FAILED=$((FAILED+1))
  fi
  return 0
}

T0=$(date +%s)
echo "damdesk L0 门禁 @ $ROOT"
echo "  执行环境: $([ "$IN_CONTAINER" = 1 ] && echo "容器 dam-dev:$CROOT" || echo "宿主机")"

# ---- 前置自愈 1：前端依赖 ----
if [ "$NO_WEB" = 0 ] && [ ! -d "$ROOT/web/node_modules" ]; then
  run "deps install" "pnpm -C web install --prefer-offline"
fi

# ---- 前置自愈 2：app-tauri 打包资源（模型体积大，不入库，软链共享目录）----
if [ "$NO_CARGO" = 0 ]; then
  # 判「已就绪」必须在**执行环境**里判：模型根两个环境不同（宿主= DAM_MODELS_SRC、容器 /models），
  # 旧写法用宿主的 `[ -e ]` 判 → 容器里指向 /data1 的悬空链接也算「就绪」，
  # 于是 app-tauri 的 build script `resources/models/**/*` glob 匹配不到 → cargo test 挂在 build script
  # 上（假 PASS，直到 cargo 那一步才炸）。这里把探测也送进执行环境。
  missing=$(_sh "for m in $MODELS; do [ -d crates/app-tauri/resources/models/\$m ] || printf '%s ' \"\$m\"; done")
  if [ -n "$missing" ] && [ "$NO_CARGO" = 0 ] && [ -z "$MODELS_SRC" ]; then
    echo "FAIL(前置): tauri resources/models 缺失（$missing），且未设 DAM_MODELS_SRC（宿主机共享模型目录）"; exit 1
  fi
  if [ -n "$missing" ]; then
    run "tauri resources" "mkdir -p crates/app-tauri/resources/models && for m in $MODELS; do [ -e crates/app-tauri/resources/models/\$m ] || ln -sfn $MODELS_SRC/\$m crates/app-tauri/resources/models/\$m; done && ls -l crates/app-tauri/resources/models | tail -n +2"
  else
    echo ""
    echo "（tauri resources 已就绪，跳过软链）"
  fi
fi

# ---- 前端：先构建（app-tauri 的 build script 依赖 ../../web/dist）----
if [ "$NO_WEB" = 0 ]; then
  run "web build" "pnpm -C web build"
  [ "$COVERAGE" = 0 ] && run "web test" "pnpm -C web test"
else
  echo ""
  if [ "$NO_CARGO" = 0 ] && [ ! -f "$ROOT/web/dist/index.html" ]; then
    run "web dist (tauri 依赖)" "pnpm -C web build"
  else
    echo "（--no-web：跳过前端构建与单测）"
  fi
fi

# ---- 覆盖率档：用覆盖率跑替代 cargo test / web test，并对改动文件判 100% ----
if [ "$COVERAGE" = 1 ]; then
  # 覆盖率档自带容器切换与工具自愈（llvm-tools-preview / cargo-llvm-cov / node_modules / models），
  # 因此在宿主机上调起，避免「容器里再套一次 docker exec」；日志由它自己落在 reports/logs/test_coverage/。
  GATE_LOCAL=1 run "coverage" "bash scripts/test_coverage.sh --base $COV_BASE"
fi

# ---- Rust 工作区：全量单测 + 集成测试（quiet，只留失败摘要）----
if [ "$COVERAGE" = 1 ]; then
  :
elif [ "$NO_CARGO" = 0 ] && [ "$SKIP_HEAVY" = 1 ]; then
  run "cargo test (--skip-heavy)" "cargo test --workspace --quiet -j 4 -- --skip c3_corpus_500_detect_and_smart_crop_recheck"
elif [ "$NO_CARGO" = 0 ]; then
  run "cargo test" "cargo test --workspace --quiet -j 4"
else
  echo ""
  echo "（--fast / --no-cargo：跳过 cargo test）"
fi

# ---- N11 架构守卫：内置核心功能（打标/查重/EXIF·OCR/向量/检索）不得被包成插件 ----
# 执行体 = crates/pack-host/tests/builtin_guard.rs（cargo test --workspace 已包含同一用例；
# 这里单跑一次是为了让门禁日志里留下 5 个扫描面的数字，出问题时不用翻别的日志）。
if [ "$COVERAGE" = 1 ] || [ "$NO_CARGO" = 1 ]; then
  echo ""
  echo "（--fast / --no-cargo / --coverage：跳过 builtin guard 单跑；覆盖档的 cargo test 仍含该用例）"
else
  run "builtin guard" "cargo test -p pack-host --test builtin_guard -- --nocapture"
fi

# ---- 分层铁律（PLAN.md §2.1）----
run "layers check" "bash scripts/check_layers.sh"

# ---- dampack 发布守卫（卡 t_b5f22e5e）：同 id@version 换内容必须被出包步骤拦下 ----
# 反向验证用真实出包命令跑（内容变+版本不变 → 必须红），2 秒级；台账 packs/pack-content-lock.json。
# 「与真实历史交付包互认」的几个用例在没 deliver 目录的机器上自动 SKIP，其余照跑。
run "pack release guard" "bash scripts/pack_release_guard_selftest.sh"

T1=$(date +%s)
echo ""
echo "══════════════ L0 门禁汇总 ══════════════"
if [ "${#RESULTS[@]}" -gt 0 ]; then
  for r in "${RESULTS[@]}"; do echo "  $r"; done
fi
echo "  --------------------------------------"
echo "  总耗时: $((T1-T0))s   失败项: $FAILED"
if [ "$FAILED" -gt 0 ]; then
  echo "  结论: FAIL —— 禁止进入 E2E / 真机（修好再跑本脚本）"
  exit 1
fi
echo "  结论: PASS —— 可以进入 L1 集成测试 / L2 真机 E2E"
exit 0
