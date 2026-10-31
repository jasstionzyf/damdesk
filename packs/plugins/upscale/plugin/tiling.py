#!/usr/bin/env python3
"""分块超分：网格规划 + 拼接（纯几何/内存搬运，不含任何算法）。

为什么要分块：宿主 `engine-restore` 的 x4v3 有**单次输入护栏 4MP**（4× 输出 64MP f32），
超过它的图网关会退回重采样（静默降质）。分块让「大图也走模型」：按块送模型，块内向外留
`MARGIN` 边距（模型感受野吃边界），只把**去掉边距的内区**贴回画布 —— 于是拼缝落在
「用边距算出来的像素」上，而不是块边界上被截断的像素上。这是 Real-ESRGAN 官方 tiling
的同一思路，只是拼接放在包里做（协议没有 composite 端点）。

边界纪律：
    - 拼接只做 `bytes` 行拷贝（`canvas[dst:dst+n] = raw[src:src+n]`），不做插值/混合；
    - 画布尺寸是 `4×` 源图（模型固定 4×），最终 2× 由网关 `/op/upscale scale=0.5` 落；
    - 分块边长由参数 `tile` 决定，`auto` = 只在大图（> 模型护栏）时按 [`AUTO_BLOCK`] 分块。
"""
from __future__ import annotations

# 块的向外边距（源图像素）。8px 足够覆盖 SRVGGNetCompact 的边界效应，
# 再大只是多花模型算力（每块都重复算一遍边距）。
MARGIN = 8
# `tile=auto` 且图超过模型护栏时用的块边长（1536² ≈ 2.36MP < 4MP 护栏）
AUTO_BLOCK = 1536
# 画布像素硬上限（与网关 MAX_OUTPUT_PIXELS 同口径）：4× 画布超了就别做，别把机器打死
MAX_CANVAS_PIXELS = 100_000_000


def block_for(tile, width, height, model_cap):
    """`tile`（"auto" 或块边长）→ 实际块边长；`None` = 不分块（整图一次送模型）。

    分工：
        - `auto`：图 ≤ 模型护栏 → 不分块（与整图推理逐像素一致）；超了 → [`AUTO_BLOCK`]。
        - 显式边长：即便图小于块边长也照走分块路径（只切出 1 块）——**参数说了算**，
          这样「同一参数在大小图上行为一致」，测试也能用小块在同批图上演分块。
    """
    if tile == "auto":
        if width * height <= model_cap:
            return None
        return AUTO_BLOCK
    return int(tile)


def grid(width, height, block):
    """切块网格：返回 `[(x0, y0, x1, y1)]`（左闭右开，按行优先）。"""
    if block <= 0:
        raise ValueError("块边长必须 > 0")
    tiles = []
    for y0 in range(0, height, block):
        y1 = min(height, y0 + block)
        for x0 in range(0, width, block):
            x1 = min(width, x0 + block)
            tiles.append((x0, y0, x1, y1))
    return tiles


def padded_box(box, width, height, margin=MARGIN):
    """块 → 实际送模型的裁剪框（向外扩 margin，夹到图内）。"""
    x0, y0, x1, y1 = box
    return (max(0, x0 - margin), max(0, y0 - margin),
            min(width, x1 + margin), min(height, y1 + margin))


def stitch_offsets(box, pbox, factor):
    """块在内区里的落点与源偏移（都乘 `factor`）：

    返回 `(dst_x, dst_y, src_x, src_y, w, h)`：
        - `dst_*`：贴到画布的左上角（= 块左上 × factor）；
        - `src_*`：从放大后的块里取图的左上角（= (块左上 − 裁剪框左上) × factor）；
        - `w/h`：块尺寸 × factor。
    """
    x0, y0, x1, y1 = box
    px0, py0, _px1, _py1 = pbox
    return (x0 * factor, y0 * factor,
            (x0 - px0) * factor, (y0 - py0) * factor,
            (x1 - x0) * factor, (y1 - y0) * factor)


def new_canvas(width, height, factor):
    """4× 画布（RGB）。超 [`MAX_CANVAS_PIXELS`] 抛 `ValueError`（调用方转结构化失败）。"""
    cw, ch = width * factor, height * factor
    if cw * ch > MAX_CANVAS_PIXELS:
        raise ValueError(
            "分块画布 %dx%d（%d 像素）超过上限 %d：请先缩小源图或改用更小的放大倍数"
            % (cw, ch, cw * ch, MAX_CANVAS_PIXELS))
    return bytearray(cw * ch * 3), cw, ch


def blit(canvas, canvas_w, canvas_h, tile_w, tile_h, channels, raw, dst_x, dst_y,
         src_x, src_y, w, h):
    """把放大后的块内区按行拷到画布。越界/通道不符一律显式报错（不静默裁）。"""
    if channels != 3:
        raise ValueError("块通道数 %r（期望 RGB）" % (channels,))
    if src_x < 0 or src_y < 0 or src_x + w > tile_w or src_y + h > tile_h:
        raise ValueError("块内区越界：src=(%d,%d) %dx%d 块=%dx%d"
                         % (src_x, src_y, w, h, tile_w, tile_h))
    if dst_x < 0 or dst_y < 0 or dst_x + w > canvas_w or dst_y + h > canvas_h:
        raise ValueError("画布落点越界：dst=(%d,%d) %dx%d 画布=%dx%d"
                         % (dst_x, dst_y, w, h, canvas_w, canvas_h))
    row_bytes = w * 3
    for i in range(h):
        src = ((src_y + i) * tile_w + src_x) * 3
        dst = ((dst_y + i) * canvas_w + dst_x) * 3
        canvas[dst:dst + row_bytes] = raw[src:src + row_bytes]
    return canvas
