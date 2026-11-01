#!/bin/bash
# DEPRECATED（DECouple-BE，卡 t_59942f03，2026-10-31）：
# 本脚本验证的「图库列表按合规状态过滤（/api/library/assets?compliance=）」与「资产级合成结论字段
# （finalStatus / assetHumanReview / state）」已在用户裁决下**下线** —— 图库域与 job 域解耦。
# 判定维度查询改走 GET /api/compliance/jobs/{runId}/verdicts；现行门禁用例见
# crates/app-web/tests/decouple_be_verdicts.rs。脚本保留作历史证据，断言已失效。
# C6 验收（一条命令，退出码 0 = 全过）：
#   bash scripts/c6_acceptance.sh
#
# A) 单测/回归：core-storage 判定仓库+反馈（trait/SQL 实现）+ svc-library 合规视图
#    （状态口径 / SQL↔Rust 对账 / 队列 / 反馈 CSV）+ 导出报告汇总口径未回归
#    + app-web C6 路由 e2e（HTTP 真跑）+ 分层铁律 + 前端双构建（tsc + vite，C6 新页面/组件）
# B) 真图 e2e：40 张真图跑一遍真合规管线（c5_e2e，真 pack/真 VL）→ 用产出的库起 dam-web，
#    用 HTTP 打 C6 全链路：状态分桶 ↔ 库视图各状态过滤计数**真数据对账**、待人工队列、
#    对真 pending_human 项写「误判/处置」反馈、反馈 CSV 行数与内容核对、切换行业包后
#    历史判定不丢（SQL 直查计数）。
# C) 硬断言：真数据下「过滤计数 == 分桶计数」「反馈 CSV 行数 == 反馈条数」「换包不删判定」
#
# 产出：reports/logs/c6-a-*.log、reports/logs/c6-e2e/（驱动日志+状态）、c6-b-http.log、c6-summary.json
#
# 容器：默认共享 dam-dev；并行卡（自有 worktree + 自有 target）用 DAM_CONTAINER=<容器名> 覆盖
# （c3/c4/c5_acceptance.sh 同口径）。容器需挂：/workspace=repo、/models=/data1/dam-models。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="$ROOT/reports/logs"
mkdir -p "$OUT"
CONTAINER="${DAM_CONTAINER:-dam-dev}"
# 语料下限：c5_e2e 的 prepare_corpus 内部 clamp 到 ≥40 张（低于 40 会与驱动自校验打架）
TARGET="${C6_TARGET:-40}"
WORK="${C6_WORK:-/tmp/c6-e2e-work}"
WEBDATA="${C6_WEBDATA:-/tmp/c6-webdata}"
PORT="${C6_PORT:-8791}"

run() { docker exec -e DAM_TEST_MODELS_U2NET=/models/u2net -e DAM_TEST_MODELS_DIR=/models "$CONTAINER" bash -lc "cd /workspace && export CARGO_TARGET_DIR=/cargo-target/dam && $1"; }

echo "########## A) 单测/回归/前端构建 ##########" | tee "$OUT/c6-a-tests.log"
run 'cargo test -p core-storage --lib compliance 2>&1 | grep -E "test result|FAILED"' | tee -a "$OUT/c6-a-tests.log"
run 'cargo test -p svc-library --lib compliance_view 2>&1 | grep -E "^test |test result|FAILED"' | tee -a "$OUT/c6-a-tests.log"
run 'cargo test -p svc-library --lib export 2>&1 | grep -E "test result|FAILED"' | tee -a "$OUT/c6-a-tests.log"
run 'cargo test -p app-web --test c6_compliance_routes 2>&1 | grep -E "test result|FAILED"' | tee -a "$OUT/c6-a-tests.log"
run 'bash scripts/check_layers.sh 2>&1 | tail -1' | tee -a "$OUT/c6-a-tests.log"
run 'cargo check -p svc-library --no-default-features 2>&1 | tail -1' | tee -a "$OUT/c6-a-tests.log"
run 'pnpm -C web build 2>&1 | tail -3' | tee -a "$OUT/c6-a-tests.log"

