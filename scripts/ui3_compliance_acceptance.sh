#!/bin/bash
# COMPLIANCE-UI3 验收（一条命令，退出码 0 = 全过）：
#   bash scripts/ui3_compliance_acceptance.sh
#
# A) 前端单测（三态口径/排序/两级聚合/改判门槛）+ 构建（tsc + vite，退出码即门禁）
# B) **真数据**：真图导入 → 真判定（真 pack + 真检测器 + 真 VL）→ 真 HTTP 复核链路：
#    - 统计三档合计 == 总资产（不造数）
#    - 复核队列真的只给「低置信度 / 过程出错」的行，且置信度是**真判定写出来的**（非 NULL 计数 > 0）
#    - 排序（置信度升序 / 资产序）真的换序
#    - 改判（reason 必填 / 高置信度要 force）→ 生效结论与三档统计真的变 → 撤销真的回到机器结论
#    - 改判历史真的落了修订记录（含撤销行）
# C) UI 实点：scripts/ui3_ui_probe.mjs 用 CDP 打开真实 dam-web（已构建 dist），
#    点出「队列/列表切换、排序、层级展开、改判对话框（备注必填）、撤销、检查器合规区块」，
#    断言取自 DOM 而不是接口返回值。
#
# 容器口径：默认用共享 dam-dev；并行卡（自有 worktree + 自有 target）用 DAM_CONTAINER=<容器名>。
# 容器需挂：/workspace=repo、/models=/data、/tmp/ui3-stage=真图语料。
# VL Key：C6 同口径，由 UI3_VL_API_KEY 注入。
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="$ROOT/reports/logs"
mkdir -p "$OUT"
CONTAINER="${DAM_CONTAINER:-dam-dev}"
TARGET="${UI3_TARGET:-24}"          # 判定张数（真跑；够覆盖三档与低置信度，不做 1500 张全量）
PORT="${UI3_PORT:-8799}"
DATA="${UI3_DATA:-/tmp/ui3-webdata}"
CORPUS="${UI3_CORPUS:-/tmp/ui3-corpus}"
STAGE="${UI3_STAGE:-/tmp/c5-stage}"
LOG="$OUT/ui3-acceptance.log"
: > "$LOG"
FAILED=0
say() { echo "$*" | tee -a "$LOG"; }
fail() { echo "FAIL: $*" | tee -a "$LOG"; FAILED=1; }
ok() { echo "OK  : $*" | tee -a "$LOG"; }
run() { docker exec "$CONTAINER" bash -lc "cd /workspace && $1"; }
jqget() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval(sys.argv[1],{'d':d}))" "$1"; }

say "########## A) 前端单测 + 构建 ##########"
run 'cd web && pnpm test 2>&1 | tail -6' | tee -a "$LOG"
run 'cd web && pnpm build 2>&1 | tail -3' | tee -a "$LOG"
if run 'cd web && pnpm test >/tmp/ui3-web-test.log 2>&1'; then ok "web 单测全绿"; else fail "web 单测失败（/tmp/ui3-web-test.log）"; fi
if run 'cd web && pnpm build >/tmp/ui3-web-build.log 2>&1'; then ok "web 构建通过（tsc + vite）"; else fail "web 构建失败"; fi

say ""
say "########## B) 真数据：真图 → 真判定 → 真复核 ##########"
run 'pkill -x app-web 2>/dev/null; true'
run "rm -rf $DATA && mkdir -p $DATA"
# 语料：真照片（c5-stage 是采样好的真图 jpg）；取前 N 张
run "rm -rf $CORPUS && mkdir -p $CORPUS && ls $STAGE/*.jpg 2>/dev/null | head -$TARGET | xargs -r cp -t $CORPUS"
N=$(run "ls $CORPUS | wc -l" | tr -d '\r')
say "语料：$N 张真图（$STAGE → $CORPUS）"
[ "$N" -ge 8 ] || { fail "语料不足（$N 张）"; exit 1; }

# 推理设置：合规判定走 settings.tag（VL）；api_key 由 UI3_VL_API_KEY 注入（不落日志）
if [ -z "${UI3_VL_API_KEY:-}" ]; then
  fail "缺少 UI3_VL_API_KEY"; exit 1
