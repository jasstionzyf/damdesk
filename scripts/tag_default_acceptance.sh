#!/usr/bin/env bash
# WIN-PRE2（DESKTOP-TAG-DEFAULT-1 / 卡 task）验收：
# 桌面形态首启默认打标 provider 必须指向 DeepSeek 公网端点（非容器口径），
# 且「真图 + 真 key」真实打标一次成功；web/容器形态默认不受影响。
#
# 用法（构建容器内；宿主亦可，需自备二进制与出网）：
#   DEEPSEEK_API_KEY=sk-xx [DAM_WEB_BIN=/path/app-web] [IMG=path/to/one.jpg] \
#     bash scripts/tag_default_acceptance.sh
#   DAM_WEB_BIN 默认 /cargo-target/debug/app-web（cargo build -p app-web 产物）
#   IMG 默认 tests/fixtures/dog.jpg
#
# 说明：卡片口径里的 /api/tag/providers 实际路由名是 /api/settings/tag/presets
# （W4 建的路由），本脚本按真实路由取证。
# 产物：reports/logs/tag-default-acceptance-<ts>/ 下全部原始响应；退出码 0 = 断言全过。
set -euo pipefail

BIN="${DAM_WEB_BIN:-/cargo-target/debug/app-web}"
KEY="${DEEPSEEK_API_KEY:?需要 DEEPSEEK_API_KEY}"
IMG="${IMG:-tests/fixtures/dog.jpg}"
PORT="${DAM_WEB_PORT:-18987}"
BASE="http://127.0.0.1:${PORT}"

[ -x "$BIN" ] || { echo "FAIL: 二进制不存在 $BIN（先 cargo build -p app-web）"; exit 1; }
[ -f "$IMG" ] || { echo "FAIL: 真图不存在 $IMG"; exit 1; }

TS=$(date +%Y%m%d-%H%M%S)
EV="reports/logs/tag-default-acceptance-$TS"
mkdir -p "$EV"
WORK=$(mktemp -d /tmp/winpre2-accept-XXXXXX)
DATA="$WORK/data"
IMGDIR="$WORK/imgs"
mkdir -p "$DATA" "$IMGDIR"
cp "$IMG" "$IMGDIR/"

mask() { sed -e "s/${KEY}/<REDACTED>/g" -e "s/sk-[A-Za-z0-9]\{8\}[A-Za-z0-9]*/sk-<REDACTED>/g"; }