echo "########## B) $TARGET 张真图合规管线 → dam-web HTTP 全链路 ##########" | tee "$OUT/c6-b-e2e.log"
# 语料：宿主机只读采样产物 /tmp/c5-stage（C5 已转好 jpg；容器看不到宿主 /data1）
if ! docker exec "$CONTAINER" test -d /tmp/c5-stage; then
  if [ -d /tmp/c5-stage ]; then
    echo "[stage] 容器内无语料 → docker cp /tmp/c5-stage（宿主机只读采样产物）"
    docker cp /tmp/c5-stage "$CONTAINER":/tmp/c5-stage >/dev/null
  else
    echo "[stage] 宿主机也没有 /tmp/c5-stage —— 先跑 scripts/c5_e2e_driver.sh 的采样段或 scripts/sample_real_photos.sh" >&2
    exit 2
  fi
fi
if [ -z "${C6_VL_API_KEY:-}" ]; then
  C6_VL_API_KEY=$(grep -A2 '^  deepseek:' ~/.hermes/profiles/soujpg/config.yaml 2>/dev/null \
    | grep 'api_key:' | head -1 | sed 's/.*api_key: *//' | tr -d '"' || true)
fi
if [ -z "${C6_VL_API_KEY:-}" ]; then
  echo "需要 C6_VL_API_KEY（或 hermes soujpg profile 的 deepseek key）" >&2
  exit 2
fi

echo "[build] 容器内 release 构建 c5_e2e + c6_pending + app-web（增量）"
run 'cargo build --release -p dev-e2e --bin c5_e2e --bin c6_pending 2>&1 | tail -1'
run 'cargo build --release -p app-web 2>&1 | tail -1'

cat > /tmp/c6_run.sh <<EOF
cd /workspace
export CARGO_TARGET_DIR=/cargo-target/dam
export C5_WORK='$WORK'
export C5_OUT='/workspace/reports/logs/c6-e2e'
export C5_TARGET='$TARGET'
export C5_WORKERS='${C6_WORKERS:-6}'
export C5_STAGE_DIR=/tmp/c5-stage
export C5_VL_BASE_URL='${C6_VL_BASE_URL:-https://api.deepseek.com/v1}'
export C5_VL_MODEL='${C6_VL_MODEL:-deepseek-flash}'
export C5_VL_API_KEY='$C6_VL_API_KEY'
exec cargo run --release -p dev-e2e --bin c5_e2e
EOF
mkdir -p "$OUT/c6-e2e"
# 上一轮若还有驱动在跑（例如本轮 B 段提前退出），两个驱动会互相删/写同一个 work 目录
# → 语料落位失败。先确定性地清掉（-x 精确匹配进程名，避免误杀本脚本自己的命令行）。
docker exec "$CONTAINER" bash -lc 'pkill -x c5_e2e 2>/dev/null; true'
sleep 1
docker exec "$CONTAINER" mkdir -p /workspace/reports/logs/c6-e2e
docker cp /tmp/c6_run.sh "$CONTAINER":/tmp/c6_run.sh >/dev/null
REUSE=0
if [ "${C6_REUSE_WORK:-0}" = "1" ] && [ -f "$OUT/c6-e2e/c5-status.txt" ] \
   && grep -q "C5_RESULT" "$OUT/c6-e2e/c5-status.txt" && docker exec "$CONTAINER" test -f "$WORK/db/library.db"; then
  REUSE=1
fi
# 不复用时清掉上一轮状态文件 —— 否则残留的 C5_RESULT 行会让下面的轮询立刻"以为跑完了"，
# 于是在驱动还在跑（DB 正被写）时就把库拷走，后面所有真数据断言都建在半截库上。
if [ "$REUSE" != "1" ]; then
  rm -f "$OUT/c6-e2e/c5-status.txt"
  docker exec "$CONTAINER" bash -lc 'pkill -x c5_e2e 2>/dev/null; true'
  sleep 1
fi
if [ "${C6_REUSE_WORK:-0}" = "1" ] && [ -f "$OUT/c6-e2e/c5-status.txt" ] \
   && grep -q "C5_RESULT" "$OUT/c6-e2e/c5-status.txt" && docker exec "$CONTAINER" test -f "$WORK/db/library.db"; then
  # 复用上一轮驱动产物：真库已在盘上且已跑完（驱动本身耗 9 分钟，改脚本后没必要重跑真图管线）
  echo "[B] C6_REUSE_WORK=1：复用 $WORK 的真库与状态（跳过驱动）" | tee -a "$OUT/c6-b-e2e.log"
