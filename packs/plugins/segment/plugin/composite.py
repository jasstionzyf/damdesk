#!/usr/bin/env python3
"""掩码 → alpha → 合成（纯标准库，确定性）。

三层口径（每一层都可单独断言，单测直接打这三层）：

1. **二值化**：宿主 `/detect/u2net` 回的是 320² 网格最近邻放大后的掩码（0/255），
   `edgeThreshold` 只决定「多少算前景」，`bytes.translate` 一次过（C 速度）。
2. **抗锯齿**：把二值掩码按宿主网格尺度 `k` 降采样再**双线性**升回全分辨率 → 边缘
   从硬台阶变成一格宽的线性斜坡。宿主网格是 320²、图宽 6016 时 `k≈19px`，所以
   边缘精度是「宿主网格级」——这是宿主能力的边界，不是本包懒（见 README「已知边界」）。
3. **羽化 `feather`**：把降采样网格放大 `k+feather` 再升采样 —— 斜坡宽度随 feather
   线性变宽（`feather=0` ⇒ 斜坡 ≈ 1 格，`feather=8` ⇒ ≈ 1 格 + 8px）。

性能口径：逐像素 Python 只出现在**斜坡段**（边缘一圈），内部纯色区一律走
`bytearray` 切片（C 速度）；10~24MP 的实测见 README「性能」。
"""
from __future__ import annotations

GRID = 320          # 宿主 u2net 掩码网格边（engine-segment::SIDE）
_MASK_ON = 255


def cell_size(width, height):
    """宿主掩码的一格在当前裁剪区里有多少像素（正方形网格映射到 w×h）。"""
    side = min(width, height)
    return max(1, int(round(side / float(GRID))))


def binarize(mask_plane, threshold):
    """`threshold` 以上算前景 → 0/255 平面（单次 translate，C 速度）。"""
    thr = max(0, min(255, int(threshold)))
    table = bytes(255 if v >= thr else 0 for v in range(256))
    return mask_plane.translate(table)


def _coarse(binary, width, height, k):
    """按 k 像素格点采样 → 粗网格（`nj × ni`），返回 `(rows, nj, ni)`。"""
    nj = (width + k - 1) // k
    ni = (height + k - 1) // k
    rows = []
    for i in range(ni):
        y = min(height - 1, i * k + k // 2)
        base = y * width
        rows.append(bytes(binary[base + min(width - 1, j * k + k // 2)] for j in range(nj)))
    return rows, nj, ni


def build_alpha(binary, width, height, feather=0):
    """二值 → 双线性插值的 alpha（0..255）。`feather` 只加宽斜坡。"""
    feather = max(0, min(64, int(feather)))
    if feather == 0:
        # 不羽化：仍按宿主网格插值（否则台阶就是 19px 的锯齿）
        k = cell_size(width, height)
    else:
        k = cell_size(width, height) + feather
    if k <= 1 or width < 3 or height < 3:
        return bytearray(binary)
    rows, nj, ni = _coarse(binary, width, height, k)
    alpha = bytearray(width * height)
    for y in range(height):
        i0 = y // k
        ty = y - i0 * k
        r0 = rows[i0]
        r1 = rows[min(i0 + 1, ni - 1)]
        base = y * width
        for j in range(nj):
            x0 = j * k
            x1 = min(width, x0 + k)
            if x1 <= x0:
                continue
            a00 = r0[j]
            a01 = r0[j + 1] if j + 1 < nj else a00
            a10 = r1[j]
            a11 = r1[j + 1] if j + 1 < nj else a10
            if a00 == a01 == a10 == a11:
                alpha[base + x0:base + x1] = bytes([a00]) * (x1 - x0)
                continue
            for x in range(x0, x1):
                tx = x - x0
                h0 = a00 + ((a01 - a00) * tx) // k
                h1 = a10 + ((a11 - a10) * tx) // k
                alpha[base + x] = h0 + ((h1 - h0) * ty) // k
    return alpha


def _runs(row, width):
    """`row` 里等值连续段 `(start, end, value)`（逐段 yield）。"""
    start = 0
    cur = row[0]
    for x in range(1, width):
        v = row[x]
        if v != cur:
            yield start, x, cur
            start, cur = x, v
    yield start, width, cur


def blend_rgb(src, width, height, alpha, bg):
    """不透明背景合成：`out = src*a + bg*(1-a)`（bg 为 (r,g,b)）。"""
    out = bytearray(len(src))
    br, bgc, bb = bg
    for y in range(height):
        line = y * width
        srow = line * 3
        arow = alpha[line:line + width]
        for start, end, a in _runs(arow, width):
            s0 = srow + start * 3
            s1 = srow + end * 3
            if a == 255:
                out[s0:s1] = src[s0:s1]
            elif a == 0:
                span = end - start
                out[s0:s1] = (bytes([br, bgc, bb]) * span)
            else:
                inv = 255 - a
                for i in range(s0, s1, 3):
                    out[i] = (src[i] * a + br * inv + 127) // 255
                    out[i + 1] = (src[i + 1] * a + bgc * inv + 127) // 255
                    out[i + 2] = (src[i + 2] * a + bb * inv + 127) // 255
    return out


def blend_rgba(src, width, height, alpha):
    """透明背景合成：RGB 保留**非预乘**原色，alpha = 掩码斜坡（无白边）。"""
    depth = width * height
    out = bytearray(depth * 4)
    out[0::4] = src[0::3]
    out[1::4] = src[1::3]
    out[2::4] = src[2::3]
    out[3::4] = alpha
    return out


def alpha_ramp_px(binary, width, height, feather=0):
    """本次用的边缘斜坡宽度（像素）——写进 evidence，便于人工核对参数是否生效。"""
    feather = max(0, min(64, int(feather)))
    k = cell_size(width, height) + max(0, feather)
    return 2 * k if k > 1 and width >= 3 and height >= 3 else 0


def subject_change(src, out, alpha, width, height, tol=8):
    """主体区域（`alpha == 255` 的不透明核心）在合成后被改动的像素数
    → `(不透明像素, 被改动像素)`。

    为什么只数 `alpha == 255`：
    * `alpha == 0` 的像素**本来就该**被换成目标底色，改得越多越对；
    * `0 < alpha < 255` 的斜坡像素**本来就该**与背景色混合（羽化/抗锯齿的定义），
      把它们算成「改动」等于让守卫在每一张正常图上误报；
    * `alpha == 255` 的像素是「原色照搬」——它们被改动只可能是坐标映射错位、合成贴错位置、
      色彩被整体偏移这类**真 bug**。

    容差 `tol` 兜住 JPEG/缩放往返的 1~2 级抖动。
    """
    depth = width * height
    subject = 0
    changed = 0
    for i in range(depth):
        if alpha[i] != 255:
            continue
        subject += 1
        base = i * 3
        if (abs(src[base] - out[base]) > tol
                or abs(src[base + 1] - out[base + 1]) > tol
                or abs(src[base + 2] - out[base + 2]) > tol):
            changed += 1
    return subject, changed

