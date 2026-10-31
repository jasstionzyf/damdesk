#!/bin/bash
# 「装载前 lint」一条命令（C8）：对任意 pack 目录做键级 schema + 注册表层体检。
#
#   bash scripts/pack_lint.sh /workspace/packs/realestate
#   bash scripts/pack_lint.sh /workspace/scripts/c7-fixtures/schema-probes/*
#
# 退出码 = 0 全部干净 / 1 有 issue（可直接进 CI）/ 2 用法错。
# 判定与引擎装载路径同一套，按 pack 形态自动分派：
# - 插件包（manifest.json 在盘）：pack_host::load_pack（app-web 导入/激活门禁同源）；
# - 合规行业包（四文件 yaml）：键级未知键（Pack::from_dir 硬拒）+ 注册表层
#   （svc-pipeline::compliance_lint，app-web 导入/激活门禁）。
#
# 运行环境两种（自动二选一）：
#  - 默认有 docker + dam-dev 容器（repo 挂在 /workspace）：路径按容器内路径解析
#    （容器 cwd=/workspace）；可用 DAM_CONTAINER 换容器名。
#  - 无 docker / 无 dam-dev（CI 干净环境）：退回直跑——`cargo run --release
#    -p dev-e2e --bin pack_lint`，相对路径按 repo 根解析。判定同一套代码，
#    只是绕开容器。
set -uo pipefail
cd "$(dirname "$0")/.."
if [ "$#" -eq 0 ]; then
  echo "用法: bash scripts/pack_lint.sh <pack 目录> [<pack 目录> ...]" >&2
  exit 2
fi
ARGS=""
for d in "$@"; do ARGS="$ARGS $(printf '%q' "$d")"; done

CONTAINER="${DAM_CONTAINER:-dam-dev}"
BIN=/cargo-target/dam/release/pack_lint
have_docker_target() {
  command -v docker >/dev/null 2>&1 || return 1
  docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null | grep -q true || return 1
  docker exec "$CONTAINER" test -x "$BIN" 2>/dev/null || return 1
  return 0
}

if have_docker_target; then
  docker exec -e HTTP_PROXY= -e HTTPS_PROXY= -e http_proxy= -e https_proxy= "$CONTAINER" \
    bash -lc "cd /workspace && $BIN$ARGS"
else
  echo "note: dam-dev 容器不可用，直跑 cargo run --release -p dev-e2e --bin pack_lint" >&2
  exec cargo run --quiet --release -p dev-e2e --bin pack_lint -- "$@"
fi