else
docker exec "$CONTAINER" bash -c 'nohup bash /tmp/c6_run.sh > /workspace/reports/logs/c6-e2e/driver.log 2>&1 & echo $!' > "$OUT/c6-e2e.pid"
PID="$(cat "$OUT/c6-e2e.pid")"
echo "C6 真图驱动已后台启动 pid=$PID（状态 $OUT/c6-e2e/c5-status.txt）" | tee -a "$OUT/c6-b-e2e.log"

DEADLINE=$(( $(date +%s) + 3600 ))   # 60 min
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  # 只认"进程已退出"为终止条件（状态文件可能出现在退出前；进程退出后再读才可信）
  if ! docker exec "$CONTAINER" kill -0 "$PID" 2>/dev/null; then sleep 3; break; fi
  # 僵尸态也算退出：容器 PID 1 不是 init（无 `--init`/tini）时，后台驱动的退出进程不会被
  # reap → 变 zombie，`kill -0` 对 zombie 恒真 → 会白等到 60min DEADLINE
  # （C8 回归 2026-10-17 与 WEBCAP 卡各踩过一次；正确的修法仍是构建容器加 `--init`）。
  # 用 /proc/<pid>/status 判 Zombie（不依赖容器内有没有 ps）。
  if docker exec "$CONTAINER" bash -lc "grep -q '^State:.*Z' /proc/$PID/status 2>/dev/null"; then
    echo "[b] 驱动进程 $PID 已是僵尸态（容器 init 不 reap）→ 视为已退出" | tee -a "$OUT/c6-b-e2e.log"
    sleep 3; break
  fi
  sleep 20
done
fi
RESULT=$(grep "C5_RESULT" "$OUT/c6-e2e/c5-status.txt" 2>/dev/null | tail -1 || true)
echo "真图驱动终态: ${RESULT:-<无——超时或崩溃>}" | tee -a "$OUT/c6-b-e2e.log"
# C6 的 B 段只看"管线跑完且结构健康"：有终态行 + classic_diff=0 + 作业全部落地
# （PHASE2 的 done+failed == target，即没有卡死/半途中断）。**不套用 C5 自己的 KPI**
# （机器处理率 ≥95% / 人工 ≤1% / failed_jobs=0）——那是 C5 卡的验收口径，且受云端 VL
# 偶发空应答影响（C5 报告里已记录该抖动与重试策略）。C6 要的是"真库里有真判定"，
# KPI 数值只如实记入日志，不当作门禁。
echo "$RESULT" | grep -q "C5_RESULT" || { echo "无终态行，B 段终止" | tee -a "$OUT/c6-b-e2e.log"; exit 1; }
P2=$(grep "C5_PHASE2 OK" "$OUT/c6-e2e/c5-status.txt" | tail -1)
GATE=$(python3 - "$TARGET" "$P2" "$RESULT" <<'PYG'
import re, sys
target, p2, res = int(sys.argv[1]), sys.argv[2], sys.argv[3]
d = re.search(r"done=(\d+)", p2); f = re.search(r"failed=(\d+)", p2)
done = int(d.group(1)) if d else -1
failed = int(f.group(1)) if f else -1
cd = "classic_diff=0" in res
ok = cd and done >= 0 and failed >= 0 and done + failed >= target
print("OK" if ok else f"BAD done={done} failed={failed} classic_diff_ok={cd}")
PYG
)
if [ "$GATE" != "OK" ]; then
  echo "真图驱动未跑完/结构异常（$GATE），B 段终止" | tee -a "$OUT/c6-b-e2e.log"
  exit 1
fi
echo "驱动结构自检: $GATE（完成 $((TARGET)) 张作业；C5 KPI 见上一行，不作 C6 门禁）" | tee -a "$OUT/c6-b-e2e.log"