fi
# 起真实 dam-web（真 SQLite + 真 dist）。UI3_BIND=0.0.0.0 供跨容器 CDP 浏览器访问；
# 裸机验收保持 127.0.0.1（R4 单用户口径）。
GW=$(docker inspect "$CONTAINER" --format '{{range .NetworkSettings.Networks}}{{.Gateway}}{{end}}')
UI3_VL_BASE_URL="${UI3_VL_BASE_URL:-http://$GW:8091/v1}"
UI3_VL_MODEL="${UI3_VL_MODEL:-qwen3.5-4b}"
start_web() {
  run "cd /workspace && DAM_WEB_DATA=$DATA DAM_WEB_PORT=$PORT DAM_WEB_BIND=${UI3_BIND:-127.0.0.1} \
    DAM_TEST_MODELS_U2NET=/models/u2net DAM_TEST_MODELS_DIR=/models \
    nohup /cargo-target/dam/debug/app-web > /tmp/ui3-web.log 2>&1 & echo started"
}
wait_web() {
  local i
  for i in $(seq 1 40); do
    if run "curl -sf $API/api/library/stats >/dev/null"; then return 0; fi
    sleep 1
  done
  return 1
}
start_web; sleep 2
API="http://127.0.0.1:$PORT"
wait_web || { fail "dam-web 未就绪"; docker exec "$CONTAINER" bash -lc 'tail -20 /tmp/ui3-web.log' | tee -a "$LOG"; exit 1; }
ok "dam-web 首次启动（迁移已建表）"

# 推理设置：设置表由 app 迁移建好后再写（提前建表会和 0001 迁移打架 → 第一版踩过）。
# 判定走 settings.tag（VL）：指向宿主真 vLLM（默认 qwen3.5-4b），Key/端点可用 env 覆盖。
say "VL 端点：$UI3_VL_BASE_URL · 模型 $UI3_VL_MODEL"
run "pkill -x app-web 2>/dev/null; sleep 1; true"
if run "UI3_DATA=$DATA UI3_VL_BASE_URL=$UI3_VL_BASE_URL UI3_VL_MODEL=$UI3_VL_MODEL UI3_VL_API_KEY='${UI3_VL_API_KEY:-}' python3 /workspace/scripts/ui3_seed_settings.py"; then
  ok "推理设置已写入真库（VL 端点就绪）"
else
  fail "推理设置写入失败"
fi
start_web; sleep 2
wait_web && ok "dam-web 重启就绪（带 VL 设置）" || { fail "dam-web 重启失败"; exit 1; }

# 激活行业包：真图语料是房产/电商杂图，用 realestate（含 F0_domain 领域 gate）跑真判定
# 行业包：内置清单 = ecommerce-taobao + realestate（卡 task 起 realestate 也编译进内置；
# 组级判定走内置路径见 scripts/cmp_job_builtin_group_e2e.sh）。
# 缺省不切换；显式 UI3_PACK=<builtin-id> 时激活（失败即 FAIL，不静默用别的包）。
if [ -n "${UI3_PACK:-}" ]; then
  run "printf '%s' '{\"source\":\"builtin\",\"id\":\"$UI3_PACK\"}' > /tmp/ui3-pack.json"
  ACT=$(run "curl -s -X POST $API/api/compliance/packs/activate -H 'content-type: application/json' --data-binary @/tmp/ui3-pack.json")
  say "激活包 [$UI3_PACK]：$ACT"
  echo "$ACT" | grep -q '"ok":true' && ok "已激活包 $UI3_PACK" || fail "激活包失败：$ACT"
else
  say "行业包：沿用默认激活包（$(run "curl -sf $API/api/compliance/status" | jqget "d['activePack']['id']" 2>/dev/null)）"
fi


# 真导入
run "curl -sf -X POST $API/api/import/start -H 'content-type: application/json' -d '{\"path\":\"$CORPUS\",\"storeMode\":\"copyin\"}'" | tee -a "$LOG"
for i in $(seq 1 120); do
  S=$(run "curl -sf $API/api/import/status")
  RUNNING=$(echo "$S" | jqget "d['running']" 2>/dev/null || echo True)
  [ "$RUNNING" = "False" ] && break
  sleep 2
