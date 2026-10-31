#!/usr/bin/env bash
# D6 真图集采样：从本机真实图库目录只读拷贝 N 张真照片到 tests/fixtures/reals/。
#
# 为什么不用合成图：ONNX 嵌入/文搜图/去重的质量评估必须建立在真实照片上
# （纯色块/噪声图对语义排序与查重毫无意义）。这里的两处源目录都是生产数据的
# **只读**拷贝源，脚本不写入、不删除、不移动源文件。
#
# 用法：
#   bash scripts/sample_real_photos.sh                 # 默认 200 张 → tests/fixtures/reals/
#   bash scripts/sample_real_photos.sh 300 /tmp/reals  # 自定义张数/输出目录
#
# 抽样策略（可复现）：两个源目录各取一半，目录内用**固定步长**扫描头 400k 条目
# （readdir 顺序），只保留 magic bytes 校验通过的图片，落到 OUT 目录后写
# manifest.json（文件名/来源/字节数）与 README。
# 输出目录已存在时先清空（幂等重跑）。
#
# 环境变量覆盖源目录：D6_SJ_DIR / D6_DP_DIR
set -euo pipefail

N=${1:-200}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT=${2:-$ROOT/tests/fixtures/reals}
SJ_DIR=${D6_SJ_DIR:-/data}
DP_DIR=${D6_DP_DIR:-/data}

rm -rf "$OUT"
mkdir -p "$OUT"

python3 - "$N" "$OUT" "$SJ_DIR" "$DP_DIR" <<'PY'
import json, os, shutil, sys

n, out, sj_dir, dp_dir = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
MAGIC = {
    b"\xff\xd8\xff": ".jpg",
    b"\x89PNG\r\n\x1a\n": ".png",
}
SCAN_CAP = 400_000          # 每个源目录最多扫描的条目数（控制耗时）
MIN_BYTES = 4096

def kind(path):
    try:
        with open(path, "rb") as fh:
            head = fh.read(16)
    except OSError:
        return None
    for m, ext in MAGIC.items():
        if head.startswith(m):
            return ext
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return ".webp"
    if head[:6] in (b"GIF87a", b"GIF89a"):
        return ".gif"
    return None

def sample(src, want, tag):
    """固定步长扫描 src（头 SCAN_CAP 个条目，readdir 顺序），取 magic 校验通过的图片。"""
    picked, seen, i = [], 0, 0
    stride = 1
    with os.scandir(src) as it:
        for entry in it:
            if seen >= SCAN_CAP:
                break
            # 前 2000 个条目先探出步长，避免逐个 stat 40 万个文件
            if seen == 2000:
                stride = max(1, SCAN_CAP // max(1, want * 4))
            if seen % stride == 0 and len(picked) < want:
                seen += 1
                i += 1
                try:
                    if not entry.is_file(follow_symlinks=False) or entry.stat().st_size < MIN_BYTES:
                        continue
                except OSError:
                    continue
                if kind(entry.path):
                    stem = os.path.splitext(entry.name)[0]
                    picked.append((entry.path, f"{tag}-{len(picked):04d}-{stem}"))
                continue
            seen += 1
    if len(picked) < want:
        raise SystemExit(f"{src} 仅取到 {len(picked)}/{want} 张可用图片（扫描 {seen} 个条目）")
    return picked

half = n // 2
plan = sample(sj_dir, half, "sj") + sample(dp_dir, n - half, "dp")

manifest = []
for src, stem in plan:
    ext = kind(src)
    dst = os.path.join(out, stem + ext)
    shutil.copy2(src, dst)
    manifest.append({
        "file": os.path.basename(dst),
        "source": src,
        "bytes": os.path.getsize(dst),
    })

with open(os.path.join(out, "manifest.json"), "w") as fh:
    json.dump({"count": len(manifest), "sources": [sj_dir, dp_dir], "photos": manifest}, fh,
              ensure_ascii=False, indent=1)

total = sum(m["bytes"] for m in manifest)
print(f"采样 {len(manifest)} 张真图 → {out}（共 {total/1e6:.1f} MB）")
print(f"  sj 源 {sj_dir} × {half}；dp 源 {dp_dir} × {n - half}")
PY

cat > "$OUT/README.md" <<EOF
# tests/fixtures/reals — 真实照片采样集（D6）

由 \`scripts/sample_real_photos.sh\` 从本机图库目录**只读**采样生成（固定步长 + magic bytes 校验），
清单见 \`manifest.json\`。**不进 git**（见仓库 .gitignore）：原始版权不属于本仓库，且体积大；
需要复现时重跑脚本即可（源目录见脚本顶部）。

- 数量：$(ls "$OUT" | grep -vc -e README.md -e manifest.json) 张
- 生成时间：$(date '+%Y-%m-%d %H:%M:%S %z')
EOF

ls -la "$OUT" | head -5
echo "OK: reals in $OUT"
