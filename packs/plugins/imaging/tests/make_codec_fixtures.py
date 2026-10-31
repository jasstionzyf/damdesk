#!/usr/bin/env python3
"""生成编码组（N6b）验收夹具：**从语料 v1 真图受控派生**，不是另造图。

为什么需要夹具（语料 v1 的缺口，实测记录）：
    - `labels/convert.json` 的 10 张源图**全都不带 alpha 通道**（cv_png_* 是 colortype=2），
      而卡要求「透明源转 png 必须保留 alpha」+ 贴 3 张 alpha 证据；
    - 语料里没有任何**带 EXIF 方向**的图，而卡要求「EXIF 方向处理正确」；
    - 语料只有 jpg/png，没有 avif 源。
所以这里用「语料真图 + 受控变换」补齐三类夹具（变换参数即 ground truth，
与 `build_plugin_corpus.py` 的 derived 同一做法）：

| 夹具 | 派生方式 | 期望 |
|---|---|---|
| `alpha/alpha_0{1,2,3}.png` | 语料 png 缩到长边 800 + 径向渐变 alpha（公式写死） | 转 png 后 alpha 通道逐像素一致 |
| `orient/orient_6.jpg` | 语料图**摆正后**逆时针转 90°存储 + EXIF Orientation=6 | 摆正后尺寸/像素 == `orient/ref_2.png` |
| `orient/orient_8.jpg` | 同上，顺时针转 90°存储 + Orientation=8 | 摆正后 == `orient/ref_3.png` |

跑法（**宿主机**，需要 Pillow；容器里没有也不该为测试装库，所以夹具随仓库提交）：

    python3 packs/plugins/imaging/tests/make_codec_fixtures.py
    python3 packs/plugins/imaging/tests/make_codec_fixtures.py --verify   # 只校验已生成夹具

夹具落 `packs/plugins/imaging/tests/fixtures/codec/`，元数据写 `manifest.json`
（E2E 只读 manifest，不在这里硬编码期望）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
OUTDIR = os.path.join(HERE, "fixtures", "codec")
CORPUS = os.environ.get("DAM_CORPUS", "/data/damdesk-test-corpus/v1")
ALPHA_LONG_EDGE = 800
ALPHA_SOURCES = [
    ("process/convert/cv_png_01.png", "alpha_01.png"),
    ("process/convert/cv_png_02.png", "alpha_02.png"),
    ("process/convert/cv_png_03.png", "alpha_03.png"),   # Adobe RGB 源，顺带验色彩空间
]
ORIENT_SOURCES = [
    ("process/convert/cv_webp_02.jpg", "orient_6.jpg", "ref_2.png", 6),
    ("process/convert/cv_webp_03.jpg", "orient_8.jpg", "ref_3.png", 8),
]


def _fit(im, long_edge):
    w, h = im.size
    if max(w, h) <= long_edge:
        return im
    scale = long_edge / float(max(w, h))
    return im.resize((max(1, round(w * scale)), max(1, round(h * scale))), Image.Resampling.LANCZOS)


def alpha_mask(w, h):
    """径向渐变 alpha：中心 255 → 四角 0（公式写死，可逐像素复算）。

    `a = round(255 * (1 - (d/dmax)**1.5))`，`d` = 到中心的欧氏距离，`dmax` = 到四角。
    结果必须满足：既有 255（不透明）也有 0（全透明）也有中间值 —— E2E 靠这个判"真保留"。
    """
    cx, cy = (w - 1) / 2.0, (h - 1) / 2.0
    dmax = (cx * cx + cy * cy) ** 0.5 or 1.0
    out = []
    for y in range(h):
        for x in range(w):
            d = (((x - cx) ** 2 + (y - cy) ** 2) ** 0.5) / dmax
            a = int(round(255 * max(0.0, 1.0 - d ** 1.5)))
            out.append(max(0, min(255, a)))
    return out


def build():
    os.makedirs(os.path.join(OUTDIR, "alpha"), exist_ok=True)
    os.makedirs(os.path.join(OUTDIR, "orient"), exist_ok=True)
    manifest = {"corpus": CORPUS, "alpha": [], "orient": []}

    for rel, name in ALPHA_SOURCES:
        src = os.path.join(CORPUS, rel)
        if not os.path.isfile(src):
            raise SystemExit("语料缺文件：%s（先跑 build_plugin_corpus.py --verify）" % src)
        im = _fit(Image.open(src).convert("RGB"), ALPHA_LONG_EDGE)
        w, h = im.size
        rgba = im.convert("RGBA")
        mask = alpha_mask(w, h)
        px = rgba.load()
        for y in range(h):
            for x in range(w):
                r, g, b, _ = px[x, y]
                px[x, y] = (r, g, b, mask[y * w + x])
        dest = os.path.join(OUTDIR, "alpha", name)
        rgba.save(dest, "PNG")
        vals = mask
        manifest["alpha"].append({
            "file": "alpha/" + name,
            "source": rel,
            "width": w,
            "height": h,
            "alphaMin": min(vals),
            "alphaMax": max(vals),
            "alphaBelow255": sum(1 for v in vals if v < 255),
            "alphaZero": sum(1 for v in vals if v == 0),
            "derive": "语料真图缩到长边 %d + 径向渐变 alpha" % ALPHA_LONG_EDGE,
        })
        print("alpha  %s  %dx%d  alpha∈[%d,%d]  <255:%d  0:%d" % (
            dest, w, h, min(vals), max(vals),
            sum(1 for v in vals if v < 255), sum(1 for v in vals if v == 0)))

    for rel, name, ref_name, orientation in ORIENT_SOURCES:
        src = os.path.join(CORPUS, rel)
        if not os.path.isfile(src):
            raise SystemExit("语料缺文件：%s" % src)
        upright = _fit(Image.open(src).convert("RGB"), 900)
        ref = os.path.join(OUTDIR, "orient", ref_name)
        upright.save(ref, "PNG")                       # 无损参考（摆正后的真值）
        # 存储像素 = 摆正图再转回去，让 EXIF 方向成为"唯一"的正确来源
        stored = upright.transpose(
            Image.Transpose.ROTATE_90 if orientation == 6 else Image.Transpose.ROTATE_270)
        exif = Image.Exif()
        exif[274] = orientation                       # 0x0112 Orientation
        dest = os.path.join(OUTDIR, "orient", name)
        stored.save(dest, "JPEG", quality=95, exif=exif)
        back = Image.open(dest)
        manifest["orient"].append({
            "file": "orient/" + name,
            "ref": "orient/" + ref_name,
            "source": rel,
            "orientation": orientation,
            "storedSize": list(back.size),
            "uprightSize": list(upright.size),
            "exifOrientation": back.getexif().get(274),
            "derive": "语料真图摆正后转 90° 存储 + EXIF Orientation=%d" % orientation,
        })
        print("orient %s  存储 %s / 摆正 %s  EXIF=%s" % (
            dest, back.size, upright.size, back.getexif().get(274)))

    manifest["generatedBy"] = "make_codec_fixtures.py"
    manifest["digest"] = _digest()
    with open(os.path.join(OUTDIR, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, ensure_ascii=False, indent=1)
        fh.write("\n")
    print("manifest：%s" % os.path.join(OUTDIR, "manifest.json"))
    return 0


def _digest():
    """夹具目录的内容摘要（判"夹具被手改过"，不进 manifest 自身）。"""
    lines = []
    for root, _dirs, files in os.walk(OUTDIR):
        for f in sorted(files):
            if f == "manifest.json":
                continue
            p = os.path.join(root, f)
            rel = os.path.relpath(p, OUTDIR)
            with open(p, "rb") as fh:
                lines.append("%s %s" % (hashlib.sha256(fh.read()).hexdigest(), rel))
    lines.sort()
    return hashlib.sha256("\n".join(lines).encode()).hexdigest()


def verify():
    path = os.path.join(OUTDIR, "manifest.json")
    if not os.path.isfile(path):
        raise SystemExit("夹具未生成：跑 python3 %s" % os.path.abspath(__file__))
    with open(path, encoding="utf-8") as fh:
        manifest = json.load(fh)
    if manifest.get("digest") != _digest():
        raise SystemExit("夹具被改动过（digest 不符）——重跑本脚本重新生成")
    for item in manifest["alpha"] + manifest["orient"]:
        p = os.path.join(OUTDIR, item["file"])
        if not os.path.isfile(p):
            raise SystemExit("缺夹具文件：%s" % p)
    for item in manifest["orient"]:
        if item.get("exifOrientation") != item["orientation"]:
            raise SystemExit("夹具 EXIF 方向不对：%s" % item["file"])
    print("夹具校验通过：alpha %d 张 / orient %d 张（digest %s）" % (
        len(manifest["alpha"]), len(manifest["orient"]), manifest["digest"][:12]))
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--verify", action="store_true", help="只校验已生成的夹具")
    args = ap.parse_args()
    return verify() if args.verify else build()


if __name__ == "__main__":
    sys.exit(main())
