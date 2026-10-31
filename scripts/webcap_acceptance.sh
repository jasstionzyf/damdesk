#!/bin/bash
# WEBCAP 验收（卡 task，一条命令，退出码 0 = 全过）：
#   DAM_CONTAINER=dam-webcap-build bash scripts/webcap_acceptance.sh
#
# 测什么：把「web 形态能力边界」从文档变成**机器可验**的开关（缺口 ①③ 收口）
#   A) 静态门禁：分层铁律（两态）+ release 构建 + 单测（能力开关 / 前置拦截 / 既有 e4 移动不回归）
#      + 前端 tsc + vitest + vite build
#   B) web 容器形态（真卷：/data 可写、/corpus 只读且**异设备**）：
#      · flags 取值（isDesktop=false / 磁盘类能力全关 / /corpus 归类 read_only_fs / 设备号不同）
#      · 「移动到目录」**前置拒绝** 400 + code（read_only_fs / cross_device /
#        not_writable_root / bad_path），且零磁盘副作用
#      · /data 卷内合法移动仍成功（② 的失败分类链不回归）
#   C) UI：CDP DOM 取证 —— 移动下拉/按钮 disabled、reason 非空可见、目录树行不可拖放
#   D) 桌面形态（`DAM_WEB_FORM=desktop` 钩子，与 scripts/tag_default_acceptance.sh 同口径）：
#      flags 全开 + 前置拦截不随形态失效
#   E) 回归（默认开，`WEBCAP_RUN_C6=0` 关闭）：scripts/c6_acceptance.sh exit 0
#
# 产出：reports/logs/webcap/（各段日志 + webcap-ui.json + webcap-ui.png + webcap-summary.json）
#
# 容器（独占，自有 worktree + 自有 CARGO_TARGET_DIR）需挂：
#   /workspace=repo、/data=可写卷、/corpus=只读且与 /data **不同文件系统**、/models
#
# ⚠️ 诚实门：桌面**真机**（Tauri 安装包）本脚本验不了 → 报告标「未验证」；
#    这里断言的是 DAM_WEB_FORM=desktop 钩子形态（同一套 app_web::is_desktop_form() 判定）。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="$ROOT/reports/logs/webcap"
mkdir -p "$OUT"
CONTAINER="${DAM_CONTAINER:-dam-webcap-build}"
TARGET_DIR=/cargo-target/dam
APP="$TARGET_DIR/release/app-web"
DATA=/data/webcap-webdata
LIB=/data/webcap-lib
PORT_WEB="${WEBCAP_PORT_WEB:-8891}"
PORT_DESK="${WEBCAP_PORT_DESK:-8892}"
CDP_PORT="${WEBCAP_CDP_PORT:-9229}"
CHROME_BIN="${WEBCAP_CHROME_BIN:-/chrome/chrome-headless-shell-linux64/chrome-headless-shell}"
RUN_C6="${WEBCAP_RUN_C6:-1}"
FAILED=0

run() { docker exec -e HTTP_PROXY= -e HTTPS_PROXY= -e http_proxy= -e https_proxy= \
  -e DAM_TEST_MODELS_DIR=/models -e DAM_TEST_MODELS_U2NET=/models/u2net "$CONTAINER" \
  bash -lc "set -o pipefail; cd /workspace && export CARGO_TARGET_DIR=$TARGET_DIR && $1"; }
# ⚠️ run() 里的 `set -o pipefail` 是硬要求：下面的门禁都用 `cmd | grep/tail` 过滤输出，
#    没有 pipefail 时管道退出码 = 最后一条 grep/tail（永远 0），**失败的测试会被记成 PASS**
#    （2026-10-17 本卡实战踩过：路由测试 FAILED 却 PASS 了门禁）。
gate() { local rc="$1" name="$2"; if [ "$rc" -eq 0 ]; then echo "PASS $name"; else echo "FAIL $name (rc=$rc)"; FAILED=$((FAILED + 1)); fi; }