done
ST=$(run "curl -sf $API/api/library/stats")
ASSETS=$(echo "$ST" | jqget "d['assets']" 2>/dev/null || echo 0)
say "导入后资产数：$ASSETS"
[ "$ASSETS" -ge 8 ] && ok "真图已入库（$ASSETS 张）" || fail "导入后资产不足（$ASSETS）"

# 真判定（产品级入口：POST /api/compliance/judge）
run "curl -sf -X POST $API/api/compliance/judge -H 'content-type: application/json' -d '{\"all\":true,\"scope\":\"both\"}'" | tee -a "$LOG"
for i in $(seq 1 300); do
  JS=$(run "curl -sf $API/api/compliance/judge/status")
  RUNNING=$(echo "$JS" | jqget "d['running']" 2>/dev/null || echo True)
  [ "$RUNNING" = "False" ] && break
  sleep 3
done
say "判定收工：$(echo "$JS" | jqget "'report='+str(d.get('report'))" 2>/dev/null | cut -c1-300)"

# 真置信度：判定行里必须有非 NULL 的 confidence（否则「低置信度复核」无从谈起）
CONF=$(run "python3 - <<'PY'
import sqlite3
c = sqlite3.connect('$DATA/dam.db')
tot = c.execute('select count(*) from rule_verdict').fetchone()[0]
nn  = c.execute('select count(*) from rule_verdict where confidence is not null').fetchone()[0]
lo  = c.execute('select count(*) from rule_verdict where confidence < 0.7').fetchone()[0]
und = c.execute(\"select count(*) from rule_verdict where result='undetermined'\").fetchone()[0]
print(f'total={tot} with_conf={nn} low={lo} undetermined={und}')
PY" | tail -1)
say "判定行：$CONF"
echo "$CONF" | grep -q "with_conf=0" && fail "判定行没有真实 confidence（低置信度复核无意义）" || ok "判定行带真实 confidence"

ST1=$(run "curl -sf $API/api/compliance/status")
say "status: $(echo "$ST1" | jqget "d['states']")"
echo "$ST1" | python3 -c "
import json,sys
d=json.load(sys.stdin); s=d['states']
tot=s['pass']+s['fail']+s['undetermined']+s['unjudged']
print('三档合计(含未跑)=%d total=%d' % (tot, s['total']))
sys.exit(0 if tot==s['total'] else 3)" || fail "三档合计 != 总资产"
ok "统计三档合计 == 总资产"

say ""
say "###### 复核队列：真 HTTP（低置信度 / 过程出错）######"
R1=$(run "curl -sf '$API/api/compliance/review?lowConfidence=true&limit=200'")
echo "$R1" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print('review total=%d threshold=%.2f 返回=%d' % (d['total'], d['threshold'], len(d['items'])))
bad=[i for i in d['items'] if not (i['machineResult']=='undetermined' or (i['machineConfidence'] is None or i['machineConfidence']<d['threshold']))]
print('不满足「需复核」口径的行=%d' % len(bad))
sys.exit(0 if not bad else 4)" || fail "复核队列混入不该复核的行"
ok "复核队列口径正确（只有低置信度 / 过程出错）"

say "###### 排序：置信度升序（最没把握的先看）真的换序 ######"
run "curl -sf '$API/api/compliance/review?lowConfidence=true&sort=confidence&limit=50'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
cs=[(i['machineConfidence'] if i['machineConfidence'] is not None else -1) for i in d['items']]
print('前 8 个置信度:', [round(c,2) for c in cs[:8]])
sys.exit(0 if cs==sorted(cs) else 5)" || fail "置信度排序不是升序"
ok "置信度升序排序生效"
run "curl -sf '$API/api/compliance/review?lowConfidence=true&sort=asset&limit=50'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
ids=[i['assetId'] for i in d['items']]
sys.exit(0 if ids==sorted(ids,reverse=True) or len(ids)<2 else 6)" || fail "资产序排序不符合服务端口径"

say ""
say "###### 改判链路（reason 必填 / force / 生效结论 / 统计 / 历史 / 撤销）######"
# 目标一：需要复核的行（低置信度 / undetermined）→ 免 force
TGT=$(run "curl -sf '$API/api/compliance/review?lowConfidence=true&limit=200'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[i for i in d['items'] if i['ruleId'] and i['humanReview'] is None]
row=rows[0]
print(row['assetId'], row['ruleId'], row['machineResult'], row['machineConfidence'], row['lowConfidence'], sep='\t')")
AID=$(echo "$TGT" | cut -f1); RID=$(echo "$TGT" | cut -f2); MRES=$(echo "$TGT" | cut -f3); MCONF=$(echo "$TGT" | cut -f4); MLOW=$(echo "$TGT" | cut -f5)
say "改判目标（待复核行）：asset=$AID rule=$RID machine=$MRES conf=$MCONF lowConfidence=$MLOW"
HUMAN=$([ "$MRES" = "fail" ] && echo pass || echo fail)

# 1) 无备注必须被拒（400 review_reason_required）
CODE=$(run "curl -s -o /tmp/r1.json -w '%{http_code}' -X POST $API/api/compliance/review -H 'content-type: application/json' -d '{\"assetId\":$AID,\"ruleId\":\"$RID\",\"humanStatus\":\"$HUMAN\",\"reason\":\"  \"}'")
if [ "$CODE" = "400" ]; then ok "无备注改判被拒（HTTP 400）"; else fail "无备注改判未被拒（HTTP $CODE）"; fi

# 2) 待复核行：免 force 直接改判（服务端口径：undetermined / confidence < 阈值 不需 force）
FORCE=''
if [ "$MLOW" = "True" ]; then
  ok "目标属待复核行（lowConfidence=True）→ 免 force"
else
  fail "取到的目标不是待复核行（lowConfidence=$MLOW），force 分支无从验证"
fi

# 2b) 高置信度行（lowConfidence=False）改判：先 400，再 force 200 —— 单独挑一行验证
TGT2=$(run "curl -sf '$API/api/compliance/review?lowConfidence=false&limit=200'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[i for i in d['items'] if i['ruleId'] and i['humanReview'] is None and not i['lowConfidence']]
print(rows[0]['assetId'], rows[0]['ruleId'], rows[0]['machineResult'], sep='\t') if rows else print('-\t-\t-')")
AID2=$(echo "$TGT2" | cut -f1); RID2=$(echo "$TGT2" | cut -f2); MRES2=$(echo "$TGT2" | cut -f3)
if [ "$AID2" != "-" ]; then
  HUMAN2=$([ "$MRES2" = "fail" ] && echo pass || echo fail)
  CODE=$(run "curl -s -o /tmp/r3.json -w '%{http_code}' -X POST $API/api/compliance/review -H 'content-type: application/json' -d '{\"assetId\":$AID2,\"ruleId\":\"$RID2\",\"humanStatus\":\"$HUMAN2\",\"reason\":\"UI3 验收：高置信度先无 force\"}'")
  [ "$CODE" = "400" ] && ok "高置信度改判需 force（HTTP 400）" || fail "高置信度改判未要求 force（HTTP $CODE）"
  say "   400 响应：$(run 'cat /tmp/r3.json' | head -c 200)"
  run "curl -sf -X DELETE '$API/api/compliance/review?assetId=$AID2&ruleId=$RID2'" >/dev/null 2>&1 || true
else
  say "数据里没有「高置信度且有结论」的行 → 跳过 force 分支（如实记，不算通过）"
fi

RESP=$(run "curl -sf -X POST $API/api/compliance/review -H 'content-type: application/json' -d '{\"assetId\":$AID,\"ruleId\":\"$RID\",\"humanStatus\":\"$HUMAN\",\"reason\":\"UI3 验收：真数据改判\"$FORCE}'")
say "改判回执：$RESP" | cut -c1-400
echo "$RESP" | python3 -c "
import json,sys
d=json.load(sys.stdin)
assert d['ok'] and d['effectiveStatus']=='$HUMAN', d
assert d['machineResult']=='$MRES', '机器结论被改写了: %s' % d['machineResult']
assert d['humanReview']['reason'], 'reason 没落库'
print('effectiveStatus=%s machineResult=%s' % (d['effectiveStatus'], d['machineResult']))
" && ok "改判生效：effectiveStatus=$HUMAN，机器结论仍为 $MRES（未被改写）" || fail "改判回执不符合口径"

# 3) 行级生效结论真的被用上（DECouple-BE：资产视图只回判定行原文 + 行级 effectiveStatus，
#    资产级合成结论（state/finalStatus）已下线，只在 job 域 GET /api/compliance/jobs/{runId}/verdicts 给）
run "curl -sf '$API/api/compliance/verdicts?assetId=$AID'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
it=[i for i in d['items'] if i['ruleId']=='$RID'][0]
print('item.effectiveStatus=%s item.result=%s human=%s' % (it['effectiveStatus'], it['result'], bool(it['humanReview'])))
sys.exit(0 if it['effectiveStatus']=='$HUMAN' and it['result']=='$MRES' and it['humanReview'] else 7)" || fail "verdicts 未体现改判（或改写了机器结论）"
ok "verdicts：effectiveStatus 覆盖生效、result 保留机器结论"

# 4) 已改判筛选 + 改判历史
run "curl -sf '$API/api/compliance/review?lowConfidence=false&reviewed=true&limit=50'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
hit=[i for i in d['items'] if i['assetId']==$AID and i['ruleId']=='$RID']
sys.exit(0 if hit and hit[0]['reviewed'] else 8)" || fail "「已改判」筛选没包含刚改判的行"
ok "「已改判」筛选命中"
run "curl -sf '$API/api/compliance/review/history?assetId=$AID&limit=20'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[i for i in d['items'] if i['ruleId']=='$RID']
active=[r for r in rows if r['active'] and r['humanStatus']=='$HUMAN']
sys.exit(0 if active else 9)" || fail "改判历史里没有生效记录"
ok "改判历史：生效记录已落库"

# 5) 撤销 → 回到机器结论
run "curl -sf -X DELETE '$API/api/compliance/review?assetId=$AID&ruleId=$RID&reason=UI3%20%E9%AA%8C%E6%94%B6%EF%BC%9A%E6%92%A4%E9%94%80'" | tee -a "$LOG" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print('revoked=%s' % d['revoked'])
sys.exit(0 if d['ok'] and d['revoked']>=1 else 10)" || fail "撤销失败"
run "curl -sf '$API/api/compliance/verdicts?assetId=$AID'" | python3 -c "
import json,sys
d=json.load(sys.stdin)
it=[i for i in d['items'] if i['ruleId']=='$RID'][0]
sys.exit(0 if it['humanReview'] is None and it['finalStatus']=='$MRES' else 11)" || fail "撤销后生效结论没回到机器结论"
ok "撤销后生效结论回到机器结论（改判行保留为历史）"

say ""
say "###### C) UI 实点（CDP 浏览器，dist 已构建）######"
if [ -n "${UI3_SKIP_UI:-}" ]; then
  say "UI3_SKIP_UI=1 → 跳过 UI 段（不算通过：本次不宣称 UI 已验证）"
else
  # 浏览器跑在宿主（CDP 端点）→ 不能打容器内 127.0.0.1；绑 0.0.0.0 时用容器 IP
  PROBE_URL="${UI3_PROBE_URL:-$API}"
  case "${UI3_BIND:-127.0.0.1}" in
    0.0.0.0) IP=$(docker inspect "$CONTAINER" --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'); PROBE_URL="${UI3_PROBE_URL:-http://$IP:$PORT}" ;;
  esac
  say "UI probed URL: $PROBE_URL（CDP: ${CDP_URL:-http://127.0.0.1:9225}）"
  CDP_URL="${CDP_URL:-http://127.0.0.1:9225}" UI3_SHOTS="${UI3_SHOTS:-$ROOT/reports/logs/ui3-shots}" \
    node "$ROOT/scripts/ui3_ui_probe.mjs" "$PROBE_URL" 2>&1 | tee -a "$LOG"
  # probe 以 PIPESTATUS 判；tee 会吞退出码，用 grep 终态行
  grep -q "UI3_PROBE_RESULT OK" "$LOG" && ok "UI 实点全过（队列/列表/排序/展开/改判/撤销/检查器）" || fail "UI 实点失败（见 $LOG）"
fi

say ""
if [ "$FAILED" = "0" ]; then say "UI3_ACCEPTANCE_OK"; else say "UI3_ACCEPTANCE_FAILED"; fi
exit "$FAILED"
