#!/usr/bin/env bash
# publish_gate.sh — 公开仓（damdesk-public）发布前门禁。放内网仓，**不进公开仓**。
#
# 用法:
#   bash scripts/publish_gate.sh [公开仓目录]        # 默认 /data/projects/damdesk-public
#   GITLEAKS_BIN=/path/to/gitleaks                   # 覆盖 gitleaks 二进制路径
#
# 门（任一失败即 exit 1，全绿输出 PASS）:
#   1. 敏感词扫描   —— soujpg / 内网地址 / 个人路径，白名单见下方 EXEMPT_RE 注释
#   2. gitleaks     —— 凭证扫描（工作区文件，--no-git）
#   3. 体积         —— 任一 tracked 文件 >20MB 即失败（当前唯大文件 NotoSansSC 17.7MB 合规）
#   4. cargo check --workspace
#   5. pnpm -C web install --frozen-lockfile && build
#   6. conformance  —— bash tools/pack-conformance/run_all.sh
#   7. pack_lint    —— 全部官方插件包（强制直跑分支，防止容器模式把路径解析到内网仓）
#   8. 官方包数量   —— packs/plugins 必须恰好 5 个
#   9. 发布形态能力面 —— app-web 依赖图必须编入 pack-host/local-infer（否则官方 3/5 包装不进）、
#                       deploy/Dockerfile 构建行显式带 --features local-infer、
#                       官方包×形态矩阵两种 feature 形态各跑一次（pack-host/tests/form_capabilities.rs）
#
# 发布口径（t_c3e0417a 裁决）：web/容器**正式发布带 local-infer**，模型不内置（挂卷/下载）。
# 桌面版（app-tauri 默认 feature）同样带；裸 `cargo build -p app-web` 是「无本地推理」形态，
# 只用于验证拒装语义，不用于发布。
#
# 敏感词白名单口径（plan §6 档 A：identifier / pack id / ARCHITECTURE 命名说明段）:
#   路径级: 无（全部收紧为形态级）
#   形态级（行匹配即放行）:
#     com.soujpg.pack.             pack id 前缀（含 format!("com.soujpg.pack.{fam}") 变量形态）
#     com.soujpg.dam.desktop       应用 identifier（appData 路径派生源，改名=数据迁移）
#     'com.soujpg'                 web 测试「UI 不得暴露内部反向域」断言字面量
#     soujpg-images-1307121509     模型分发镜像桶/域名（GH-9 发布裁决的稳定分发基础设施；
#                                  桶私有，仅 damdesk/models/ 前缀按桶策略放行）
#   文件级（段内放行）: docs/ARCHITECTURE.md 的「命名与历史」节（档 A 说明段；
#     节外命中仍失败；节标题改名/删除即整文件恢复零豁免）
#   新增白名单条目必须在 PR 说明里写明理由。
set -uo pipefail

PUB="${1:-/data/projects/damdesk-public}"
INTRANET="$(cd "$(dirname "$0")/.." && pwd)"
LOG="/tmp/publish-gate-$(date +%Y%m%d-%H%M%S).log"
TMP="$(mktemp -d /tmp/publish-gate-work.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
STEP=0
FAIL=0

say()  { printf '[gate %d/9] %s\n' "$STEP" "$*" | tee -a "$LOG"; }
fail() { printf 'FAIL: %s\n' "$*" | tee -a "$LOG"; FAIL=1; }

# --- 前置 ---
[ -d "$PUB/.git" ] || { echo "公开仓不存在: $PUB"; exit 2; }
cd "$PUB" || exit 2
: > "$LOG"
for t in git cargo pnpm; do
  command -v "$t" >/dev/null 2>&1 || { echo "FAIL: 缺工具链 $t — gate 环境需与 ci.yml 系统依赖一致（git + apt: libvips-dev pkg-config cmake libgtk-3-dev libwebkit2gtk-4.1-dev librsvg2-dev patchelf + cargo/pnpm）"; exit 2; }
done
# 容器跑挂载仓时 uid 与目录 owner 不一致会让 git 拒绝操作（dubious ownership）——
# gate 环境是一次性容器/专用发布机，加 safe.directory 无副作用
git config --global --add safe.directory "$PUB" 2>/dev/null || true
if [ -n "$(git status --porcelain)" ]; then
  echo "FAIL: 工作区不干净 — 门禁对已提交树负责，先 commit（mirror 会自动先提交）"; exit 1
fi