start_web() { # $1=port  $2=额外 env（如 DAM_WEB_FORM=desktop）
  docker exec -d "$CONTAINER" bash -lc \
    "cd /workspace && DAM_WEB_DATA=$DATA DAM_WEB_DIST=/workspace/web/dist DAM_WEB_BIND=127.0.0.1 DAM_WEB_PORT=$1 ${2:-} $APP > /tmp/webcap-srv-$1.log 2>&1"
}
wait_health() {
  local port="$1"
  for _ in $(seq 1 60); do
    docker exec "$CONTAINER" curl -sf -o /dev/null "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && return 0
    sleep 1
  done
  docker exec "$CONTAINER" bash -lc "tail -20 /tmp/webcap-srv-$port.log" 2>/dev/null
  return 1
}
stop_web() { docker exec "$CONTAINER" pkill -f 'release/app-web' >/dev/null 2>&1; sleep 1; }
curl_in() { docker exec "$CONTAINER" curl -sf -o /dev/null "$@"; }

# CDP DOM 取证：$1=expect(disabled|enabled) $2=port $3=证据文件名（无扩展名）
ui_probe() {
  local expect="$1" port="$2" base="$3"
  if ! docker exec "$CONTAINER" test -x "$CHROME_BIN"; then
    echo "FAIL headless chrome 缺失：$CHROME_BIN（用 WEBCAP_CHROME_BIN 指到容器内的 chrome-headless-shell）" | tee -a "$OUT/webcap-c-ui.log"
    FAILED=$((FAILED + 1))
    return 1
  fi
  docker exec "$CONTAINER" pkill -f chrome-headless-shell >/dev/null 2>&1
  docker exec "$CONTAINER" rm -rf /tmp/chs-webcap >/dev/null 2>&1
  docker exec -d "$CONTAINER" "$CHROME_BIN" --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --remote-debugging-port="$CDP_PORT" --remote-debugging-address=127.0.0.1 \
    --user-data-dir=/tmp/chs-webcap about:blank
  local ok=1
  for _ in $(seq 1 40); do
    curl_in "http://127.0.0.1:$CDP_PORT/json/version" >/dev/null 2>&1 && { ok=0; break; }
    sleep 1
  done
  gate "$ok" "[$expect] headless chrome CDP 就绪（127.0.0.1:$CDP_PORT）"
  docker exec -e CDP_PORT="$CDP_PORT" -e APP_URL="http://127.0.0.1:$port" -e EXPECT="$expect" \
    -e OUT="/tmp/$base.json" -e SHOT="/tmp/$base.png" "$CONTAINER" \
    node /workspace/scripts/webcap_ui_probe.mjs 2>&1 | tee -a "$OUT/webcap-c-ui.log"
  gate "${PIPESTATUS[0]}" "[$expect] UI DOM 断言（$base）"
  docker exec "$CONTAINER" bash -lc "chmod -R a+r /tmp/$base.json /tmp/$base.png 2>/dev/null; true" >/dev/null 2>&1
  docker cp "$CONTAINER":"/tmp/$base.json" "$OUT/$base.json" >/dev/null 2>&1 || true
  docker cp "$CONTAINER":"/tmp/$base.png" "$OUT/$base.png" >/dev/null 2>&1 || true
  docker exec "$CONTAINER" pkill -f chrome-headless-shell >/dev/null 2>&1
}

echo "########## A) 静态门禁 / 单测 / 前端构建 ##########" | tee "$OUT/webcap-a-static.log"
run 'bash scripts/check_layers.sh 2>&1 | tail -1' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "分层铁律 check_layers（默认 feature 态）"
run 'cargo check -p svc-library --no-default-features 2>&1 | tail -2' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "分层/编译（--no-default-features 态）"
run 'cargo test -p svc-library --lib capabilities 2>&1 | grep -E "^test |test result|FAILED"' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "svc-library 能力开关单测（probe/路径/features/precheck）"
run 'cargo test -p app-web --lib 2>&1 | grep -E "test result|FAILED"' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "app-web 单测（含既有 e4 移动链路不回归）"
run 'cargo test -p app-web --test webcap_capabilities 2>&1 | grep -E "test result|FAILED|panicked"' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "app-web /api/capabilities + 前置拦截 路由测试"
run 'cargo build --release -p app-web 2>&1 | tail -2' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "release 构建 app-web"
run 'pnpm -C web exec tsc -b 2>&1 | tail -3' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "前端 tsc"
run 'pnpm -C web test 2>&1 | tail -6' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "前端 vitest（caps 判定口径）"
run 'pnpm -C web build 2>&1 | tail -3' | tee -a "$OUT/webcap-a-static.log"; gate "${PIPESTATUS[0]}" "前端 vite build"