# ---- 用真图库起 dam-web（真数据，不做任何 mock）----
# SQLite 一致性快照：用 backup API 而不是 cp（WAL 模式下 cp 单个 .db 会丢已在 wal 里的事务）
docker exec "$CONTAINER" bash -lc "rm -rf $WEBDATA && mkdir -p $WEBDATA && python3 - <<'PY'
import sqlite3
src = sqlite3.connect('$WORK/db/library.db')
dst = sqlite3.connect('$WEBDATA/dam.db')
src.backup(dst)
dst.close(); src.close()
print('snapshot ok')
PY
ls -l $WEBDATA"
# 关键：上一轮的 dam-web 可能还在跑（pkill -f 会匹配到自己的命令行而自杀，靠不住）——
# 残留进程占着 $PORT 时，新进程起不来，后面所有 HTTP 断言都会打到**上一轮的旧库**上，
# 表现为"接口返回 ok、真库里却是 0 行"。这里用 -x 精确匹配，并等端口真空出来。
docker exec "$CONTAINER" bash -lc 'pkill -x app-web 2>/dev/null; true'
for i in $(seq 1 15); do
  if ! docker exec "$CONTAINER" bash -lc "curl -s -o /dev/null --max-time 1 http://127.0.0.1:$PORT/healthz"; then break; fi
  sleep 1
done
docker exec "$CONTAINER" bash -c "cd /workspace && export CARGO_TARGET_DIR=/cargo-target/dam DAM_WEB_DATA=$WEBDATA DAM_WEB_PORT=$PORT DAM_WEB_BIND=127.0.0.1 && nohup /cargo-target/dam/release/app-web > /tmp/c6-web.log 2>&1 & echo started"
sleep 3
# 自检：这个新进程必须真的在服务我们刚快照的那个库（数量级对不上就立刻停）
SNAPN=$(docker exec "$CONTAINER" bash -lc "python3 -c \"import sqlite3;print(sqlite3.connect('$WEBDATA/dam.db').execute('select count(*) from assets').fetchone()[0])\"")
SERVED=$(docker exec "$CONTAINER" bash -lc "curl -s http://127.0.0.1:$PORT/api/library/assets?limit=1" | python3 -c "import json,sys;print(json.load(sys.stdin)['total'])")
if [ "$SNAPN" != "$SERVED" ]; then
  echo "dam-web 服务的库不是本次快照（快照 assets=$SNAPN，接口 assets=$SERVED）——有残留实例？" >&2
  docker exec "$CONTAINER" bash -lc 'tail -5 /tmp/c6-web.log' >&2 || true
  exit 1
fi
for i in $(seq 1 40); do
  if docker exec "$CONTAINER" curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then break; fi
  sleep 1
done
docker exec "$CONTAINER" curl -sf "http://127.0.0.1:$PORT/healthz" || { echo "dam-web 未就绪"; docker exec "$CONTAINER" tail -20 /tmp/c6-web.log; exit 1; }

# ---- HTTP 全链路断言（python3 在容器内跑，真 HTTP）----
cat > /tmp/c6_http_check.py <<'PY'
import json, os, subprocess, sys, urllib.request

PORT = os.environ.get("C6_PORT", "8791")
BASE = f"http://127.0.0.1:{PORT}"
fail = []

def req(path, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method,
                               headers={"content-type": "application/json"} if data else {})
    try:
        with urllib.request.urlopen(r) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw.strip().startswith(("{", "[")) else raw)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        return e.code, (json.loads(raw) if raw.strip().startswith(("{", "[")) else raw)

def check(name, cond, detail=""):
    print(("PASS  " if cond else "FAIL  ") + name + ("" if cond else f"  ← {detail}"))
    if not cond:
        fail.append(name)

st_code, st = req("/api/compliance/status")
check("status 200", st_code == 200, str(st))
states = st["states"]
print("      真图分桶:", json.dumps(states, ensure_ascii=False), "队列:", st["queue"], "包:", st["activePack"]["id"])
check("真图有判定（judged > 0）", states["judged"] > 0, str(states))
check("分桶合计 = 真图资产数", states["total"] == states["judged"] + states["unjudged"])

# 1) 真数据对账：库视图各状态过滤 total == status 分桶
for word in ["pass", "fixed", "fail", "pending_human", "unjudged"]:
    code, r = req(f"/api/library/assets?compliance={word}&limit=1000")
    check(f"过滤 {word} 200", code == 200, str(r))
    check(f"过滤 {word} 计数 == 分桶 {states.get(word)}", r.get("total") == states.get(word),
          f"filter={r.get('total')} bucket={states.get(word)}")