# --- 1. 敏感词 ---
STEP=1; say "敏感词扫描（档 A 白名单）…"
SENSITIVE_RE='soujpg|172\.25\.45|172\.17\.0|gpu[0-9]\.dev|sk-[A-Za-z0-9]{20,}|zhaoyufei|dev\.yufei'
EXEMPT_RE='com\.soujpg\.pack\.|com\.soujpg\.dam\.desktop|'\''com\.soujpg'\''|soujpg-images-1307121509'
raw="$(git grep -nE "$SENSITIVE_RE" 2>/dev/null)"
rc=$?
# ARCHITECTURE「命名与历史」段内放行（段边界动态解析；标题不存在=零豁免）
arch_section() {
  local start end
  start="$(grep -n '^## .*命名与历史' docs/ARCHITECTURE.md 2>/dev/null | head -1 | cut -d: -f1)"
  [ -n "$start" ] || { echo "0 0"; return; }
  end="$(awk -v s="$start" 'NR>s && /^## /{print NR; exit}' docs/ARCHITECTURE.md)"
  [ -n "$end" ] || end="$(wc -l < docs/ARCHITECTURE.md)"
  echo "$start $end"
}
read -r SEC_START SEC_END <<< "$(arch_section)"
filtered="$raw"
if [ "$SEC_START" != 0 ]; then
  filtered="$(printf '%s\n' "$filtered" | awk -v f=docs/ARCHITECTURE.md -v s="$SEC_START" -v e="$SEC_END" '
    $0 ~ "^"f":" { split($0, a, ":"); if (a[2]+0 >= s && a[2]+0 <= e) next } { print }')"
fi
filtered="$(printf '%s' "$filtered" | grep -vE "$EXEMPT_RE" || true)"
if [ "$rc" -ge 2 ]; then
  fail "git grep 运行失败(rc=$rc) — 扫描不可信，禁止放行"
elif [ -n "$filtered" ]; then
  fail "敏感词残留（白名单外命中，见下）"
  printf '%s\n' "$filtered" | tee -a "$LOG"
  say "白名单口径见脚本头注释；新增豁免必须写进 PR 说明。"
else
  say "OK — 白名单外零命中"
fi

# --- 2. gitleaks ---
STEP=2; say "gitleaks（凭证，仅提交树）…"
GL="${GITLEAKS_BIN:-$(command -v gitleaks || true)}"
if [ -z "$GL" ]; then
  fail "gitleaks 未安装 — 门禁不降级。安装: https://github.com/gitleaks/gitleaks/releases (Linux x64 tar 包解出放入 PATH)，或 GITLEAKS_BIN 指向二进制"
else
  ARC="$TMP/publish-gate-archive"
  rm -rf "$ARC"; mkdir -p "$ARC"
  if ! ( git archive HEAD | tar -x -C "$ARC" ) 2>> "$LOG"; then
    fail "git archive 导出提交树失败 — gitleaks 无从扫描，禁止放行"
  # cd 进提交树快照用相对路径扫（--source .）: 指纹为仓库相对路径，仓内 .gitleaksignore 才能稳定匹配
  elif ! ( cd "$ARC" && "$GL" detect --no-git --source . --redact ) >> "$LOG" 2>&1; then
    fail "gitleaks 发现疑似凭证 — 详单见 $LOG"
  else
    say "OK — 0 finding"
  fi
fi

# --- 3. 体积 ---
STEP=3; say "体积门（单文件 ≤20MB）…"
big="$(git ls-files -z | xargs -0 -r du -b 2>/dev/null | awk '$1 > 20971520 {print $1, $2}' | sort -rn || true)"
if [ -n "$big" ]; then
  fail "存在 >20MB 文件（提交前须移出或写明理由进白名单）"
  printf '%s\n' "$big" | tee -a "$LOG"
else
  say "OK — 最大文件 $(git ls-files -z | xargs -0 -r du -b 2>/dev/null | sort -rn | head -1 | awk '{printf "%.1fMB %s", $1/1048576, $2}')"
fi

# ort-sys(rc.10 download-binaries) 下载失败的 build 目录会被 cargo 沿用（重跑仍走失败
# fallback 输出空 -L），重试前必须清掉失败单元目录才能触发重下载
clear_failed_ort_units() {
  local base="${CARGO_TARGET_DIR:-target}"
  local prof d
  for prof in debug release; do
    for d in "$base/$prof"/build/ort-sys-*; do
      [ -d "$d" ] || continue
      if grep -q '^cargo:rustc-link-search=native=$' "$d/output" 2>/dev/null; then
        rm -rf "$d"
        echo "cleaned failed ort-sys build unit: $d" >> "$LOG"
      fi
    done
  done
}

# --- 4. cargo check ---
STEP=4; say "cargo check --workspace …"
# --locked: 依赖集以 Cargo.lock 为准不漂移。环境要求: 需要网络（CI 同款）——
# ort(rc.10 download-binaries) 首次编译按 feature 集拉 onnxruntime 预编译包（缓存于
# $HOME/.cache/ort.pyke.io，挂卷持久化）。
ok=0
for i in 1 2 3; do
  if cargo check --workspace --locked >> "$LOG" 2>&1; then ok=1; break; fi
  say "attempt $i failed — clean failed ort units and retry…"
  clear_failed_ort_units
  sleep 3
done
if [ "$ok" -ne 1 ]; then
  fail "cargo check 失败 — 尾部日志:"; tail -30 "$LOG"
else
  say "OK"
fi

# --- 5. web build ---
STEP=5; say "pnpm install --frozen-lockfile + build …"
if ! ( pnpm -C web install --frozen-lockfile && pnpm -C web build ) >> "$LOG" 2>&1; then
  fail "web 安装/构建失败 — 尾部日志:"; tail -30 "$LOG"
