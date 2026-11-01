#!/usr/bin/env bash
# CMP-JOB 跨层 E2E 复验（卡 t_54f92c17）——一条命令跑完 A/B/C 三段。
#
# 为什么要重跑而不是信父卡的数字：
#   父卡 t_6dc3aae4（run 账本）、t_ef24e923（执行列表 UI）在**各自那层**都有测试，
#   但他们用的真数据里 skipped 恒为 0（24 张完好真图），而「跳过 / 各次分开 / 认领遗留任务 /
#   迁移前老行」正是这次改造的核心语义。本脚本在一个进程、一个真 SQLite、真 HTTP、真 VL
#   （同网段 vLLM）上跑 5 次执行，把 API / 数据库 / DOM 三处事实互相校验。
#
# 段：
#   A) L0 门禁（scripts/test_gate.sh：cargo test + web build + web test + 分层校验）
#   B) 5 次真执行（cmp_job_e2e.py）：
#        run1 真跑完（含 2 张坏字节 → skipped=2、1 张失联 → 不进跳过）
#        run2 再跑一次（asset+group）→ 各次互不改写
#        run3 跑到一半取消 → cancelled>0，剩余留在队列
#        run4 跑到一半 kill -9 → 重启自愈 interrupted
#        run5 再跑 → 认领 run3/run4 遗留 pending，历史行数字不变
#      每步同时断言：API 字段、job_queue 实际行、恒等式 total=done+failed+skipped+cancelled
#   C) 界面实点（cmp_job_e2e_ui.mjs，chrome-headless-shell + CDP）：列表逐行对账 + 跳过原因 + 历史累计折叠
#
# 用法：
#   bash scripts/cmp_job_e2e.sh              # 全量（约 8-12 分钟，VL 是长尾）
#   CMP_SKIP_GATE=1 bash scripts/cmp_job_e2e.sh   # 跳过 A 段（已单独跑过门禁）
#   CMP_ROOT_HOST=/data/projects/soujpg-dam-desktop bash scripts/cmp_job_e2e.sh
#
# 退出码：0 全 PASS；非 0 = FAIL 段数。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
CONTAINER="${DAM_CONTAINER:-dam-dev}"
WT="/workspace/.worktrees/$(basename "$ROOT")"
PORT="${CMP_PORT:-18899}"
CDP_PORT="${CMP_CDP_PORT:-9249}"
WORK="${CMP_WORK:-/tmp/cmpjobE2E}"
# 并发隔离（同容器里多张卡同时跑 E2E 时会互相踢）：
#   构建目录共用 /cargo-target/dam 会被别人 cargo clean/重建覆盖（界面的 dist 路径跟着串台）；
#   更致命的是别人的 `pkill -f /cargo-target/dam/debug/app-web` 会直接把本进程杀掉。
# → CMP_TARGET_DIR 换构建目录；CMP_APP_BIN 把二进制另存到独立路径（不改目录也能躲开 pkill）。
TARGET_DIR="${CMP_TARGET_DIR:-/cargo-target/dam}"
BUILT_BIN="$TARGET_DIR/debug/app-web"
APP_BIN="${CMP_APP_BIN:-$BUILT_BIN}"
# pkill 模式用 [a]pp 写法，避免模式串本身把执行它的 shell 也匹配上（曾静默杀掉自己）
APP_KILL_PAT="$(dirname "$APP_BIN")/[a]pp-$(basename "$APP_BIN" | sed 's/^app-//')"
CHROME_KILL_PAT="chrome-headless-shell.*$(basename "$WORK")"
CHROME=/chrome/chrome-headless-shell-linux64/chrome-headless-shell
OUT="$ROOT/reports/evidence/CMP-JOB-E2E"
LOG="$ROOT/reports/logs/cmp_job_e2e.log"
mkdir -p "$OUT" "$ROOT/reports/logs"
: > "$LOG"
FAILED=0

say() { echo "$*" | tee -a "$LOG" 2>/dev/null || echo "$*"; }
gate() { local rc="$1"; local what="$2"; if [ "$rc" -ne 0 ]; then FAILED=$((FAILED + 1)); say "FAIL: $what"; else say "OK  : $what"; fi; }
# 容器以 root 身份写证据（bind mount）→ 收尾把属主还回宿主用户，方便 git 提交
chown_back() { sudo -n chown -R "$(id -u):$(id -g)" "$OUT" "$LOG" 2>/dev/null || true; }
run() { docker exec "$CONTAINER" bash -lc "$1"; }

say "########## A) L0 门禁 ##########"
if [ "${CMP_SKIP_GATE:-0}" = "1" ]; then
  say "CMP_SKIP_GATE=1：跳过（门禁已在本次任务内单独跑过，见 reports/CMP-JOB-E2E.md）"
