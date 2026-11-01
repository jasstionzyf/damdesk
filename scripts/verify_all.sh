#!/usr/bin/env bash
# D7 链尾全链路复验（verify_all.sh）— 零信任口径：从空库跑通 D0-D6 全链。
#
# 用法：
#   宿主机（repo 根）：  bash scripts/verify_all.sh          # 起容器 → 容器内执行
#   容器内（/workspace）：bash scripts/verify_all.sh          # 跳过容器步，直接执行
#
# 步骤（任一步非零退出即停，逐步计时落 reports/logs/verify-*.log）：
#   0. 容器 dam-dev up
#   1. 构建 verify_all harness（release）
#   2. 全链路：清空库 → 建库(迁移) → 导入 fixture → mock 打标 → mock 嵌入(双塔)
#      → 三种搜索 → 查重(fileHash 精确 + 向量 near/variant) → 软删/恢复 → 清理
#   3. cargo test --workspace 全绿
#   4. 分层铁律 check_layers.sh
#
# 幂等：harness 每轮 rm -rf 工作目录从头重建，中断后重跑结果一致。
set -euo pipefail

IN_CONTAINER=0
[ -f /.dockerenv ] && IN_CONTAINER=1
cd "$(dirname "$0")/.."
LOG=reports/logs
mkdir -p "$LOG"
SECONDS=0
step() { echo; echo "==== [verify_all] $1 ===="; }

if [ "$IN_CONTAINER" -eq 0 ]; then
    step "0. 容器 dam-dev up"
    docker compose -f dev/docker-compose.yml up -d dam-dev | tee "$LOG/verify-container.log"
    run() { docker exec dam-dev bash -lc "$*"; }
else
    step "0. 已在容器内（跳过容器步）"
    run() { bash -lc "$*"; }
fi

step "1. 构建 verify_all harness（release）"
run 'cd /workspace && export CARGO_TARGET_DIR=/cargo-target/dam && cargo build --release -p dev-e2e --bin verify_all' 2>&1 | tee "$LOG/verify-build.log"

step "2. 全链路（清库→建库→导入→mock打标→mock嵌入→三搜索→查重→软删恢复→清理）"
if [ "$IN_CONTAINER" -eq 0 ]; then
    docker exec dam-dev /cargo-target/dam/release/verify_all 2>&1 | tee "$LOG/verify-all.log"
else
    /cargo-target/dam/release/verify_all 2>&1 | tee "$LOG/verify-all.log"
fi

step "3. cargo test --workspace 全绿"
# 注意：不能在容器内再接管道（会吃掉 cargo 退出码）；全量日志落盘，失败即 tail 停机
run 'cd /workspace && export CARGO_TARGET_DIR=/cargo-target/dam && cargo test --workspace' > "$LOG/verify-tests.log" 2>&1 || {
    tail -40 "$LOG/verify-tests.log"
    echo "FAIL: cargo test --workspace 存在失败（全文 $LOG/verify-tests.log）"
    exit 1
}
grep -cE "^test result: ok" "$LOG/verify-tests.log" | xargs -I{} echo "  ok 套件数：{}"
grep -E "0 failed" "$LOG/verify-tests.log" | tail -1

step "4. 分层铁律（check_layers.sh）"
run 'cd /workspace && bash scripts/check_layers.sh' 2>&1 | tee "$LOG/verify-layers.log"

echo
echo "VERIFY_ALL_SCRIPT: ALL PASS（总耗时 ${SECONDS}s，日志 $LOG/verify-*.log）"
