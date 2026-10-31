#!/usr/bin/env bash
# 分层铁律检查（PLAN.md §2.1）：L3(app-tauri) → L2(svc-*) → L1(engine-*) → L0(core-*)
# 单向向下、同层禁止互引。解析各 crate Cargo.toml 的 workspace 内部依赖，违规即失败。
set -euo pipefail
cd "$(dirname "$0")/.."

declare -A LAYER=(
  [core-types]=0 [core-storage]=0 [core-hash]=0 [core-media]=0 [core-config]=0
  [engine-embed]=1 [engine-tag]=1 [engine-classic]=1 [engine-jobq]=1 [engine-search]=1 [engine-dedup]=1 [engine-segment]=1 [engine-restore]=1 [engine-exif]=1 [engine-ocr]=1
  [svc-import]=2 [svc-pipeline]=2 [svc-sync]=2 [svc-library]=2
  # 行业包宿主内核（IP-K02）：与 svc-* 同层 —— 它是「能力网关 + 子进程生命周期」的
  # 基础设施，被 svc/编排层向上使用；自身只依赖 L0/L1，不依赖任何 svc-*（避免同层互引）。
  [pack-host]=2
  [app-tauri]=3 [app-web]=3 [dev-e2e]=3
)

fail=0
for crate_dir in crates/*/; do
  crate=$(basename "$crate_dir")
  my=${LAYER[$crate]:-}
  if [[ -z "$my" ]]; then
    echo "UNKNOWN-CRATE: $crate（未登记层级，请更新 scripts/check_layers.sh）"
    fail=1
    continue
  fi
  # 提取 [dependencies] 段里的内部依赖（workspace.dependencies 的 key 集合）
  deps=$(awk '/^\[dependencies\]/{flag=1;next}/^\[/{flag=0}flag' "$crate_dir/Cargo.toml" \
    | grep -oE '^[a-zA-Z0-9_-]+' | sort -u)
  for d in $deps; do
    # core-types 豁免：唯一全共享类型层，任何层可用（PLAN.md §2.1）
    [[ "$d" == "core-types" && "$crate" != "core-types" ]] && continue
    # W7 豁免：app-tauri(L3) → app-web(L3) —— R3 终态，Tauri 壳把 web 服务编进同一
    # 进程（localhost 随机端口 + 一次性 token），web/desktop 前端一套 bundle。
    # W7+：壳只多一项原生能力（tauri-plugin-dialog / dialog:allow-open），不新增层级
    # 依赖，故此处仍无需额外豁免条目。
    [[ "$crate" == "app-tauri" && "$d" == "app-web" ]] && continue
    dl=${LAYER[$d]:-}
    [[ -z "$dl" ]] && continue           # 外部依赖不管
    if (( dl >= my )); then
      echo "LAYER-VIOLATION: $crate(L$my) -> $d(L$dl)（同层或向上引用禁止）"
      fail=1
    fi
  done
done

if (( fail )); then
  echo "check_layers: FAILED"
  exit 1
fi
echo "check_layers: OK（L3→L2→L1→L0 单向依赖全部合规）"
