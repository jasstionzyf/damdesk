#!/usr/bin/env bash
# WIN-PRE3 / DESKTOP-THUMB-PORTRAIT-1 一键验收：缩略图竖图语义双后端统一（长边 ≤ size）。
# 在构建容器内跑（工作区挂载在 /workspace），例：
#   docker exec dam-thumb-build bash -lc \
#     'cd /workspace && CARGO_TARGET_DIR=/cargo-target bash scripts/thumb_acceptance.sh'
#
# 步骤：
#   1) 默认构建（Linux = vips 后端）跑 core-media 全部单测；
#   2) --features rustimg-backend 强制启用纯 Rust 后端（Windows 目标平台同款路径）再跑一遍；
#   3) 同一张 800×3000 竖图在两后端各出一张 256 档缩略图 → 打印尺寸表，
#      断言两后端一致且均为长边语义（68×256）。
#      vl_image 短边 512 归一契约回归（vl_jpeg_short_side_512_contract_unaffected）
#      已包含在两轮单测里。全部通过 → 退出码 0。
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${THUMB_ACCEPTANCE_DIR:-$PWD/reports/logs/thumb-acceptance-$(date +%Y%m%d-%H%M%S)}"
# 测试二进制的 cwd 是 crate 目录（/workspace/crates/core-media），必须用绝对路径才能对上
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
mkdir -p "$OUT"
export THUMB_ACCEPTANCE_DIR="$OUT"

echo "== [1/3] vips 后端（Linux 默认构建）core-media 单测 =="
cargo test -p core-media

echo
echo "== [2/3] rustimg 后端（--features rustimg-backend，Windows 同款纯 Rust 路径）core-media 单测 =="
cargo test -p core-media --features rustimg-backend

echo
echo "== [3/3] 竖图 800×3000 + 256 两后端尺寸表与一致性断言 =="
python3 - "$OUT" <<'PY'
import glob
import json
import os
import sys

out = sys.argv[1]
rows = {}
for f in sorted(glob.glob(os.path.join(out, "result-*.json"))):
    with open(f) as fh:
        d = json.load(fh)
    rows[d["backend"]] = d

missing = {"vips", "rustimg"} - set(rows)
assert not missing, f"两后端结果不齐，缺: {sorted(missing)}（out={out}）"
v, r = rows["vips"], rows["rustimg"]
print(f"{'源图':<12} {'档位':<6} | {'vips 出图':<10} | {'rustimg 出图':<12} | 一致")
print(f"{v['src']:<12} {v['size']:<6} | {v['out']:<10} | {r['out']:<12} | {v['out'] == r['out']}")
assert v["out"] == r["out"], f"两后端不一致: vips={v['out']} rustimg={r['out']}"
ow, oh = (int(x) for x in v["out"].split("x"))
assert max(ow, oh) == v["size"], f"非长边语义: {v['out']}（档位 {v['size']}）"
print(f"PASS: 两后端一致 = {v['out']}，长边语义成立（长边 {max(ow, oh)} = 档位 {v['size']}，短边 {min(ow, oh)}）")
PY

echo
echo "== 产物存证 =="
ls -l "$OUT"
echo
echo "THUMB-ACCEPTANCE ALL PASS（报告目录: $OUT）"