SRV_PID=""
cleanup() { [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== WIN-PRE2 验收 @ $TS（证据目录 $EV）=="

# ---------- 1. 清空配置目录 → 首启（DAM_WEB_FORM=desktop 复刻桌面壳 seeding 形态） ----------
# 剥代理 env：engine-tag 默认走 env 代理，而构建容器的代理指向已停用的 infer-host:11700
# （2026-10-16 起 V2Ray 在 infer-host 本机）；infer-host 直连 api.deepseek.com 实测可达（/v1/models 401）。
rm -rf "$DATA"; mkdir -p "$DATA"
env -u HTTP_PROXY -u HTTPS_PROXY -u http_proxy -u https_proxy \
DAM_WEB_FORM=desktop DAM_WEB_DATA="$DATA" DAM_WEB_PORT="$PORT" \
DAM_WEB_BIND=127.0.0.1 DAM_WEB_TOKEN= DAM_WEB_DIST=/nonexistent \
  "$BIN" >"$EV/server.log" 2>&1 &
SRV_PID=$!

for i in $(seq 1 60); do
  curl -sf "$BASE/api/healthz" >/dev/null 2>&1 && break
  kill -0 "$SRV_PID" 2>/dev/null || { echo "FAIL: server 启动即退"; mask <"$EV/server.log"; exit 1; }
  sleep 1
done
echo "ok: 首启 server up（清空目录 $DATA）"

# ---------- 2. 断言桌面默认 provider = deepseek 公网端点 ----------
curl -sf "$BASE/api/settings/inference" | mask >"$EV/01-inference-fresh.json"
python3 - "$EV/01-inference-fresh.json" <<'EOF'
import json, sys
j = json.load(open(sys.argv[1]))["settings"]["tag"]
assert j["provider"] == "deepseek", j
assert j["base_url"] == "https://api.deepseek.com/v1", j
assert j["model"] == "deepseek-flash", j
assert j["enabled"] is True, j
assert j["api_key"] == "", "Key 必须默认空（引导用户自填）"
assert "host.docker.internal" not in j["base_url"], "禁止容器口径"
print("ok: 默认 provider=deepseek / https://api.deepseek.com/v1 / deepseek-flash / enabled / key 空")
EOF

# ---------- 3. presets 原始返回含 deepseek（卡片口径 /api/tag/providers = 该路由） ----------
curl -sf "$BASE/api/settings/tag/presets" | mask >"$EV/02-tag-presets.json"
python3 - "$EV/02-tag-presets.json" <<'EOF'
import json, sys
ps = json.load(open(sys.argv[1]))
if isinstance(ps, dict): ps = ps.get("presets", [])
dp = [p for p in ps if p.get("id") == "deepseek"]
assert dp, ps
assert dp[0]["base_url"] == "https://api.deepseek.com/v1"
assert "deepseek-flash" in dp[0]["models"]
print("ok: presets 含 deepseek 条目")
EOF
echo "---- presets 原始返回 ----"; cat "$EV/02-tag-presets.json"; echo

# ---------- 4. Key 未填 → /api/tag/status provider.hasKey=false（UI 引导依据） ----------
curl -sf "$BASE/api/tag/status" | mask >"$EV/03-status-nokey.json"
python3 - "$EV/03-status-nokey.json" <<'EOF'
import json, sys
p = json.load(open(sys.argv[1]))["provider"]
assert p["configured"] is True, p      # base_url+model 预填
assert p["hasKey"] is False, p         # Key 未填 → TagTasks/设置页显式引导
assert p["provider"] == "deepseek", p
print("ok: status.provider configured=true hasKey=false（触发「去填 API Key」引导）")
EOF

# ---------- 5. 贴真实 key（配置写路径） ----------
# 注：app-web 纯 web 构建按设计拒绝本地 ONNX embedder（local_infer_disabled），
# 桌面默认 active_embedder=siglip2 在该二进制下 PUT 不通 → 写路径切 web 构建支持的
# openai-compat-embedding 配置（仅 embed 段；tag 段原样 + api_key）。真实桌面壳
# （local-infer 构建）无此限制。tag 默认断言（步骤 2-4）在写入前已完成，不受影响。
curl -sf "$BASE/api/settings/inference" | python3 -c "
import json, sys
j = json.load(sys.stdin); s = j['settings']
if s['active_embedder'].startswith(('siglip2', 'local')) or s['active_embedder'] not in ('openai-compat-embedding',):
    s['active_embedder'] = 'openai-compat-embedding'
    s['embedding'] = {'base_url': 'http://host.docker.internal:8090/v1', 'api_key': '', 'model': 'qwen3-vl-embedding-2b', 'dims': 0, 'timeout_secs': 120, 'proxy': ''}
s['tag']['api_key'] = sys.argv[1]
print(json.dumps(s))" "$KEY" >"$WORK/settings-put.json"
HTTP=$(curl -s -o "$WORK/put-resp.json" -w '%{http_code}' -X PUT "$BASE/api/settings/inference" \
  -H 'content-type: application/json' --data-binary @"$WORK/settings-put.json")
if [ "$HTTP" != "200" ]; then echo "FAIL: PUT settings http=$HTTP"; mask <"$WORK/put-resp.json"; exit 1; fi
mask <"$WORK/put-resp.json" >"$EV/04-settings-saved.json"
curl -sf "$BASE/api/tag/status" | mask >"$EV/05-status-withkey.json"
python3 - "$EV/05-status-withkey.json" <<'EOF'
import json, sys
p = json.load(open(sys.argv[1]))["provider"]
assert p["hasKey"] is True, p
print("ok: key 写入后 hasKey=true（读写回路成立）")
EOF

# ---------- 6. 测试连接（文本探针，走 engine-tag openai_compat provider） ----------
# /api/settings/tag/test 收的是 TagSettings 本体（settings-put.json 里的 .tag 段）
python3 -c "
import json, sys
print(json.dumps(json.load(open(sys.argv[1]))['tag']))" "$WORK/settings-put.json" >"$WORK/tag-put.json"
curl -sS -X POST "$BASE/api/settings/tag/test" -H 'content-type: application/json' \
  --data-binary @"$WORK/tag-put.json" -m 180 | mask >"$EV/06-test-connection.json"
python3 - "$EV/06-test-connection.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
assert r.get("ok") is True, r
print("ok: 测试连接通过:", {k: r[k] for k in ("model", "latencyMs") if k in r})
EOF
echo "---- test 连接原始返回 ----"; cat "$EV/06-test-connection.json"; echo

# ---------- 7. 真图真实打标：导入 1 张 → 模板打标队列 → 回读标签 ----------
curl -sf -X POST "$BASE/api/import/start" -H 'content-type: application/json' \
  -d "{\"path\": \"$IMGDIR\", \"storeMode\": \"reference\", \"autoTag\": false}" \
  >"$EV/07-import-start.json"
for i in $(seq 1 120); do
  RUNNING=$(curl -sf "$BASE/api/import/status" | python3 -c "import json,sys; print(json.load(sys.stdin)['running'])")
  [ "$RUNNING" = "False" ] && break
  sleep 1
done
curl -sf "$BASE/api/import/status" | mask >"$EV/08-import-status.json"
python3 - "$EV/08-import-status.json" <<'EOF'
import json, sys
st = json.load(open(sys.argv[1]))
prog = st.get("progress") or {}
done = prog.get("doneCount", prog.get("done", 0))
assert done and int(done) >= 1, st
print("ok: 导入完成 doneCount =", done)
EOF

ASSET_ID=$(curl -sf "$BASE/api/library/assets?limit=5" | python3 -c "
import json, sys
j = json.load(sys.stdin)
items = j.get('items') or j.get('assets') or []
assert items, j
print(items[0]['assetId'])")
echo "ok: assetId=$ASSET_ID"

curl -sf -X POST "$BASE/api/tag/start" -H 'content-type: application/json' \
  -d '{"mode": "pending"}' | mask >"$EV/09-tag-start.json"
for i in $(seq 1 240); do
  st=$(curl -sf "$BASE/api/tag/status")
  STATE=$(echo "$st" | python3 -c "
import json, sys
q = json.load(sys.stdin)['template']
print('DONE' if not q['running'] and (q.get('progress') or {}).get('doneCount', 0) >= 1 else 'WAIT')")
  [ "$STATE" = "DONE" ] && { echo "$st" | mask >"$EV/10-tag-status-final.json"; break; }
  sleep 2
done
[ "${STATE:-}" = "DONE" ] || { echo "FAIL: 打标队列 480s 未完成"; echo "$st" | mask; exit 1; }

# E5 两段式：模板打标先落 tag_proposal 提案 —— 原始推理结果从这里回读
curl -sf "$BASE/api/tag/review?limit=50" | mask >"$EV/11-tag-review-proposals.json"
echo "---- 真图打标原始响应（/api/tag/review 提案回读，deepseek 真实推理产物）----"
cat "$EV/11-tag-review-proposals.json"; echo
python3 - "$EV/11-tag-review-proposals.json" <<'EOF'
import json, sys
j = json.load(open(sys.argv[1]))
groups = j.get("items") or []
rows = [r for g in groups for r in (g.get("items") or [])]
vals = [r for r in rows if r.get("value") or r.get("name")]
assert vals, f"提案为空: {j}"
models = {str(r.get("model")) for r in vals}
assert any("deepseek" in m for m in models), f"提案 model 不含 deepseek: {models}"
kinds = {r.get("kind") for r in vals}
print("ok: 提案含 deepseek 产物, model =", models, ", kinds =", kinds, ", rows =", len(vals))
EOF

# 提案确认入 tag 表（E5 第二段），回读 asset 标签作为最终证据
curl -sf -X POST "$BASE/api/tag/review/commit" -H 'content-type: application/json' \
  -d "{\"assetIds\": [$ASSET_ID]}" | mask >"$EV/12-review-commit.json"
curl -sf "$BASE/api/library/assets/$ASSET_ID/tags" | mask >"$EV/13-asset-tags.json"
echo "---- 确认后 asset 标签回读（assetId=$ASSET_ID）----"; cat "$EV/13-asset-tags.json"; echo
python3 - "$EV/13-asset-tags.json" <<'EOF'
import json, sys
rows = json.load(open(sys.argv[1]))
assert isinstance(rows, list) and rows, rows
models = {str(r.get("model")) for r in rows if r.get("origin") == "auto"}
assert any("deepseek" in m for m in models), f"auto 标签 model 不含 deepseek: {rows}"
print("ok: asset 标签 origin=auto 落库, model =", models)
EOF

echo
echo "PASS: WIN-PRE2 全部断言通过（原始证据在 $EV，key 已打码）"