code, r = req("/api/library/assets?compliance=nonsense")
check("非法状态词 400", code == 400, f"{code}")

# 2) 真待人工队列
qcode, q = req("/api/compliance/queue?limit=50")
check("queue 200", qcode == 200, str(q))
check("队列 total == 分桶 pending_human", q["total"] == states["pending_human"],
      f"queue={q['total']} bucket={states['pending_human']}")
if q["items"]:
    card = q["items"][0]
    rule = card["rules"][0]
    check("队列项含规则名/证据原因字段", "ruleName" in rule and "ruleId" in rule, str(rule))
    check("队列项真实图片存在", bool(card["fileName"]), str(card))
    # 3) 对真 pending_human 项写反馈（人工判不合格）→ 落库 + 队列 resolved
    fcode, f = req("/api/compliance/feedback", "POST", {
        "assetId": card["assetId"], "ruleId": rule["ruleId"],
        "humanResult": "fail", "reason": "C6 验收：人工复核真图判定",
    })
    check("真图反馈写入 200", fcode == 200, str(f))
    check("机器结论由服务端推导（= 判定行 result）", f["feedback"]["machineResult"] == rule["result"],
          f"machine={f['feedback']['machineResult']} verdict={rule['result']}")
    check("转人工处置不算误判", f["feedback"]["misjudged"] is False, str(f["feedback"]))
    # 反向：对一条机器 pass 的规则判 fail → 必须记误判
    code, pas = req("/api/library/assets?compliance=pass&limit=1")
    if pas["items"]:
        aid = pas["items"][0]["assetId"]
        vcode, v = req(f"/api/compliance/verdicts?assetId={aid}")
        pass_rule = next((i for i in v["items"] if i["result"] == "pass"), None)
        if pass_rule:
            fcode2, f2 = req("/api/compliance/feedback", "POST", {
                "assetId": aid, "ruleId": pass_rule["ruleId"], "humanResult": "fail",
            })
            check("机器 pass / 人工 fail → 记误判", fcode2 == 200 and f2["feedback"]["misjudged"] is True,
                  str(f2))
            check("误判自动补 reason", "误判" in (f2["feedback"]["reason"] or ""), str(f2["feedback"]))
    # 4) 反馈 CSV 完整
    code, csv = req("/api/compliance/feedback/export")
    lines = [l for l in csv.strip().split("\n")]
    list_code, fl = req("/api/compliance/feedback")
    check("CSV 200", code == 200)
    check("CSV 行数 == 反馈条数 + 表头", len(lines) == fl["total"] + 1,
          f"lines={len(lines)} feedback={fl['total']}")
    check("CSV 表头固定列", lines[0].strip().lstrip("\ufeff").startswith(
        "packId,assetId,ruleId,machineResult,humanResult,misjudged,reason,ts"), lines[0])
    check("CSV 含误判标记列取值", any(",1," in l for l in lines[1:]) and any(",0," in l for l in lines[1:]),
          str(lines[1:3]))
    # 5) 行业包切换：历史判定不丢（SQL 直查）+ 切回后分桶复现
    before_code, before = req("/api/compliance/status")
    code, packs = req("/api/compliance/packs")
    check("packs 200 且内置包可装载", code == 200 and any(p["rules"] > 0 for p in packs["builtin"]), str(packs)[:200])
    code, act = req("/api/compliance/packs/activate", "POST", {"source": "builtin", "id": "ecommerce-taobao"})
    check("激活内置包 200", code == 200, str(act))
    code, pv = req("/api/compliance/packs/ecommerce-taobao")
    check("只读预览四段齐", code == 200 and len(pv["rules"]) > 0 and len(pv["pipeline"]["steps"]) > 0
          and len(pv["report"]["summary"]) > 0 and len(pv["taxonomy"]) > 0, str(pv)[:200])
    code, st2 = req("/api/compliance/status")
    check("切回内置包后分桶复现", st2["states"] == before["states"], f"{st2['states']} vs {before['states']}")