echo "########## B) web 容器形态：flags + 前置拒绝 + 合法移动 ##########" | tee "$OUT/webcap-b-web.log"
run "rm -rf $DATA $LIB /tmp/webcap-outside && mkdir -p $DATA $LIB" | tee -a "$OUT/webcap-b-web.log"
gate "${PIPESTATUS[0]}" "重置验收数据目录（$DATA / $LIB）"

# 首启一次让 app-web 建库+迁移，再 seed（避免与运行中的连接抢写）
start_web "$PORT_WEB" ""
if wait_health "$PORT_WEB"; then
  echo "PASS 首启建库（$DATA/dam.db）" | tee -a "$OUT/webcap-b-web.log"
else
  echo "FAIL 首启建库（dam-web 未就绪）" | tee -a "$OUT/webcap-b-web.log"; FAILED=$((FAILED + 1)); exit 1
fi
stop_web
docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py --phase seed 2>&1 | tee -a "$OUT/webcap-b-web.log"
gate "${PIPESTATUS[0]}" "seed 真库（2 只读根资产 + 2 可写根资产，真磁盘文件）"

start_web "$PORT_WEB" ""
wait_health "$PORT_WEB" || { echo "FAIL web 形态服务未就绪" | tee -a "$OUT/webcap-b-web.log"; FAILED=$((FAILED + 1)); exit 1; }

docker exec "$CONTAINER" curl -s "http://127.0.0.1:$PORT_WEB/api/capabilities" > "$OUT/capabilities-web.json"
check_rc=$?; gate "$check_rc" "抓 web 形态 /api/capabilities 原始 JSON → webcap/capabilities-web.json"
docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py \
  --base "http://127.0.0.1:$PORT_WEB" --form web --phase flags 2>&1 | tee -a "$OUT/webcap-b-web.log"
gate "${PIPESTATUS[0]}" "web 形态 flags 取值（isDesktop/storeMode/writableRoots/roots/features/reasons）"

docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py \
  --base "http://127.0.0.1:$PORT_WEB" --form web --phase reject 2>&1 | tee -a "$OUT/webcap-b-web.log"
gate "${PIPESTATUS[0]}" "web 形态：移动到目录前置拒绝（4 类 code + 零副作用）"

echo "########## C) UI：CDP DOM 取证（web 形态：移动入口灰置 + reason + 目录不可拖放） ##########" | tee "$OUT/webcap-c-ui.log"
ui_probe disabled "$PORT_WEB" webcap-ui-web

docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py \
  --base "http://127.0.0.1:$PORT_WEB" --form web --phase move 2>&1 | tee -a "$OUT/webcap-b-web.log"
gate "${PIPESTATUS[0]}" "web 形态：/data 卷内合法移动仍成功（② 不回归）"
stop_web

echo "########## D) 桌面形态（DAM_WEB_FORM=desktop 钩子）：flags + 预检不随形态失效 ##########" | tee "$OUT/webcap-d-desktop.log"
run "rm -rf $DATA $LIB && mkdir -p $DATA $LIB" | tee -a "$OUT/webcap-d-desktop.log"
start_web "$PORT_DESK" "DAM_WEB_FORM=desktop"
if wait_health "$PORT_DESK"; then
  echo "PASS 桌面钩子形态服务就绪（DAM_WEB_FORM=desktop）" | tee -a "$OUT/webcap-d-desktop.log"
else
  echo "FAIL 桌面钩子形态服务未就绪" | tee -a "$OUT/webcap-d-desktop.log"; FAILED=$((FAILED + 1))
