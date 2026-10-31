#!/bin/bash
# 「装载前 lint」一条命令（C8）：对任意 pack 目录做键级 schema + 注册表层体检。
#
#   bash scripts/pack_lint.sh /workspace/packs/realestate
#   bash scripts/pack_lint.sh /workspace/scripts/c7-fixtures/schema-probes/*
#
# 退出码 = 0 全部干净 / 1 有 issue（可直接进 CI）/ 2 用法错。
# 判定与引擎装载路径同一套：键级未知键（Pack::from_dir 硬拒）+ 注册表层
# （svc-pipeline::compliance_lint，app-web 导入/激活门禁）。
# **路径要写成容器内路径**（repo 挂在 /workspace）；相对路径按容器 cwd=/workspace 解析。
set -uo pipefail
cd "$(dirname "$0")/.."
CONTAINER="${DAM_CONTAINER:-dam-dev}"
BIN=/cargo-target/dam/release/pack_lint
if [ "$#" -eq 0 ]; then
  echo "用法: bash scripts/pack_lint.sh <pack 目录> [<pack 目录> ...]" >&2
  exit 2
fi
ARGS=""
for d in "$@"; do ARGS="$ARGS $(printf '%q' "$d")"; done
docker exec -e HTTP_PROXY= -e HTTPS_PROXY= -e http_proxy= -e https_proxy= "$CONTAINER" \
  bash -lc "cd /workspace && test -x $BIN || { echo '缺 $BIN：先 cargo build --release -p dev-e2e --bin pack_lint' >&2; exit 3; }; $BIN$ARGS"