else
  say "OK"
fi

# --- 6. conformance ---
STEP=6; say "pack conformance 全套 …"
if ! bash tools/pack-conformance/run_all.sh >> "$LOG" 2>&1; then
  fail "conformance 失败 — 尾部日志:"; tail -30 "$LOG"
else
  say "OK"
fi

# --- 7. pack_lint（全部官方包，强制直跑） ---
STEP=7; say "pack_lint × 全部官方包（强制直跑分支）…"
PACKS=$(ls -d packs/plugins/*/ | sed 's:/$::')
lint_ok=0
for i in 1 2; do
  if DAM_CONTAINER=__gate_no_container__ bash scripts/pack_lint.sh $PACKS >> "$LOG" 2>&1; then lint_ok=1; break; fi
  say "attempt $i failed — clean failed ort units and retry…"
  clear_failed_ort_units
  sleep 3
done
if [ "$lint_ok" -ne 1 ]; then
  fail "pack_lint 失败 — 尾部日志:"; tail -30 "$LOG"
else
  say "OK — $(echo $PACKS | wc -w) 个包全过"
fi

# --- 8. 官方包数量 ---
STEP=8; say "官方包数量 == 5 …"
COUNT=$(ls packs/plugins | wc -l)
if [ "$COUNT" -ne 5 ]; then
  fail "packs/plugins 数量为 $COUNT，要求 5（增删官方包属协议面变更，须走 PR 说明）"
else
  say "OK — $(ls packs/plugins | tr '\n' ' ')"
fi

# --- 9. 发布形态能力面（t_c3e0417a 裁决: web 正式发布带 local-infer） ---
STEP=9; say "发布形态能力面（依赖图 / Dockerfile 构建行 / 官方包×形态矩阵）…"
# 9a. 发布产物 app-web 的依赖图必须编入 pack-host/local-infer。
#     注意：`cargo build/check --workspace` 会做 feature 统一（app-tauri 默认开 local-infer），
#     所以「workspace 能编过」**不能**证明 app-web 单包构建带 feature —— 必须单独查 -p app-web。
tree_ok=0
for i in 1 2; do
  if cargo tree -p app-web --features local-infer --no-default-features -e features -i pack-host \
       >> "$LOG" 2>&1 \
     && cargo tree -p app-web --features local-infer --no-default-features -e features -i pack-host 2>/dev/null \
        | grep -q 'pack-host feature "local-infer"'; then
    tree_ok=1; break
  fi
  say "attempt $i failed — clean failed ort units and retry…"
  clear_failed_ort_units
  sleep 3
done
if [ "$tree_ok" -ne 1 ]; then
  fail "app-web 依赖图未编入 pack-host/local-infer —— 该产物会拒装官方 imaging/segment/ecommerce 三包（pack_capability_missing）。检查 crates/app-web/Cargo.toml 的 pack-host 依赖与 local-infer feature 转发"
else
  say "OK — app-web → pack-host/local-infer 在依赖图上"
fi
# 9b. Docker 构建行逐字带 feature（防镜像构建与 9a 判定脱节：漏 feature 的镜像 = 官方包装不进）
if grep -qx 'RUN cargo build --release --features local-infer -p app-web' deploy/Dockerfile; then
  say "OK — deploy/Dockerfile 构建行显式 --features local-infer"
else
  fail "deploy/Dockerfile 的 app-web 构建行必须逐字为 'RUN cargo build --release --features local-infer -p app-web'（发布口径 t_c3e0417a）"
fi
# 9c. 官方包 × 形态矩阵：真 manifest 两种 feature 形态各跑一次
matrix_ok=1
run_matrix_form() {  # $1=标签，其余=cargo 参数
  local label="$1"; shift
  local attempt
  for attempt in 1 2; do
    if cargo test -p pack-host --test form_capabilities "$@" >> "$LOG" 2>&1; then
      say "OK — 矩阵[$label]"; return 0
    fi
    say "矩阵[$label] attempt $attempt failed — clean failed ort units and retry…"
    clear_failed_ort_units
    sleep 3
  done
  fail "官方包×形态矩阵失败[$label] — 尾部日志:"; tail -30 "$LOG"
  return 1
}
run_matrix_form "带 local-infer（桌面 + web 正式发布）" || matrix_ok=0
run_matrix_form "裸 web 形态（无本地推理）" --no-default-features || matrix_ok=0
[ "$matrix_ok" -eq 1 ] && say "OK — 官方 5 包能力面与两种形态一致（带 local-infer: 5/5 可装；裸 web: 2/5）"

# --- 汇总 ---
echo
if [ "$FAIL" -ne 0 ]; then
  echo "=== publish_gate: FAIL（完整日志 $LOG）===" | tee -a "$LOG"
  exit 1
fi
FILES=$(git ls-files | wc -l)
REPO_SZ=$(du -sh .git | cut -f1)
echo "=== publish_gate: PASS — files=$FILES repo=$REPO_SZ log=$LOG ===" | tee -a "$LOG"
