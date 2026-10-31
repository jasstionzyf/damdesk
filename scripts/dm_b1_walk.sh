#!/usr/bin/env bash
# DM-B1（task / KI-36）手验一条命令：起真后端 → 装一个**映射外**能力 → 跑真 DOM 断言。
#
#   docker exec dam-dev bash -lc 'bash /workspace/.worktrees/task/scripts/dm_b1_walk.sh'
#
# 做什么（顺序 = 用户操作顺序，全部真接口 / 真落库 / 真 DOM 文本）：
#   1. 全新数据目录起 `app-web`（不带任何旧账本，验证 fresh 库形态）；
#   2. 造 `com.soujpg.pack.upscale_demo`（仓库里没有的 id）并走 `POST /api/compliance/packs/install` 真装；
#   3. `GET /api/compliance/packs` 打印该行原文（name/summary 从哪来有据可查）；
#   4. 跑 `web/tests/live/packNames.live.test.tsx`：导航 tab / 能力面板 / 详情页 / 卸载确认
#      四处 DOM 文本断言（断言期望值取自第 3 步的接口原文）。
#
# 退出码：0 全绿 / 1 任一步失败。KEEP=1 保留服务（默认收尾 kill）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA=${DATA:-/tmp/dm-b1-walk}
PORT=${PORT:-18791}
BIN=${BIN:-/cargo-target/dm-b1/debug/app-web}
DEMO_ID=com.soujpg.pack.upscale_demo
export DAM_WEB_BASE="http://127.0.0.1:$PORT"
export DAM_DEMO_ID="$DEMO_ID"

log() { printf '\n== %s\n' "$*"; }

[ -x "$BIN" ] || { echo "缺 $BIN：先 cargo build -p app-web（CARGO_TARGET_DIR=/cargo-target/dm-b1）" >&2; exit 1; }
[ -f "$ROOT/web/dist/index.html" ] || { echo "缺 web/dist：先 pnpm -C web build" >&2; exit 1; }

log "1) 全新数据目录起 app-web :$PORT"
pkill -f "DAM_WEB_PORT=$PORT" 2>/dev/null || true
rm -rf "$DATA"; mkdir -p "$DATA"
export DAM_WEB_DATA="$DATA" DAM_WEB_PORT="$PORT" DAM_WEB_BIND=127.0.0.1 DAM_WEB_DIST="$ROOT/web/dist"
nohup "$BIN" >"$DATA/server.log" 2>&1 &
echo $! >"$DATA/server.pid"
ok=0
for _ in $(seq 1 100); do
  if curl -sf "$DAM_WEB_BASE/api/healthz" >/dev/null 2>&1; then ok=1; break; fi
  sleep 0.2
done
if [ "$ok" != 1 ]; then echo "服务没起来；tail log:"; tail -30 "$DATA/server.log"; exit 1; fi
echo "up pid=$(cat "$DATA/server.pid") port=$PORT data=$DATA"

log "2) 造映射外能力并真装"
python3 "$ROOT/scripts/dm_b1_demo_pack.py" --out "$DATA/$DEMO_ID.dampack"
curl -sS -X POST "$DAM_WEB_BASE/api/compliance/packs/install" \
  -H 'content-type: application/json' \
  -d "{\"path\":\"$DATA/$DEMO_ID.dampack\",\"activate\":true}" \
  | tee "$DATA/install.json"
echo

log "3) 列表接口原文（该行）"
curl -sS "$DAM_WEB_BASE/api/compliance/packs" | python3 -c '
import json,sys
d=json.load(sys.stdin)
row=next((p for p in d["plugins"] if p["id"]=="com.soujpg.pack.upscale_demo"), None)
print(json.dumps(row, ensure_ascii=False, indent=2))
'

log "4) 真 DOM 断言（导航 / 面板 / 详情 / 卸载确认）"
( cd "$ROOT/web" && npx vitest run --config vitest.live.config.ts tests/live/packNames.live.test.tsx )
rc=$?

if [ "${KEEP:-0}" != 1 ]; then
  kill "$(cat "$DATA/server.pid")" 2>/dev/null || true
fi
exit $rc
