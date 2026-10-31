#!/usr/bin/env bash
# OCR-MATCH（卡 task）一键验收：OCR 字面检索从「全表 LIKE 扫描」下推到 SQLite
# FTS5 倒排（中文 2-gram + 单字允许），并把字面证据暴露给 API/前端。
#
# 用法（构建容器内，工作区挂载在 /workspace）：
#   docker exec dam-ocrmatch-build bash -lc 'cd /workspace && bash scripts/ocr_match_acceptance.sh'
# 可用 env 覆盖：BENCH_ASSETS（默认 20000）BENCH_FRAC（默认 0.6）SKIP_WEB=1
#
# 判据（任一失败即非零退出）：
#   0) 迁移号顺延（0014_ocr_fts.sql 在 migrations.rs 注册）+ 索引表/触发器/账本存在
#   1) core-storage 单测：分段口径（中文 2-gram+单字、拉丁整词、跨分隔不连词）、
#      触发器同步（INSERT/UPDATE/DELETE/软删）、dirty 自愈、分段版本升级全库重建、
#      查询计划、候选截断（大命中面退化为 rowid 序）
#   2) svc-library 单测：融合语义（抬分/补入/部分折扣/单字/整词/k 截断确定序）+ 片段窗口
#   3) app-web 端到端（真实 Router + mock OpenAI 兼容端点，无模型/无外网）：
#      /api/search/text 暴露 ocrMatched + ocrSnippet，语义独有不误标、字面独有不丢、
#      minScore 不吞字面证据、软删除过滤、外部 SQL 改写旧索引不复用且自愈
#   4) 全量回归：cargo test --workspace（除 app-tauri，Linux 存量问题）
#   5) 基准/对拍（release）：20k 资产真实语料 旧 LIKE 扫描 vs 新倒排（不漏召回 +
#      量化 2-gram 变宽 + 命中面/耗时表）；净化语料上逐查询命中集完全一致
#   6) 前端不回归：pnpm test（含 ocrChip 单测）+ pnpm run build（tsc + vite）
#
# 产出：reports/logs/ocr-match-<ts>/*.log（原始输出，验收存证）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
TS="$(date +%Y%m%d-%H%M%S)"
EV="$ROOT/reports/logs/ocr-match-$TS"
mkdir -p "$EV"
BENCH_ASSETS="${BENCH_ASSETS:-20000}"
BENCH_FRAC="${BENCH_FRAC:-0.6}"

# 容器构建期代理清理（crates.io 直连可达；代理只用于 pnpm registry）
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy 2>/dev/null || true

echo "== OCR-MATCH 验收 @ $TS（证据目录 $EV）=="
echo "   工作区 $ROOT"

# ---------- 0) 迁移与索引对象在盘 ----------
echo
echo "== [0/7] 迁移顺延 + 索引对象在盘 =="
test -f crates/core-storage/src/migrations/0014_ocr_fts.sql || { echo "FAIL: 缺 0014_ocr_fts.sql"; exit 1; }
grep -q '0014_ocr_fts.sql' crates/core-storage/src/migrations.rs || { echo "FAIL: 0014 未注册"; exit 1; }
grep -q '"ocr_fts"' crates/core-storage/src/migrations.rs || { echo "FAIL: 迁移名未登记"; exit 1; }
python3 - <<'PY' | tee "$EV/00-schema.txt"
import sqlite3, tempfile, os
# 用真实迁移文件建库（assets/resource 由 0001 建，这里只按 0014 的 SQL 验证语法与对象名）
db = os.path.join(tempfile.mkdtemp(), "schema.db")
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE assets(assetId INTEGER PRIMARY KEY, deleted INTEGER NOT NULL DEFAULT 0,
                    ocr_text TEXT NOT NULL DEFAULT '');