print(json.dumps({"states": states, "queue": st["queue"], "feedback": st["feedback"]}, ensure_ascii=False))
# 落盘内置包分桶：严格包脚本切回内置包后要跟这份快照逐字段对账（不假设某个具体数值）
open("/tmp/c6-builtin-states.json", "w").write(json.dumps(states))
sys.exit(1 if fail else 0)
PY
docker cp /tmp/c6_http_check.py "$CONTAINER":/tmp/c6_http_check.py >/dev/null

# ---- B2) 真图真「待人工」段：导入严格包（HTTP）→ 跑真编排 → 队列/误判/过滤真数据验收 ----
echo "[B2] 导入并激活严格包 + 真编排产出 pending_human"
cat > /tmp/c6_strict_check.py <<'PY'
import json, os, sys, urllib.request

PORT = os.environ.get("C6_PORT", "8791")
BASE = f"http://127.0.0.1:{PORT}"
fail = []

def req(path, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method,
                               headers={"content-type": "application/json"} if data else {})
    try:
        with urllib.request.urlopen(r) as resp:
            raw = resp.read().decode()
            return resp.status, (json.loads(raw) if raw.strip().startswith(("{", "[")) else raw)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        return e.code, (json.loads(raw) if raw.strip().startswith(("{", "[")) else raw)

def check(name, cond, detail=""):
    print(("PASS  " if cond else "FAIL  ") + name + ("" if cond else f"  ← {detail}"))
    if not cond:
        fail.append(name)

# 严格包已由 acceptance 脚本导入并激活（HTTP 走的就是 C6 的导入/激活接口）
code, st = req("/api/compliance/status")
check("严格包已激活", code == 200 and st["activePack"]["id"] == "c6-strict-pack", str(st["activePack"]))
check("严格包规则数 = 2", st["pack"]["rules"] == 2, str(st["pack"]))
states = st["states"]
print("      严格包真图分桶:", json.dumps(states, ensure_ascii=False), "队列:", st["queue"])
check("真图产出 pending_human（队列有素材）", states["pending_human"] > 0, str(states))
check("分桶合计 = 真图资产数", states["total"] == states["judged"] + states["unjudged"])

# 过滤计数 == 分桶（严格包真数据对账）
for word in ["pass", "fixed", "fail", "pending_human", "unjudged"]:
    code, r = req(f"/api/library/assets?compliance={word}&limit=1000")
    check(f"严格包过滤 {word} 计数 == 分桶 {states.get(word)}", code == 200 and r.get("total") == states.get(word),
          f"http={code} filter={r.get('total') if isinstance(r, dict) else r} bucket={states.get(word)}")

# 队列 → 真卡片（真图片名/真证据）→ 写若干条反馈
qcode, q = req("/api/compliance/queue?limit=50&onlyUnresolved=true")
check("队列 200 且 total == pending_human 分桶", qcode == 200 and q["total"] == states["pending_human"],
      f"queue={q.get('total')} bucket={states['pending_human']}")
check("队列有卡片", len(q["items"]) > 0, str(q)[:200])
written = 0
for card in q["items"][:5]:
    for rule in card["rules"]:
        if written >= 5:
            break
        # 真图：机器判不了 → 人工处置（不算误判）
        code, f = req("/api/compliance/feedback", "POST", {
            "assetId": card["assetId"], "ruleId": rule["ruleId"], "humanResult": "fail",
            "reason": "C6 验收：真图严格包人工复核",
        })
        check(f"真图 pending 项反馈写入 #{card['assetId']}/{rule['ruleId']}", code == 200, str(f))
        check("机器结论 = 判定行 result（服务端推导）",
              code == 200 and f["feedback"]["machineResult"] == rule["result"], str(f))
        written += 1
    if written >= 5:
        break
check("至少写入 1 条真图反馈", written > 0, f"written={written}")

# 重复处置：最新反馈为准（写第二次 pass → 列表/CSV 应显示 pass）
if q["items"]:
    c0 = q["items"][0]
    r0 = c0["rules"][0]
    code, _ = req("/api/compliance/feedback", "POST",
                  {"assetId": c0["assetId"], "ruleId": r0["ruleId"], "humanResult": "pass"})
    check("重复处置可改判（写新行）", code == 200)
    lcode, fl = req(f"/api/compliance/feedback?assetId={c0['assetId']}&ruleId={r0['ruleId']}")
    latest = fl["items"][-1] if fl["items"] else {}
    check("同键多条以最新为准（id 最大）", latest.get("humanResult") == "pass", str(fl)[:200])

