#!/usr/bin/env bash
# GH-7 打包门禁（结构级）：安装包只准带检索必需塔（siglip2 双塔 + tokenizer）。
#
# 为什么是硬门禁：GH-7 起 v0.1.0-preview 安装包为 lite 形态 —— resources/models 里
# 除 `siglip2-base-p224-onnx/` 外出现**任何**权重条目即构建失败；`dinov3*` 路径
# 任何位置出现即失败（Meta DINOv3 许可禁止随包再分发，GH-5 摘除后再入是合规事故）。
# 字节级（bytes+sha256 ↔ model_store::manifests()）校验在 Rust 集成测试
# `crates/app-tauri/tests/bundle_models_gate.rs`（DAM_RELEASE_GATE=1 时强制），
# 本脚本管结构与红线，两层各守一道。
#
# 用法：
#   bash scripts/models_bundle_gate.sh            # 断言：不合格即退出 1（beforeBundleCommand 挂的就是这个）
#   bash scripts/models_bundle_gate.sh --prune    # 先删 siglip2 以外的条目，再断言（release 流程第一步）
#
# 退出码：0 通过；1 不合格。
#
# 可移植性：git-bash(Windows) / macOS / Linux 通用（不用 GNU 专属选项；体积用 wc -c）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RES="$ROOT/crates/app-tauri/resources"
MODELS_DIR="$RES/models"
BUNDLED="siglip2-base-p224-onnx"
# 前 3 个是 model_store::siglip2_base_p224() 声明的分发文件（sha256 级校验在 Rust 门禁）；
# 后 2 个是 HF 导出的运行时配套（preprocessor/config，缺失会让 preprocess 配置回退失效）。
REQUIRED_FILES="vision.onnx text.onnx tokenizer.json config.json preprocessor_config.json"

fail() { echo "FAIL  $*" >&2; exit 1; }

# ---------- 1) dinov3 红线：resources 下任何路径命中即死（先于一切，prune 也不许带它走） ----------
hits="$(find "$RES" -iname '*dinov3*' 2>/dev/null)"
if [ -n "$hits" ]; then
  echo "FAIL  打包红线：resources/ 出现 dinov3 路径（Meta 许可禁止随包分发，GH-5 摘除对象）：" >&2
  echo "$hits" | sed 's/^/    /' >&2
  exit 1
fi

# ---------- 2) --prune：删掉 siglip2 以外的所有条目（release 流程第一步；破坏性操作自报家门） ----------
if [ "${1:-}" = "--prune" ]; then
  if [ -d "$MODELS_DIR" ]; then
    for entry in "$MODELS_DIR"/*; do
      [ -e "$entry" ] || continue
      name="$(basename "$entry")"
      [ "$name" = "$BUNDLED" ] && continue
      echo "prune: 移除 $entry"
      rm -rf "$entry"
    done
  fi
elif [ $# -gt 0 ]; then
  fail "未知参数 $1（支持：--prune / 无参）"
fi

# ---------- 3) 结构断言：models/ 下有且只有一个条目 = siglip2-base-p224-onnx（非空目录） ----------
[ -d "$MODELS_DIR/$BUNDLED" ] || fail "$MODELS_DIR/$BUNDLED 不存在 —— 先把 siglip2 权重 stage 进来（release.yml 的 stage-models 步骤；开发机误删可从分发源重建）"

others=""
for entry in "$MODELS_DIR"/*; do
  [ -e "$entry" ] || continue
  name="$(basename "$entry")"
  if [ "$name" != "$BUNDLED" ]; then
    others="$others
  $entry"
  fi
done
if [ -n "$others" ]; then
  echo "FAIL  resources/models 出现内置清单以外的权重条目（lite 包只准带 $BUNDLED，其余走 model_store 分发/GH-9）：" >&2
  echo "$others" | sed 's/^/    /' >&2
  exit 1
fi

# ---------- 4) siglip2 关键文件齐全且非空（空目录 = 权重没解开，按「没有」处理） ----------
missing=""
for f in $REQUIRED_FILES; do
  p="$MODELS_DIR/$BUNDLED/$f"
  if [ ! -s "$p" ]; then
    missing="$missing $f"
  fi
done
[ -z "$missing" ] || fail "$BUNDLED 缺关键文件（空文件也算缺）：$missing"

total="$(du -sh "$MODELS_DIR/$BUNDLED" | cut -f1)"
echo "PASS  models bundle gate：resources/models 仅含 $BUNDLED/（${total}，关键文件 5/5 就位；无 dinov3 路径）"
