#!/usr/bin/env bash
# publish_public.sh — 单仓双 remote 导出：内网 main → 本地 public 分支 → GitHub。
#
# 取代旧的 publish_mirror.sh（那版在第二个工作副本上用 rsync 比对文件系统，
# 碰到 node_modules / target / 模型权重这类未追踪文件会算出上万条"删除"）。
# 本版全程走 git 对象树，不碰工作区未追踪文件。
#
# 用法:
#   bash scripts/publish_public.sh [--dry-run] [--push] [--gate]
#     --dry-run  只算差异并打印（不改 public 分支、不提交、不推）
#     --push     提交后 push 到 github public:main
#     --gate     同步后跑 publish_gate.sh（发布守门，分钟级；默认不跑）
#
# 仓结构（唯一真相 = 内网 gitea main；GitHub = public 分支）:
#   remote gitea  → 内网主线（开发、kanban、reports 都在这）
#   remote github → 公开仓（内容来自 public 分支的快照）
#
# 同步面（与旧版白名单一致）:
#   SYNC_SUBTREES  crates web packs tools tests registry  整树
#   SYNC_SCRIPTS   scripts/ 白名单（两仓共有的工程脚本）
#   SYNC_ROOT      Cargo.toml Cargo.lock package.json pnpm-lock.yaml pnpm-workspace.yaml
#   例外          packs/pack-content-lock.json 不同步
#
# PUBLIC-OWNED（本脚本永不触碰，在 public 分支上直接维护）:
#   README.md CONTRIBUTING.md SECURITY.md NOTICE LICENSE CODE_OF_CONDUCT.md
#   .github/  docs/  deploy/  .gitignore .dockerignore  scripts/ 非白名单文件
#
# 外部贡献回流: GitHub 上来的 PR 先 `git fetch github pull/<n>/head` → cherry-pick 进 main，
# 下次导出自然带过去；public 分支上手工改过的 PUBLIC-OWNED 文件不要用本脚本覆盖。
set -uo pipefail

SELF_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SELF_DIR" || exit 2

SRC_REF="main"
PUB_BRANCH="public"
GH_REMOTE="github"
GH_BRANCH="main"
DRY=0; PUSH=0; GATE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --push)    PUSH=1; shift ;;
    --gate)    GATE=1; shift ;;
    *) echo "未知参数: $1"; exit 2 ;;
  esac
done

SYNC_SUBTREES="crates web packs tools tests registry"
SYNC_SCRIPTS="bench_tag_scale.py builtin_guard.sh check_layers.sh cmp_job_e2e.py dm_b1_walk.sh \
i18n_error_code_check.py journey_acceptance.sh k15_build_packs.py mac_bundle_dylibs.sh \
merge_acceptance.sh migrate_human_review_v2.py ocr_match_acceptance.sh pack_lint.sh \
pack_runtime_gate.sh sample_real_photos.sh stage_pack_runtime.py tag_default_acceptance.sh \
test_gate.sh thumb_acceptance.sh ui3_compliance_acceptance.sh ui_copy_guard.py verify_all.sh \
webcap_acceptance.sh"
SYNC_ROOT="Cargo.toml Cargo.lock package.json pnpm-lock.yaml pnpm-workspace.yaml"
EXCLUDE="packs/pack-content-lock.json"

git rev-parse --verify "$SRC_REF^{commit}" >/dev/null || { echo "FAIL: 源 ref 不存在: $SRC_REF"; exit 2; }
git rev-parse --verify "$PUB_BRANCH^{commit}" >/dev/null || { echo "FAIL: 公开分支不存在: $PUB_BRANCH（先 git fetch $GH_REMOTE main:$PUB_BRANCH）"; exit 2; }
git remote get-url "$GH_REMOTE" >/dev/null 2>&1 || { echo "FAIL: 缺 remote $GH_REMOTE"; exit 2; }
if [ -n "$(git status --porcelain -- "$SRC_REF" 2>/dev/null)" ] && [ "$(git rev-parse --abbrev-ref HEAD)" = "$SRC_REF" ] && [ -n "$(git status --porcelain)" ]; then
  echo "WARN: 工作区有未提交改动，导出的仍是 $SRC_REF 的 commit 树（不含未提交内容）"
fi
SHA="$(git rev-parse --short "$SRC_REF")"
echo "== export: $SRC_REF@$SHA → $PUB_BRANCH (github:$GH_BRANCH) $( [ $DRY -eq 1 ] && echo '(dry-run)' )"

PATHS="$SYNC_SUBTREES"
for f in $SYNC_SCRIPTS; do PATHS="$PATHS scripts/$f"; done
for f in $SYNC_ROOT;   do PATHS="$PATHS $f"; done