# 队列已处置项标记 resolved（取消 onlyUnresolved 回看）
code, q2 = req("/api/compliance/queue?limit=50")
resolved_n = sum(1 for c in q2["items"] if c["resolved"])
check("回看队列含已处置项（resolved=true）", resolved_n > 0, f"resolved={resolved_n}")

# CSV == 反馈条数
code, csv = req("/api/compliance/feedback/export")
lcode, fl = req("/api/compliance/feedback")
lines = csv.strip().split("\n")
check("严格包反馈 CSV 行数 == 反馈条数 + 表头", len(lines) == fl["total"] + 1,
      f"lines={len(lines)} feedback={fl['total']}")
check("CSV 记录的是严格包", "c6-strict-pack" in csv, csv[:200])

# 切回内置包：分桶回到内置口径（历史判定不丢的另一半证明）
code, act = req("/api/compliance/packs/activate", "POST", {"source": "builtin", "id": "ecommerce-taobao"})
check("切回内置包 200", code == 200, str(act))
code, st3 = req("/api/compliance/status")
try:
    builtin_states = json.load(open("/tmp/c6-builtin-states.json"))
except Exception as e:  # noqa: BLE001
    builtin_states = {}
    check("能读到内置包分桶快照", False, str(e))
check("切回内置包：包与分桶都回到快照（不串包、判定不丢）",
      st3["activePack"]["id"] == "ecommerce-taobao" and st3["states"] == builtin_states,
      f"now={st3['states']} snapshot={builtin_states}")
print(json.dumps({"strict_states": states, "strict_queue": st["queue"]}, ensure_ascii=False))
sys.exit(1 if fail else 0)
PY
docker cp /tmp/c6_strict_check.py "$CONTAINER":/tmp/c6_strict_check.py >/dev/null

# 导入严格包（HTTP，真走 C6 导入接口）+ 激活
docker exec "$CONTAINER" curl -sf -X POST "http://127.0.0.1:$PORT/api/compliance/packs/import" \
  -H 'content-type: application/json' \
  -d '{"dir":"/workspace/scripts/c6-fixtures/c6-strict-pack"}' | tee -a "$OUT/c6-b-e2e.log"
echo | tee -a "$OUT/c6-b-e2e.log"
docker exec "$CONTAINER" curl -sf -X POST "http://127.0.0.1:$PORT/api/compliance/packs/activate" \
  -H 'content-type: application/json' \
  -d '{"source":"imported","id":"c6-strict-pack"}' | tee -a "$OUT/c6-b-e2e.log"
echo | tee -a "$OUT/c6-b-e2e.log"

# 真编排：严格包 classic-only 跑真图（同一套 orchestrator + 落库路径）
docker exec -e CARGO_TARGET_DIR=/cargo-target/dam -e C6_DB="$WEBDATA/dam.db" \
  -e C6_PACK_DIR=/workspace/scripts/c6-fixtures/c6-strict-pack "$CONTAINER" \
  /cargo-target/dam/release/c6_pending 2>&1 | tee -a "$OUT/c6-b-e2e.log"
PENDING_RESULT=$(grep "C6_PENDING_RESULT" "$OUT/c6-b-e2e.log" | tail -1 || true)
if ! echo "$PENDING_RESULT" | grep -q "C6_PENDING_RESULT OK"; then
  echo "严格包真编排未成功：$PENDING_RESULT" >&2
  exit 1
fi

echo "########## C) 硬断言（真数据 HTTP 全链路） ##########" | tee "$OUT/c6-c-assert.log"
: > "$OUT/c6-b-http.log"   # 每轮重来：追加会让上一轮的 FAIL 混进本轮统计
set +e
# B2 把严格包设成了激活包 —— 先切回内置包，内置包阶段必须在内置包上下文里跑
docker exec "$CONTAINER" curl -sf -X POST "http://127.0.0.1:$PORT/api/compliance/packs/activate" \
  -H 'content-type: application/json' -d '{"source":"builtin","id":"ecommerce-taobao"}' >/dev/null