else
  bash scripts/test_gate.sh 2>&1 | tee -a "$LOG" | tail -8
  gate "${PIPESTATUS[0]}" "L0 门禁（scripts/test_gate.sh）"
fi

say ""
say "########## B) 5 次真执行 + 三层对账（cmp_job_e2e.py） ##########"
# pkill 模式用 [a]pp 写法，避免模式串本身把执行它的 shell 也匹配上（曾静默杀掉自己）
run "pkill -f '$APP_KILL_PAT' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
run "cd $WT && export CARGO_TARGET_DIR=$TARGET_DIR && cargo build -p app-web > /tmp/cmpe2e-cargo.log 2>&1"
gate "$?" "app-web 构建（$BUILT_BIN）"
if [ "$APP_BIN" != "$BUILT_BIN" ]; then
  # 注意：$TARGET_DIR 是容器里的 docker volume（宿主看不到），cp 必须进容器做
  run "cp -f $BUILT_BIN $APP_BIN && chmod +x $APP_BIN"
  gate "$?" "app-web 二进制另存（$APP_BIN，躲开并发 pkill）"
else
  say "OK  : app-web 二进制（$APP_BIN）"
fi

GW=$(docker inspect "$CONTAINER" --format '{{range .NetworkSettings.Networks}}{{.Gateway}}{{end}}')
VL_BASE="${CMP_VL_BASE_URL:-http://$GW:8091/v1}"
say "VL 端点：$VL_BASE（同网段 vLLM，真模型路径）"

run "cd $WT && CMP_ROOT=$WT CMP_WORK=$WORK CMP_PORT=$PORT CMP_APP_BIN=$APP_BIN \
  CMP_VL_BASE_URL='$VL_BASE' CMP_VL_MODEL='${CMP_VL_MODEL:-qwen3.5-4b}' \
  python3 scripts/cmp_job_e2e.py --out $WT/reports/evidence/CMP-JOB-E2E" 2>&1 | tee -a "$LOG" | tail -70
gate "${PIPESTATUS[0]}" "B 段：5 次真执行 + 库/接口对账（no FAIL）"

say ""
say "########## C) 界面实点（CDP，DOM 断言） ##########"
# 起一个干净的服务实例给界面用（数据沿用 B 段那份真库）
run "pkill -f '$CHROME_KILL_PAT' 2>/dev/null; sleep 1; true" >/dev/null 2>&1
run "DAM_WEB_DATA=$WORK/data DAM_WEB_PORT=$PORT DAM_WEB_DIST=$WT/web/dist nohup $APP_BIN > $WORK/web-ui.log 2>&1 & echo started" >/dev/null
for i in $(seq 1 60); do
  run "curl -sf http://127.0.0.1:$PORT/api/compliance/jobs?limit=1 >/dev/null" && break
  sleep 1
done
gate "$?" "界面用服务实例就绪（127.0.0.1:$PORT）"

run "$CHROME --no-sandbox --disable-gpu --remote-debugging-port=$CDP_PORT --remote-allow-origins='*' --user-data-dir=$WORK/chrome-profile about:blank > $WORK/chrome.log 2>&1 & echo chrome" >/dev/null
sleep 3
CDPRE=$(run "curl -s http://127.0.0.1:$CDP_PORT/json/version | head -c 40" || true)
[ -n "$CDPRE" ] && gate 0 "headless chrome CDP 就绪（$CDP_PORT）" || gate 1 "CDP 未就绪（$WORK/chrome.log）"

run "cd $WT && CDP_PORT=$CDP_PORT APP_URL=http://127.0.0.1:$PORT \
  SNAPSHOT=$WT/reports/evidence/CMP-JOB-E2E/api-snapshot.json \
  OUT=$WT/reports/evidence/CMP-JOB-E2E/ui-assertions.json \
  SHOT_DIR=$WT/reports/evidence/CMP-JOB-E2E node scripts/cmp_job_e2e_ui.mjs" 2>&1 | tee -a "$LOG" | tail -40
gate "${PIPESTATUS[0]}" "C 段：界面 DOM 断言（UI_PROBE_RESULT OK）"

# 收尾：不留后台进程
run "pkill -f '$APP_KILL_PAT' 2>/dev/null; pkill -f '$CHROME_KILL_PAT' 2>/dev/null; true" >/dev/null 2>&1

chown_back
say ""
say "证据目录：$OUT"
ls -1 "$OUT" 2>/dev/null | tee -a "$LOG"
if [ "$FAILED" -eq 0 ]; then say "==== CMP-JOB E2E 全 PASS ===="; else say "==== $FAILED 段 FAIL ===="; fi
exit "$FAILED"
