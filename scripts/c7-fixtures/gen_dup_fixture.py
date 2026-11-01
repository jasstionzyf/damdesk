#!/usr/bin/env python3
"""C7 spike：生成「重复检测」探针夹具（默认 /tmp/c7-dupfixture，由验收脚本 docker cp 进容器）。

夹具构成（全部来自真图 /tmp/c5-stage，不伪造图像内容）：
  r1.jpg         房源 A 视角 1（原始）
  r1_recomp.jpg  同一张图重存（JPEG q=55）        → 位不同、视觉相同（近似重复）
  r1_copy.jpg    同一张图字节级复制                → SHA-256 相同（精确重复）
  r2.jpg         另一张真图（模拟「同一房源的另一张照片 / 另一房间」）→ 视觉不同
  目的：量「视觉重复检测能力」在 L1 是否真的可用，并给出真实汉明距离；
        同时说明 pHash 抓不住「同房间不同角度」这类语义重复。

用法：python3 scripts/c7-fixtures/gen_dup_fixture.py [输出目录] [源图目录]
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

FFMPEG = os.environ.get("DAMDESK_FFMPEG", "ffmpeg")  # 个人路径不入库，env 指路


def main() -> int:
    dst = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/c7-dupfixture")
    src = Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/c5-stage")
    if not src.is_dir():
        print(f"源图目录不存在: {src}", file=sys.stderr)
        return 2
    imgs = sorted(p for p in src.iterdir() if p.suffix.lower() in (".jpg", ".jpeg", ".png"))
    if len(imgs) < 2:
        print(f"源图不足 2 张: {len(imgs)}", file=sys.stderr)
        return 2
    dst.mkdir(parents=True, exist_ok=True)
    for f in dst.iterdir():
        f.unlink()
    base, other = imgs[0], imgs[-1]
    shutil.copy2(base, dst / "r1.jpg")
    shutil.copy2(base, dst / "r1_copy.jpg")          # 字节级复制 → SHA-256 相同
    r = subprocess.run([FFMPEG, "-v", "error", "-y", "-i", str(base), "-q:v", "12",
                        str(dst / "r1_recomp.jpg")], capture_output=True)
    if r.returncode != 0:
        print("ffmpeg 重编码失败:", r.stderr.decode()[:300], file=sys.stderr)
        return 1
    shutil.copy2(other, dst / "r2.jpg")             # 另一张真图（视觉不同）
    print(f"夹具已生成 {dst}/: r1.jpg r1_copy.jpg r1_recomp.jpg r2.jpg"
          f"（源 {base.name} / {other.name}）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
