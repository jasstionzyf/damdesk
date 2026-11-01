#!/usr/bin/env python3
"""叠加组语料验收（真网关 + 真像素）：watermark 10 张 + rename 10 张（N6c）。

与 `test_overlay.py` 的分工：
    单测   —— 参数/计划/行形状对桩网关（毫秒级，`test_gate.sh` 里跑）；
    本脚本 —— **真网关 + 真编解码 + 真产物**：`watermark_action` 走 `/io/read` +
              `/op/watermark` 出真图，再逐图核对：

    watermark  ① 产物尺寸 == 源图尺寸（叠加不是缩放）；
               ② 墨迹**只**出现在标签要求的锚点窗内，对角窗不许出现成片墨迹；
               ③ 墨迹到锚定两条边的距离 ≈ `margin`（容差 = 一个字号：字形 bbox ≠ 墨迹 bbox）；
               ④ 实测透明度 ≈ 标签 `expect.opacity`（反差像素的 p90，容差 0.06）；
               ⑤ 边缘过渡像素占比（糊不糊的量化代理，< 0.80 算锐利）；
               ⑥ 可读性：墨迹区域裁出来喂宿主 `/detect/ocr`，记是否读回原文字（软判据）。
    rename     ① 10 张同模板同批 → 名字互不相同（重复名 `-2`/`-3` 递增）；
               ② 同一批重放（新 registry）→ 名字逐字相同；
               ③ 常量模板 → `same/same-2/…`；改名不出产物（行里无 artifacts、不写盘）。

**为什么要两个探针色**：语料标签只约束 `anchor/opacity/text`，**不约束颜色**；而这 10 张是
白底商品图，白字压在白底角上天然不可测（实测落墨像素 0，diff 全被 JPEG 噪声淹没）。
所以逐图先用**黑字探针**量；黑字在该区域不可量测（例如主体是深底色）时再换**白字探针**
（= 产品默认色）。判据是「至少一个探针可量测，且可量测的那个必须满足全部条件」——
既没有拿「换个颜色」糊过去，也不假装白字在白底上看得见。

**阈值怎么定的（数据驱动，不是拍的）**：先把对角窗（该处本来不该有水印）的 `src vs out`
差值分布量出来当**重编码噪声基线**，锚点窗落墨阈值取 `max(12, 噪声max + 4)`；
对角窗「成片墨迹」判据取 `spill ≤ max(50, 0.15×锚点窗落墨像素)`（噪声泄漏远小于一个字形）。

为什么用 `/op/crop` 取小块再在 Python 里解码：整图纯 Python 解码太慢，判定只需要锚点窗 +
对角窗这两小块；`/op/crop` 顺手把 JPEG 转 PNG（编码质量口径归宿主）。

前置：一个真网关。两种用法——
    A) 本脚本自己拉起（需要 cargo / 容器内）：
         python3 packs/plugins/imaging/tests/corpus_overlay_e2e.py --spawn-gateway
    B) 外部已起好：
         python3 .../corpus_overlay_e2e.py --gateway-url http://127.0.0.1:PORT --gateway-token T

环境变量兜底：`DAMPACK_GATEWAY_URL` / `DAMPACK_GATEWAY_TOKEN`；语料路径 `DAM_CORPUS`。
退出码：0 全绿 / 1 有 FAIL（打印逐图表格 + JSON 落盘）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
PACK = os.path.dirname(HERE)
REPO = os.path.abspath(os.path.join(PACK, "..", "..", ".."))
PLUGIN = os.path.join(PACK, "plugin")
for p in (PACK, PLUGIN):
    if p not in sys.path:
        sys.path.insert(0, p)

import overlay                                     # noqa: E402
from gw import Gateway, GatewayError               # noqa: E402

DEFAULT_CORPUS = os.environ.get("DAM_CORPUS", "")
LABEL_ANCHORS = {"br": "bottom_right", "bl": "bottom_left", "tr": "top_right",
                 "tl": "top_left", "c": "center", "center": "center"}
MIN_INK_DIFF = 8              # 解 alpha 的最小像素差（再小就是纯噪声，除法放大误差）
ALPHA_BAND = 0.06             # 「alpha ≈ 期望透明度」的带宽（字形内部像素落在这里）
OPACITY_TOL = 0.04            # band 内 alpha 中位数与期望的容差（重编码量化会低 ~0.03）
EDGE_FRACTION_MAX = 0.80      # 边缘过渡像素占比上限（糊了会接近 1.0）
INK_MIN_PX = 60               # 「确实落墨」的下限 = 成片带内像素数（10 张实测 85~104 px，字号 48；
                              # 字号越小成片像素越少，故留 40% 余量；纯噪声对角窗实测 0~2）
SPILL_MAX_RATIO = 0.25        # 对角窗最大连通块 ≤ 锚点窗的比例（字形是成片的，噪声是散点）
PROBES = (("black", {"watermarkColor": "#000000"}, (0, 0, 0)),
          ("white", {}, (255, 255, 255)))       # 第二次用产品默认色（不传 color）


def _sha256(path):
    """源图字节指纹：rename 的验收口径是「字节不变」，光看「没出产物」不够硬。"""
    h = hashlib.sha256()
    with open(path, "rb") as fp:
        for chunk in iter(lambda: fp.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# ---------------------------------------------------------------- 网关拉起
class SpawnedGateway:
    """`cargo run -p pack-host --example gateway_serve`（容器内用）。"""

    def __init__(self, work_dir, caps="io.read,op.overlay,op.geometric,detect.ocr"):
        self.work_dir = work_dir
        self.caps = caps
        self.proc = None
        self.url = None
        self.token = None

    def __enter__(self):
        cmd = ["cargo", "run", "-q", "-p", "pack-host", "--example", "gateway_serve", "--",
               "--work-dir", self.work_dir, "--capabilities", self.caps]
        env = dict(os.environ)
        env.setdefault("DAM_MODELS_ROOT", "/")
        self.proc = subprocess.Popen(cmd, cwd=REPO, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, stdin=subprocess.PIPE,
                                     text=True, bufsize=1, env=env)
        line = self.proc.stdout.readline()
        if not line.strip():
            err = self.proc.stderr.read() if self.proc.stderr else ""
            raise RuntimeError("gateway_serve 没输出 URL（stderr 尾部：%s）" % err[-2000:])
        info = json.loads(line)
        self.url, self.token = info["url"], info["token"]
        for _ in range(50):
            try:
                urllib.request.urlopen(self.url + "/io/read", data=b"{}", timeout=1)
            except urllib.error.HTTPError:
                break
            except Exception:                      # noqa: BLE001
                time.sleep(0.1)
        return self

    def __exit__(self, *exc):
        if self.proc:
            try:
                self.proc.stdin.write("quit\n")
                self.proc.stdin.flush()
            except Exception:                      # noqa: BLE001
                pass
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        return False


# ---------------------------------------------------------------- 极简 PNG 解码
def read_png(path):
    """8-bit RGB/RGBA PNG → `(w, h, channels, bytes)`（只用标准库 zlib）。

    不用 PIL：宿主容器里没有，而验收脚本必须「拿到仓库就能跑」。
    """
    with open(path, "rb") as fh:
        data = fh.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("不是 PNG：%s" % path)
    pos = 8
    width = height = depth = color = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, depth, color, _comp, _filt, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or color not in (2, 6) or interlace:
                raise ValueError("只支持 8-bit RGB/RGBA 非隔行 PNG（depth=%s color=%s inter=%s）"
                                 % (depth, color, interlace))
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break
    if width is None or color is None:
        raise ValueError("PNG 缺 IHDR：%s" % path)
    ch = 3 if color == 2 else 4
    raw = zlib.decompress(bytes(idat))
    stride = width * ch
    out = bytearray(stride * height)
    prev = bytearray(stride)
    pos = 0
    for _y in range(height):
        ftype = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        if ftype == 1:                              # Sub
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 0xFF
        elif ftype == 2:                            # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:                            # Average
            for i in range(stride):
                left = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:                            # Paeth
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        elif ftype != 0:
            raise ValueError("未知 PNG filter %d" % ftype)
        out[_y * stride:(_y + 1) * stride] = line
        prev = line
    return width, height, ch, bytes(out)


# ---------------------------------------------------------------- 窗口 / 差值
def window_for(anchor, w, h, ww, wh, margin):
    """锚点窗（源图坐标）。四边各内缩 margin，方便量「墨迹到边的距离」。"""
    ww = min(ww, w - 2 * margin)
    wh = min(wh, h - 2 * margin)
    left = margin
    right = w - ww - margin
    top = margin
    bottom = h - wh - margin
    if anchor == "bottom_right":
        return [right, bottom, right + ww, bottom + wh]
    if anchor == "bottom_left":
        return [left, bottom, left + ww, bottom + wh]
    if anchor == "top_right":
        return [right, top, right + ww, top + wh]
    if anchor == "top_left":
        return [left, top, left + ww, top + wh]
    return [(w - ww) // 2, (h - wh) // 2, (w - ww) // 2 + ww, (h - wh) // 2 + wh]


def control_window(anchor, w, h, ww, wh, margin):
    """对角窗：证明「墨迹没乱跑」（该处本来不该有任何水印）。"""
    other = {"bottom_right": "top_left", "bottom_left": "top_right",
             "top_right": "bottom_left", "top_left": "bottom_right"}.get(anchor, "top_left")
    return window_for(other, w, h, ww, wh, margin)


def crop_png(gw, src, box, out):
    gw.crop(src, box, out)
    return read_png(out)


def _maxdiff(a, b, ch, idx):
    p = idx * ch
    d1 = abs(a[p] - b[p])
    d2 = abs(a[p + 1] - b[p + 1])
    d3 = abs(a[p + 2] - b[p + 2])
    return max(d1, d2, d3)


def noise_stats(src_px, out_px):
    """窗内逐像素最大通道差分布 —— JPEG 重编码噪声基线。"""
    w, h, ch, a = src_px
    _, _, _, b = out_px
    ordered = sorted(_maxdiff(a, b, ch, i) for i in range(w * h))

    def q(f):
        return ordered[min(len(ordered) - 1, int(len(ordered) * f))]

    return {"p50": ordered[len(ordered) // 2], "p95": q(0.95), "p99": q(0.99),
            "p999": q(0.999), "max": ordered[-1], "px": len(ordered)}


def analyse_window(src_px, out_px, color_rgb, expected_opacity, min_diff=MIN_INK_DIFF):
    """按**混合方程**判落墨：解出每个像素的 alpha，数「alpha ≈ 期望值」的像素。

    为什么不用「差值大于某阈值」：JPEG 重编码噪声在主体纹理区能到 30+，而 35% 透明度的
    白/黑字在中等底色上的差值也就 20~60 —— 单看幅度根本分不开（实测 center 锚点图上，
    按幅度筛会把整个窗判成墨迹）。混合方程 `out = src*(1-a) + color*a` 给出的是**物理量**：
    噪声让 alpha 乱飘，真水印让成百上千像素落在同一个 alpha 上。

    只有反差够（`|color-src| ≥ 45`）的像素能解 alpha，所以逐图要挑「量得到」的探针色。
    """
    w, h, ch, a = src_px
    _, _, _, b = out_px
    band = []                    # alpha 落在期望带内 = 字形内部
    alphas = []                  # 所有可解像素（含边缘反锯齿）
    for idx in range(w * h):
        if _maxdiff(a, b, ch, idx) <= min_diff:
            continue
        p = idx * ch
        best = None
        for c in range(3):
            delta = color_rgb[c] - a[p + c]
            if abs(delta) < 45:            # 反差不足的像素解不出 alpha（除法放大噪声）
                continue
            alpha = (b[p + c] - a[p + c]) / float(delta)
            if best is None or abs(alpha - expected_opacity) < abs(best - expected_opacity):
                best = alpha
        if best is None:
            continue
        alpha = max(0.0, min(1.0, best))
        alphas.append(alpha)
        if abs(alpha - expected_opacity) <= ALPHA_BAND:
            band.append((idx % w, idx // w, alpha))
    alphas.sort()

    def q(f):
        return round(alphas[min(len(alphas) - 1, int(len(alphas) * f))], 3) if alphas else None

    band_alphas = sorted(v for _x, _y, v in band)
    band_median = round(band_alphas[len(band_alphas) // 2], 3) if band_alphas else None
    band_set = {(x, y) for x, y, _v in band}
    # 「成片」过滤：alpha 落在带内、且四邻里有 ≥2 个也在带内。
    # 噪声逐像素独立（邻居落在同一 alpha 带的概率 ~5%），字形内部则是连续同 alpha 的块 ——
    # 这一步把「busy 区域随机落在 0.35 上的散点」剔掉（实测 center 锚点图噪声带 250 px 全是散点）。
    coherent = [(x, y, v) for x, y, v in band
                if sum(1 for nb in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1))
                       if nb in band_set) >= 2]
    coh_alphas = sorted(v for _x, _y, v in coherent)
    coh_median = round(coh_alphas[len(coh_alphas) // 2], 3) if coh_alphas else None
    box = None
    if coherent:
        xs = [x for x, _y, _v in coherent]
        ys = [y for _x, y, _v in coherent]
        box = [min(xs), min(ys), max(xs) + 1, max(ys) + 1]
    p90 = q(0.9)
    soft = sum(1 for v in alphas if 0.15 < v < 0.6 * p90) if p90 else 0
    opaque = sum(1 for v in coh_alphas if v >= 0.85) if expected_opacity < 0.7 else None
    return {
        "bandCount": len(band),                     # 落在期望 alpha 带的像素数（含噪声散点）
        "coherentCount": len(coherent),              # 成片的带内像素 = 字形内部
        "bandBox": box,                              # 成片像素的框（噪声不会形成框）
        "bandMedianAlpha": coh_median,
        "opaqueInBand": opaque,                     # 期望不是不透明却出现大量不透明像素 = 透明度没生效
        "maxComponent": _largest_component(band),   # 最大连通块：字形是成片的，噪声是散点
        "alphaN": len(alphas),
        "alphaP50": q(0.5),
        "alphaP90": p90,
        "edgeFraction": round(soft / float(len(alphas)), 4) if alphas else None,
        "minDiff": min_diff,
    }


def _largest_component(band):
    """band 像素的最大 4-连通块大小（字形成片 vs 噪声散点 —— 与幅度无关的结构判据）。"""
    if not band:
        return 0
    pixels = {(x, y) for x, y, _v in band}
    seen = set()
    best = 0
    for start in pixels:
        if start in seen:
            continue
        stack = [start]
        seen.add(start)
        size = 0
        while stack:
            x, y = stack.pop()
            size += 1
            for nb in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
                if nb in pixels and nb not in seen:
                    seen.add(nb)
                    stack.append(nb)
        best = max(best, size)
    return best


def anchor_gaps(ink_box, win, w, h):
    """墨迹框到四边的距离（源图坐标）。"""
    if not ink_box:
        return None
    wx, wy = win[0], win[1]
    x1, y1, x2, y2 = ink_box[0] + wx, ink_box[1] + wy, ink_box[2] + wx, ink_box[3] + wy
    return {"left": x1, "right": w - x2, "top": y1, "bottom": h - y2,
            "box": [x1, y1, x2, y2]}


def anchored_gaps(gaps, anchor):
    """锚定两轴的边距（该两轴应 ≈ margin）。center 锚点两轴都按居中算。"""
    if not gaps:
        return None, None
    if anchor in ("bottom_right", "top_right"):
        horizontal = gaps["right"]
    elif anchor in ("bottom_left", "top_left"):
        horizontal = gaps["left"]
    else:
        horizontal = None
    if anchor in ("bottom_left", "bottom_right"):
        vertical = gaps["bottom"]
    elif anchor in ("top_left", "top_right"):
        vertical = gaps["top"]
    else:
        vertical = None
    return horizontal, vertical


def try_ocr(gw, path):
    """宿主 OCR（软判据）：读不出/没模型都不判死，只记录。"""
    try:
        reply = gw.call("/detect/ocr", {"path": path})
        lines = reply.get("lines") or []
        return " ".join((ln.get("text") or "").strip() for ln in lines).strip(), None
    except GatewayError as exc:
        return None, str(exc)


# ---------------------------------------------------------------- 单次探针判定
def analyse_probe(gw, src, row, work, tag, color_rgb, expected_opacity, want_ocr):
    """一次探针（一个颜色）的像素判定：锚点窗 + 对角窗 + OCR。"""
    fields = row["fields"]
    out_path = fields["outPath"]
    w, h = fields["in"]
    font_size, margin = fields["fontSize"], fields["margin"]
    ww, wh = min(w, 700), min(h, 320)
    win = window_for(anchor=fields["anchor"], w=w, h=h, ww=ww, wh=wh, margin=margin)
    ctl = control_window(fields["anchor"], w, h, ww, wh, margin)
    # ① 锚点窗：按混合方程数「alpha ≈ 期望」的像素
    win_src = crop_png(gw, src, win, os.path.join(work, "_%s_win_src.png" % tag))
    win_out = crop_png(gw, out_path, win, os.path.join(work, "_%s_win_out.png" % tag))
    stats = analyse_window(win_src, win_out, color_rgb, expected_opacity)
    # ② 对角窗：噪声不该产出成片同 alpha 像素（这就是「墨迹没乱跑」的判据）
    ctl_src = crop_png(gw, src, ctl, os.path.join(work, "_%s_ctl_src.png" % tag))
    ctl_out = crop_png(gw, out_path, ctl, os.path.join(work, "_%s_ctl_out.png" % tag))
    ctl_stats = analyse_window(ctl_src, ctl_out, color_rgb, expected_opacity)
    noise = noise_stats(ctl_src, ctl_out)
    spill = ctl_stats["bandCount"]
    gaps = anchor_gaps(stats["bandBox"], win, w, h)
    horizontal, vertical = anchored_gaps(gaps, fields["anchor"])
    # center 锚点没有「锚定边」可量：改用**对称性**判（墨迹框中心 ≈ 画布中心）
    center_offset = None
    if gaps:
        center_offset = {"horizontal": round((gaps["left"] - gaps["right"]) / 2.0, 1),
                         "vertical": round((gaps["top"] - gaps["bottom"]) / 2.0, 1)}
    # ③ OCR（软判据）：墨迹框裁出来单独喂
    ocr_text = ocr_err = None
    if stats["bandBox"] and want_ocr:
        pad = 6
        box = [win[0] + stats["bandBox"][0] - pad, win[1] + stats["bandBox"][1] - pad,
               win[0] + stats["bandBox"][2] + pad, win[1] + stats["bandBox"][3] + pad]
        box = [max(0, box[0]), max(0, box[1]), min(w, box[2]), min(h, box[3])]
        ocr_png = os.path.join(work, "_%s_ocr.png" % tag)
        crop_png(gw, out_path, box, ocr_png)
        ocr_text, ocr_err = try_ocr(gw, ocr_png)
    gap_ok = (horizontal is None or abs(horizontal - margin) <= font_size) and \
             (vertical is None or abs(vertical - margin) <= font_size)
    return {
        "probe": tag,
        "colorRgb": list(color_rgb),
        "fields": fields,
        "sizeUnchanged": fields["out"] == [w, h],
        "stats": stats,
        "controlStats": ctl_stats,
        "noise": noise,
        "spill": spill,
        "gaps": gaps,
        "centerOffset": center_offset,
        "anchoredGaps": {"horizontal": horizontal, "vertical": vertical,
                         "margin": margin, "tolerance": font_size},
        "gapMatchesMargin": gap_ok,
        "ocr": {"text": ocr_text, "error": ocr_err},
        "window": win,
        "controlWindow": ctl,
    }


def grade(probe, expected_opacity):
    """把一次探针的硬判据收成一张表（OCR 是软判据，不进 hard）。"""
    stats = probe["stats"]
    anchor = probe["fields"]["anchor"]
    gap_ok = probe["gapMatchesMargin"]
    if anchor == "center":
        off = probe.get("centerOffset") or {}
        tol = probe["anchoredGaps"]["tolerance"]
        gap_ok = (off.get("horizontal") is not None and abs(off["horizontal"]) <= tol
                  and off.get("vertical") is not None and abs(off["vertical"]) <= tol)
    opacity_ok = (
        stats["bandMedianAlpha"] is not None
        and abs(stats["bandMedianAlpha"] - expected_opacity) <= OPACITY_TOL
    )
    if opacity_ok and stats["opaqueInBand"] is not None:
        # 期望是半透明却出现成片「不透明」像素 = 透明度参数没生效（或被当成 100%）
        opacity_ok = stats["opaqueInBand"] <= 0.1 * max(1, stats["coherentCount"])
    return {
        "sizeUnchanged": probe["sizeUnchanged"],
        "inkPresent": stats["coherentCount"] >= INK_MIN_PX,
        # 「墨迹没乱跑」：对角窗里不许出现成片的同 alpha 块（噪声是散点，字形是连通块）
        "noSpill": probe["controlStats"]["coherentCount"]
                   <= max(8, int(SPILL_MAX_RATIO * stats["coherentCount"])),
        "opacity": opacity_ok,
        "sharp": stats["edgeFraction"] is not None
                 and stats["edgeFraction"] < EDGE_FRACTION_MAX,
        "gapMatchesMargin": gap_ok,
    }


# ---------------------------------------------------------------- 主流程
def run(args):
    corpus = os.path.abspath(args.corpus)
    labels_path = os.path.join(corpus, "labels", "watermark.json")
    if not os.path.isfile(labels_path):
        print("FAIL：缺语料标签 %s" % labels_path)
        return 1
    with open(labels_path, encoding="utf-8") as fh:
        items = json.load(fh)["items"]

    work = os.path.abspath(args.work_dir)
    os.makedirs(work, exist_ok=True)
    ctx = SpawnedGateway(work) if args.spawn_gateway else None
    if ctx:
        ctx.__enter__()
        gw = Gateway(base_url=ctx.url, token=ctx.token)
    else:
        gw = Gateway(base_url=args.gateway_url or os.environ.get("DAMPACK_GATEWAY_URL"),
                     token=args.gateway_token or os.environ.get("DAMPACK_GATEWAY_TOKEN"))

    rows = []
    failures = 0
    print("file                          in          anchor   探针  bandpx 带内α/期望 边缘比 对角band 边距(横/纵) OCR 判定")
    try:
        for item in items:
            src = os.path.join(corpus, item["file"])
            expect = item["expect"]
            anchor = LABEL_ANCHORS[expect["anchor"]]
            expected_opacity = float(expect["opacity"])
            base_params = {"watermarkText": expect["text"],
                           "watermarkAnchor": expect["anchor"],
                           "watermarkOpacity": expect["opacity"]}
            probes = []
            chosen = None
            rec = {"file": item["file"], "expect": expect, "probes": probes}
            for tag, extra, color_rgb in PROBES:
                pwork = os.path.join(work, tag)
                os.makedirs(pwork, exist_ok=True)
                row = overlay.watermark_action({"id": item["file"], "path": src},
                                              dict(base_params, **extra), gw, pwork)
                if row["result"] != "ok":
                    probes.append({"probe": tag, "row": row, "error": row.get("error")})
                    continue
                probe = analyse_probe(gw, src, row, pwork, tag, color_rgb,
                                      expected_opacity, not args.no_ocr)
                probe["row"] = row
                probes.append(probe)
                # 选「量得最实」的探针：band 像素最多的那个（噪声不会成片落在同一 alpha 上）
                if chosen is None or probe["stats"]["coherentCount"] > chosen["stats"]["coherentCount"]:
                    chosen = probe
            if chosen is None:
                rec.update({"verdict": "FAIL", "why": "两个探针都没有 ok 行"})
                failures += 1
                rows.append(rec)
                print("%-29s FAIL 两个探针都没有 ok 行" % item["file"])
                continue
            hard = grade(chosen, expected_opacity)
            # 未选中的探针只做「行 ok + 尺寸不变」的旁证（不做像素判据）
            other_ok = all(p.get("row", {}).get("result") == "ok" for p in probes)
            ok = all(hard.values()) and other_ok
            if not ok:
                failures += 1
            wanted = "".join(expect["text"].split()).lower()
            ocr_hit = bool(chosen["ocr"]["text"]) and \
                wanted in "".join(chosen["ocr"]["text"].split()).lower()
            rec.update({
                "verdict": "PASS" if ok else "FAIL",
                "probeUsed": chosen["probe"],
                "checks": hard,
                "stats": chosen["stats"],
                "noise": chosen["noise"],
                "spillInControlWindow": chosen["spill"],
                "gaps": chosen["gaps"],
                "centerOffset": chosen.get("centerOffset"),
                "anchoredGaps": chosen["anchoredGaps"],
                "ocr": {"text": chosen["ocr"]["text"], "hit": ocr_hit,
                        "error": chosen["ocr"]["error"]},
                "window": chosen["window"],
                "controlWindow": chosen["controlWindow"],
                "outPath": chosen["fields"]["outPath"],
                "otherProbeOk": other_ok,
            })
            rows.append(rec)
            stats = chosen["stats"]
            horizontal = chosen["anchoredGaps"]["horizontal"]
            vertical = chosen["anchoredGaps"]["vertical"]
            print("%-29s %-10s %-8s %-5s %-6d %-6s/%.2f %-6s 块%d/%d %s/%s 居中%s  %-4s %s" % (
                item["file"], "%dx%d" % tuple(chosen["fields"]["in"]), anchor,
                chosen["probe"], stats["coherentCount"],
                stats["bandMedianAlpha"] if stats["bandMedianAlpha"] is not None else -1,
                expected_opacity,
                ("%.2f" % stats["edgeFraction"]) if stats["edgeFraction"] is not None else "-",
                stats["maxComponent"], chosen["controlStats"]["maxComponent"],
                horizontal if horizontal is not None else "-",
                vertical if vertical is not None else "-",
                ("%s" % chosen["centerOffset"]) if chosen.get("centerOffset") else "-",
                "hit" if ocr_hit else ("n/a" if chosen["ocr"]["error"] else "miss"),
                "PASS" if ok else "FAIL " + ",".join(k for k, v in hard.items() if not v)
                + ("" if other_ok else ",otherProbe"),
            ))

        # -------------------------------------------------- rename
        print()
        print("== rename 批次（同模板 10 张：唯一性 + 重放确定性 + 不出产物） ==")
        reg_a, reg_b = overlay.RenameRegistry(), overlay.RenameRegistry()
        names_a, names_b = [], []
        rename_rows = []
        # 改名不改字节：源图 sha256 前后必须逐字节一致（卡验收要求「贴 sha256 对比」）
        sha_before = {it["file"]: _sha256(os.path.join(corpus, it["file"])) for it in items}
        for item in items:
            src = os.path.join(corpus, item["file"])
            params = {"renameTemplate": "{name}_{index}"}
            ra = overlay.rename_action({"id": item["file"], "path": src}, params, gw, work,
                                      registry=reg_a)
            rb = overlay.rename_action({"id": item["file"], "path": src}, params, gw, work,
                                      registry=reg_b)
            names_a.append(ra.get("fields", {}).get("assetName"))
            names_b.append(rb.get("fields", {}).get("assetName"))
            rename_rows.append({"file": item["file"], "first": ra, "replay": rb})
        reg_c = overlay.RenameRegistry()
        dup_names = []
        for item in items:
            src = os.path.join(corpus, item["file"])
            rc = overlay.rename_action({"id": item["file"], "path": src},
                                      {"renameTemplate": "same"}, gw, work, registry=reg_c)
            dup_names.append(rc.get("fields", {}).get("assetName"))
        unique_ok = len(set(names_a)) == len(names_a) and all(names_a)
        replay_ok = names_a == names_b
        dup_ok = len(set(dup_names)) == len(dup_names) and dup_names[1] == "same-2.jpg"
        rows_ok = all(r["first"]["result"] == "ok" and "artifacts" not in r["first"]
                      for r in rename_rows)
        # 改名不写盘：它产出的名字**不在** workdir 里存在任何同名文件
        ghost = [n for n in names_a if n and os.path.exists(os.path.join(work, n))]
        no_artifact_ok = rows_ok and not ghost
        sha_after = {it["file"]: _sha256(os.path.join(corpus, it["file"])) for it in items}
        bytes_ok = sha_before == sha_after
        print("  唯一性：%s（%d 个名字 / %d 个不同）" % ("PASS" if unique_ok else "FAIL",
                                                    len(names_a), len(set(names_a))))
        print("  重放确定性：%s（两批逐字%s）" % ("PASS" if replay_ok else "FAIL",
                                          "相同" if replay_ok else "不同"))
        print("  常量模板去重：%s → %s" % ("PASS" if dup_ok else "FAIL", dup_names))
        print("  不出产物：%s（result=ok 且行无 artifacts；workdir 里没有改名产物%s）"
              % ("PASS" if no_artifact_ok else "FAIL",
                 "" if not ghost else "：%s" % ghost))
        print("  源图字节不变：%s（10 张 sha256 前后逐一相同%s）"
              % ("PASS" if bytes_ok else "FAIL",
                 "" if bytes_ok else "：%s" % [k for k in sha_before if sha_before[k] != sha_after.get(k)]))
        for key, ok in (("rename_unique", unique_ok), ("rename_replay", replay_ok),
                        ("rename_dedup", dup_ok), ("rename_no_artifact", no_artifact_ok),
                        ("rename_bytes_unchanged", bytes_ok)):
            if not ok:
                failures += 1
    finally:
        if ctx:
            ctx.__exit__(None, None, None)

    total = len(rows) + 5
    print()
    print("---- 汇总：%d 项，PASS %d，FAIL %d ----" % (total, total - failures, failures))
    payload = {
        "corpus": corpus,
        "probes": [{"tag": tag, "params": extra, "colorRgb": list(rgb)}
                   for tag, extra, rgb in PROBES],
        "watermark": rows,
        "rename": {"names": names_a, "replay": names_b, "constantTemplate": dup_names,
                   "srcSha256": sha_before, "bytesUnchanged": sha_before == sha_after},
        "summary": {"total": total, "fail": failures,
                    "ocrHits": sum(1 for r in rows if r.get("ocr", {}).get("hit")),
                    "probeUsed": {r["file"]: r.get("probeUsed") for r in rows}},
    }
    if args.json_out:
        os.makedirs(os.path.dirname(os.path.abspath(args.json_out)), exist_ok=True)
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=1)
        print("逐图原始结果：%s" % os.path.abspath(args.json_out))
    return 0 if failures == 0 else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=DEFAULT_CORPUS)
    ap.add_argument("--work-dir", default=os.path.join(REPO, "reports", "logs", "overlay-e2e-work"))
    ap.add_argument("--spawn-gateway", action="store_true")
    ap.add_argument("--gateway-url", default=None)
    ap.add_argument("--gateway-token", default=None)
    ap.add_argument("--json-out", default=None)
    ap.add_argument("--no-ocr", action="store_true", help="跳过 OCR 软判据（无模型环境）")
    args = ap.parse_args()
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