# 内置包阶段（真图）：分桶↔过滤真数据对账 + 误判标记 + 切包不丢；同时落盘分桶快照
docker exec -e C6_PORT="$PORT" "$CONTAINER" python3 /tmp/c6_http_check.py 2>&1 | tee -a "$OUT/c6-b-http.log"
HTTP_RC=${PIPESTATUS[0]}
# 严格包阶段（待人工真素材）：队列/处置/过滤/CSV；脚本结尾切回内置包并与快照对账
# （内置包阶段末尾把它切回来了，这里再切回严格包 —— 真判定一直在库里，切包只是换视图）
docker exec "$CONTAINER" curl -sf -X POST "http://127.0.0.1:$PORT/api/compliance/packs/activate" \
  -H 'content-type: application/json' -d '{"source":"imported","id":"c6-strict-pack"}' >/dev/null
docker exec -e C6_PORT="$PORT" "$CONTAINER" python3 /tmp/c6_strict_check.py 2>&1 | tee -a "$OUT/c6-b-http.log"
STRICT_RC=${PIPESTATUS[0]}
set -e
# 换包不删判定的 SQL 直查（真库）
DBSZ=$(docker exec "$CONTAINER" bash -lc "python3 - <<'PY'
import sqlite3, json
c = sqlite3.connect('$WEBDATA/dam.db')
n = c.execute('SELECT COUNT(*) FROM rule_verdict').fetchone()[0]
pk = c.execute('SELECT packId, COUNT(*) FROM rule_verdict GROUP BY packId').fetchall()
fb = c.execute('SELECT COUNT(*) FROM rule_feedback').fetchone()[0]
print(json.dumps({'verdicts': n, 'byPack': pk, 'feedback': fb}))
PY")
echo "真库判定/反馈: $DBSZ" | tee -a "$OUT/c6-c-assert.log"
VERDICT_N=$(python3 -c "import json,sys;print(json.loads('''$DBSZ''')['verdicts'])")
FB_N=$(python3 -c "import json,sys;print(json.loads('''$DBSZ''')['feedback'])")
PACKS_N=$(python3 -c "import json,sys;print(len(json.loads('''$DBSZ''')['byPack']))")
HUMAN=$(grep -c "PASS" "$OUT/c6-b-http.log" || true)
FAILED=$(grep -c "FAIL" "$OUT/c6-b-http.log" || true)

python3 - "$OUT/c6-summary.json" "$VERDICT_N" "$FB_N" "$FAILED" "$HUMAN" "$PACKS_N" <<'PY'
import json, sys
out = sys.argv[1]
verdicts, feedback, failed, passed, packs = (int(x) for x in sys.argv[2:7])
s = {"verdicts_in_db": verdicts, "feedback_in_db": feedback, "packs_in_db": packs,
     "http_checks_pass": passed, "http_checks_fail": failed}
open(out, "w").write(json.dumps(s, ensure_ascii=False, indent=2))
print(json.dumps(s, ensure_ascii=False))
checks = {
    "真图判定落库 > 0": verdicts > 0,
    "HTTP 全链路无 FAIL": failed == 0 and passed > 0,
    "反馈落库（与 CSV/接口同源）": feedback > 0,
    "真库含两个包的判定（换包不删）": packs >= 2,
}
for k, v in checks.items():
    print(f"{'PASS' if v else 'FAIL'}  {k}")
sys.exit(0 if all(checks.values()) else 1)
PY
RC=$?
docker exec "$CONTAINER" bash -lc "pkill -x app-web >/dev/null 2>&1" || true
if [ "$RC" -ne 0 ] || [ "$HTTP_RC" -ne 0 ] || [ "$STRICT_RC" -ne 0 ]; then
  echo "C6 acceptance 失败（HTTP_RC=$HTTP_RC STRICT_RC=$STRICT_RC）" >&2
  exit 1
fi
echo "C6 acceptance 全部通过 — 产物 $OUT/c6-{a-tests,b-e2e,b-http,c-assert}.log、c6-summary.json、c6-e2e/"