CREATE TABLE resource(resourceId INTEGER PRIMARY KEY, assetId INTEGER, role TEXT);
""")
c.executescript(open("crates/core-storage/src/migrations/0014_ocr_fts.sql").read())
objs = [r[0] for r in c.execute(
    "SELECT name FROM sqlite_master WHERE name LIKE 'ocr_fts%' ORDER BY name")]
print("0014 建成对象:", objs)
need = {"ocr_fts", "ocr_fts_dirty", "ocr_fts_state",
        "ocr_fts_ai", "ocr_fts_au", "ocr_fts_aud", "ocr_fts_ad"}
missing = need - set(objs)
assert not missing, f"缺对象: {missing}"
print("PASS: 索引虚表 + 待建队列 + 账本 + 4 个同步触发器齐备")
PY

# ---------- 1) core-storage ----------
echo
echo "== [1/7] core-storage（分段/索引同步/自愈/查询计划） =="
set +e
cargo test -p core-storage 2>&1 | tee "$EV/01-core-storage.log" | grep -E "^test |test result|FAILED"
rc=${PIPESTATUS[0]}
set -e
[ "$rc" = 0 ] || { echo "FAIL: core-storage 测试"; exit 1; }

# ---------- 2) svc-library ----------
echo
echo "== [2/7] svc-library（融合语义/片段） =="
set +e
cargo test -p svc-library 2>&1 | tee "$EV/02-svc-library.log" | grep -E "^test |test result|FAILED"
rc=${PIPESTATUS[0]}
set -e
[ "$rc" = 0 ] || { echo "FAIL: svc-library 测试"; exit 1; }

# ---------- 3) app-web 端到端 ----------
echo
echo "== [3/7] app-web 端到端（真实 Router + mock 端点；打印原始 JSON 存证） =="
set +e
cargo test -p app-web --test ocr_match_e2e -- --nocapture 2>&1 | tee "$EV/03-ocr-match-e2e.log" \
  | grep -E "^test |test result|---- query|ocrMatched|FAILED|panicked"
rc=${PIPESTATUS[0]}
set -e
[ "$rc" = 0 ] || { echo "FAIL: app-web OCR-MATCH e2e"; exit 1; }
grep -q '"ocrMatched":true' "$EV/03-ocr-match-e2e.log" || { echo "FAIL: 响应里没有 ocrMatched=true 存证"; exit 1; }
grep -q '"ocrSnippet"' "$EV/03-ocr-match-e2e.log" || { echo "FAIL: 响应里没有 ocrSnippet 存证"; exit 1; }

# ---------- 4) 全量回归 ----------
echo
echo "== [4/7] 全量回归（workspace 除 app-tauri；--test-threads=1） =="
# 为什么要串行：engine-segment 的 `models_root_explicit_overrides_env_and_missing_fails_loud`
# 用 std::env::set_var("DAM_MODELS_ROOT", ...) 改**进程级**环境变量，而同二进制的
# `registry::tests::fg_ratio_params_validation` 依赖「没有模型根时响亮报错」——两者并行时
# 会出现 Onnx(...does not exist) 而非 NoModelsRoot 的假失败（本卡改动未触碰 engine-segment，
# git diff 为空；见 reports/OCR-MATCH.md「排障记录」与已登记的建议修复：改用 set_models_root
# 注入或 serial_test）。串行执行下顺序确定、不重叠，回归覆盖面不变。
set +e
cargo test --workspace --exclude app-tauri -- --test-threads=1 > "$EV/04-workspace.log" 2>&1
rc=$?
n_failed=$(grep -cE "test result: FAILED" "$EV/04-workspace.log")
set -e
echo "   test result: FAILED 出现次数 = $n_failed（期望 0）"
[ "$rc" = 0 ] && [ "$n_failed" = 0 ] || { echo "FAIL: 全量回归"; tail -40 "$EV/04-workspace.log"; exit 1; }
grep -E "test result: ok" "$EV/04-workspace.log" | tail -3

# ---------- 5) 基准/对拍（release） ----------
echo
echo "== [5/7] 基准/对拍 release（$BENCH_ASSETS 资产，带文本 $BENCH_FRAC） =="
set +e
cargo run --release -p dev-e2e --bin ocr_match_bench -- "$BENCH_ASSETS" "$BENCH_FRAC" \
  2>&1 | tee "$EV/05-bench.log" | grep -vE "^warning|^ +\||^ +=|^ +-->|^[0-9]+ \|"
rc=${PIPESTATUS[0]}
set -e
[ "$rc" = 0 ] || { echo "FAIL: 基准/对拍"; exit 1; }
grep -q "OCR_MATCH_BENCH_RESULT" "$EV/05-bench.log" || { echo "FAIL: 基准未产出结果行"; exit 1; }
grep -q "完全一致 ✓" "$EV/05-bench.log" || { echo "FAIL: 净化语料未逐查询对齐"; exit 1; }

# ---------- 6) 标注语料评测（命中率/误召回/top-10 精确率，与旧实现对比） ----------
echo
echo "== [6/7] 标注语料评测（39 张带文字图 × 20 条查询：单字/多词/跨词/英文/混排） =="
set +e
cargo run --release -p dev-e2e --bin ocr_match_eval 2>&1 | tee "$EV/06-eval.log" \
  | grep -vE "^warning|^ +\||^ +=|^ +-->|^[0-9]+ \|"
rc=${PIPESTATUS[0]}
set -e
[ "$rc" = 0 ] || { echo "FAIL: 标注语料评测"; exit 1; }
grep -q "OCR_MATCH_EVAL_RESULT" "$EV/06-eval.log" || { echo "FAIL: 评测未产出结果行"; exit 1; }
grep -qE "^ +发 → terms=\[\"发\"\] .*VIRTUAL TABLE INDEX" "$EV/06-eval.log" \
  || { echo "FAIL: 单字「发」缺索引命中证据（EXPLAIN QUERY PLAN）"; exit 1; }
grep -qE "^ +发票 → terms=\[\"发票\"\] .*VIRTUAL TABLE INDEX" "$EV/06-eval.log" \
  || { echo "FAIL: 2 字「发票」缺索引命中证据"; exit 1; }
if grep -qE "panicked|assertion" "$EV/06-eval.log"; then echo "FAIL: 评测断言失败"; exit 1; fi

# ---------- 7) 前端 ----------
if [ "${SKIP_WEB:-0}" = "1" ]; then
  echo
  echo "== [7/7] 前端：SKIP_WEB=1 跳过 =="
else
  echo
  echo "== [7/7] 前端：vitest（ocrChip 口径 + DOM 文本存证）+ tsc/vite 构建 =="
  set +e
  (cd web && HTTPS_PROXY="${NPM_PROXY:-http://192.0.2.11:11700}" \
      HTTP_PROXY="${NPM_PROXY:-http://192.0.2.11:11700}" \
      NO_PROXY="127.0.0.1,localhost,172.16.0.0/12,10.0.0.0/8,192.168.0.0/16" \
      pnpm install --prefer-offline >/dev/null 2>&1; pnpm test 2>&1 | tail -60) \
    | tee "$EV/07-web-test.log"
  rc1=${PIPESTATUS[0]}
  (cd web && pnpm run build 2>&1 | tail -6) | tee "$EV/07-web-build.log"
  rc2=${PIPESTATUS[0]}
  set -e
  grep -qE "ocrChip" "$EV/07-web-test.log" || { echo "FAIL: vitest 未跑到 ocrChip 用例"; exit 1; }
  grep -q "OCR 文字命中" "$EV/07-web-test.log" || { echo "FAIL: 缺「文字命中」DOM 文本存证"; exit 1; }
  [ "$rc1" = 0 ] && [ "$rc2" = 0 ] || { echo "FAIL: 前端测试/构建（rc1=$rc1 rc2=$rc2）"; exit 1; }
fi

echo
echo "== 产物存证 =="
ls -l "$EV"
echo
echo "OCR_MATCH_ACCEPTANCE PASS（logs: $EV）"