WT="$(mktemp -d /tmp/publish-public.XXXXXX)/wt"
trap 'git worktree remove --force "$WT" 2>/dev/null; rm -rf "$(dirname "$WT")"' EXIT
git worktree add --detach "$WT" "$PUB_BRANCH" >/dev/null || { echo "FAIL: worktree 创建失败"; exit 1; }

# 1) 白名单子树从 main 重建（-A 也删除 main 中已不存在的文件）
git -C "$WT" checkout "$SRC_REF" -- $PATHS 2>/dev/null || { echo "FAIL: checkout 失败"; exit 1; }

# 2) 清掉 public 有而 main 没有的白名单文件（git 层比对，不碰未追踪文件）
git ls-tree -r --name-only "$SRC_REF" -- $SYNC_SUBTREES | sort > "$WT/.mn.txt"
git ls-tree -r --name-only "$PUB_BRANCH" -- $SYNC_SUBTREES | sort > "$WT/.pub.txt"
comm -13 "$WT/.mn.txt" "$WT/.pub.txt" | grep -v -x "$EXCLUDE" > "$WT/.gone.txt" || true
if [ -s "$WT/.gone.txt" ]; then
  echo "⚠️  公开侧独有、将被删除的文件（若是公开侧修复，先 cherry-pick 回 $SRC_REF）:"
  sed 's/^/    /' "$WT/.gone.txt" | head -30
  if [ $DRY -eq 0 ]; then
    while IFS= read -r f; do git -C "$WT" rm -q --ignore-unmatch -- "$f"; done < "$WT/.gone.txt"
  fi
fi
rm -f "$WT/.mn.txt" "$WT/.pub.txt" "$WT/.gone.txt"

# 3) 显式恢复例外（若 main 有则该文件不导出）
git -C "$WT" reset -q -- "$EXCLUDE" 2>/dev/null || true
[ -f "$WT/$EXCLUDE" ] && git -C "$WT" rm -q --cached --ignore-unmatch -- "$EXCLUDE" 2>/dev/null

STAGED="$(git -C "$WT" diff --cached --numstat | wc -l)"
[ "$STAGED" = "0" ] && STAGED="$(git -C "$WT" status --porcelain | wc -l)"
echo "== 待导出变更: $STAGED 个文件"
if [ "$STAGED" = "0" ]; then
  echo "== 公开分支已同步，无变更"; exit 0
fi
git -C "$WT" status --porcelain | head -20

if [ $DRY -eq 1 ]; then
  echo "== dry-run 结束（未提交、未推送）"; exit 0
fi

git -C "$WT" add -A
git -C "$WT" -c user.name='DAMDesk' -c user.email='dev@damdesk.local' \
  commit -q -m "sync: export $SRC_REF@$SHA to public snapshot" || { echo "FAIL: commit 失败"; exit 1; }
git -C "$WT" log --oneline -1

if [ $GATE -eq 1 ]; then
  echo "== publish_gate =="
  bash "$SELF_DIR/scripts/publish_gate.sh" 2>&1 | tail -25 || { echo "FAIL: publish_gate 未过，未推送"; exit 1; }
fi

NEWSHA="$(git -C "$WT" rev-parse HEAD)"

if [ $PUSH -eq 1 ]; then
  # 修复推送 bug：$WT 是 detach 检出的 worktree，commit 后 public 分支引用并不跟着走，
  # 旧写法 `push $PUB_BRANCH:$GH_BRANCH` 推的还是旧引用（Everything up-to-date）。
  # 现在推 HEAD:$GH_BRANCH，再把本地 public 分支指到新 commit，最后 ls-remote 复核远端 sha。
  echo "== push → $GH_REMOTE HEAD:$GH_BRANCH"
  git -C "$WT" -c http.proxy="${PUBLISH_HTTP_PROXY:-}" push "$GH_REMOTE" "HEAD:$GH_BRANCH" 2>&1 | tail -5 \
    || { echo "FAIL: push 失败"; exit 1; }
  git branch -f "$PUB_BRANCH" "$NEWSHA" 2>/dev/null \
    || { echo "FAIL: 本地 $PUB_BRANCH 引用更新失败（可能被某 worktree 占用）"; exit 1; }
  REMOTE_SHA="$(git -c http.proxy="${PUBLISH_HTTP_PROXY:-}" ls-remote "$GH_REMOTE" "refs/heads/$GH_BRANCH" | cut -f1)"
  if [ "$REMOTE_SHA" != "$NEWSHA" ]; then
    echo "FAIL: 远端复核不一致 remote=$REMOTE_SHA local=$NEWSHA"
    exit 1
  fi
  echo "== 复核一致: $GH_REMOTE/$GH_BRANCH = ${REMOTE_SHA:0:7} (本地 $PUB_BRANCH = $(git rev-parse --short "$PUB_BRANCH"))"
fi
echo "== 完成。public 分支已同步$( [ $PUSH -eq 1 ] && echo "并推送（远端 sha 已复核一致）" )"