fi
stop_web
docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py --phase seed 2>&1 | tee -a "$OUT/webcap-d-desktop.log"
gate "${PIPESTATUS[0]}" "桌面钩子形态 seed"
start_web "$PORT_DESK" "DAM_WEB_FORM=desktop"
wait_health "$PORT_DESK" || { echo "FAIL 桌面钩子形态服务未就绪" | tee -a "$OUT/webcap-d-desktop.log"; FAILED=$((FAILED + 1)); }
docker exec "$CONTAINER" curl -s "http://127.0.0.1:$PORT_DESK/api/capabilities" > "$OUT/capabilities-desktop.json"
gate "$?" "抓桌面钩子形态 /api/capabilities 原始 JSON → webcap/capabilities-desktop.json"
docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py \
  --base "http://127.0.0.1:$PORT_DESK" --form desktop --phase flags 2>&1 | tee -a "$OUT/webcap-d-desktop.log"
gate "${PIPESTATUS[0]}" "桌面钩子形态 flags 取值（isDesktop=true / 磁盘类能力全开 / 无 reason）"
docker exec "$CONTAINER" python3 /workspace/scripts/webcap_http_asserts.py \
  --base "http://127.0.0.1:$PORT_DESK" --form desktop --phase reject 2>&1 | tee -a "$OUT/webcap-d-desktop.log"
gate "${PIPESTATUS[0]}" "桌面钩子形态：前置拦截同样生效（只读根/跨设备/库根外）"
# UI（桌面钩子形态）：入口可用；但只读库根目录仍不可拖放（③ 逐目录限定）
echo "---- 桌面钩子形态 UI（EXPECT=enabled）" | tee -a "$OUT/webcap-c-ui.log"
ui_probe enabled "$PORT_DESK" webcap-ui-desktop
stop_web

if [ "$RUN_C6" = "1" ]; then
  echo "########## E) 回归：scripts/c6_acceptance.sh（形态分支不能被打崩） ##########" | tee "$OUT/webcap-e-c6.log"
  DAM_CONTAINER="$CONTAINER" C6_WORK=/tmp/webcap-c6-work C6_WEBDATA=/tmp/webcap-c6-webdata \
    C6_PORT=8895 bash scripts/c6_acceptance.sh 2>&1 | tail -40 | tee -a "$OUT/webcap-e-c6.log"
  gate "${PIPESTATUS[0]}" "c6 回归（bash scripts/c6_acceptance.sh exit 0）"
else
  echo "SKIP c6 回归（WEBCAP_RUN_C6=0）" | tee "$OUT/webcap-e-c6.log"
fi

echo "########## 汇总 ##########" | tee "$OUT/webcap-summary.log"
PASSN=$(grep -rhoE "^PASS " "$OUT"/*.log | wc -l)
FAILN=$(grep -rhoE "^FAIL " "$OUT"/*.log | wc -l)
UIN=$(grep -cE "^PASS " "$OUT/webcap-c-ui.log" 2>/dev/null || echo 0)
python3 - "$OUT/webcap-summary.json" "$PASSN" "$FAILN" "$FAILED" "$UIN" <<'PY'
import json, sys
out, passed, failed, gates_failed, ui = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
s = {"asserts_pass": passed, "asserts_fail": failed, "gates_failed": gates_failed,
     "ui_dom_asserts_pass": ui,
     "honesty": "桌面真机（Tauri 安装包）未验证：本脚本只断言 DAM_WEB_FORM=desktop 钩子形态（同一套 is_desktop_form() 判定 + 同一份 web/dist）"}
open(out, "w").write(json.dumps(s, ensure_ascii=False, indent=2))
print(json.dumps(s, ensure_ascii=False))
checks = {
    "断言全 PASS（无 FAIL 行）": failed == 0 and passed > 0,
    "门禁全 PASS": gates_failed == 0,
}
for k, v in checks.items():
    print(f"{'PASS' if v else 'FAIL'}  {k}")
sys.exit(0 if all(checks.values()) else 1)
PY
RC=$?
if [ "$RC" -ne 0 ]; then
  echo "WEBCAP acceptance 失败（门禁失败 $FAILED 项）" >&2
  exit 1
fi
echo "WEBCAP acceptance 全部通过 — 产物 $OUT/（webcap-a-static.log / webcap-b-web.log / webcap-c-ui.log / webcap-d-desktop.log / webcap-ui-web.{json,png} / webcap-ui-desktop.{json,png} / webcap-summary.json）"
