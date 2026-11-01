"""几何修复闭式解：`crop → letterbox(1:1 白底) → upscale`，**bbox/宿主窗驱动**。

**为什么要闭式解**：旧引擎是「单步 smart_crop → 只复检本条规则」，
裁完把白边/尺寸弄坏也没有下一步动作（`fix-policy-and-a4.md` 的缺陷证据），
所以 A4 修好后 A2/A3 必坏。这里把三件事一次算出来。

**为什么不再用 fg_ratio 反推裁剪窗**（K07 门禁暴露的缺陷，`KI-24`）：
只拿 `fg_ratio` 这一标量做 `M = sqrt(A / target)` 只能**居中推定**窗口，
主体偏心的图（a002 主体横跨画面 93% 宽）会被裁掉一部分，占比从 0.445 掉到 0.032，
第二轮再按缩小后的占比收紧、越收越小 —— 这就是「修 3 轮把主体裁没」的根因。
协议 1.1 的 `/detect/bbox` 给了三种锚点，本模块按「离主体最近」的档位取：

1. `window`（宿主带 `target_ratio` 回的窗）：**直接用宿主的窗**。它由同一张 u2net 掩码
   在 320² 网格上按「主主体中心对齐 + 占比二分到目标」算出，预测占比用窗内网格占比
   （网格级精确），插件这层只剩补白/放大。
2. `bbox`（主主体紧框）：**裁到主体紧框**，占比偏高才向四周扩窗（扩到 target 即可，
   不再凭空算一个以画面中心为锚的窗）；一档就能命中目标区间。
3. 都没有：居中推定（兼容 1.0 宿主；主体贴边时第一轮可能裁多，靠实测纠偏）。

**补白是可选动作**（K07-FIX-E 裁决）：补白成 1:1 会把画布面积放大到 `max(w,h)²`，
宽主体的图会因此把占比压破下限（a002 0.715 → 0.385）。所以这里先按 1:1 预测：

    predicted_square = A / max(crop_w, crop_h)²     补白后的占比
    predicted_aspect = A / (crop_w * crop_h)        不补白（保持窗口比例）的占比

`predicted_square` 达标 → 补白（平台主图惯例）；否则**不补白**、保留窗口比例，
只在 `evidence` 里标 `letterbox: false`。两者都低于下限 → 判不可解（`needs_human`），
绝不靠继续切主体去凑占比。

    A          = fg_ratio * W * H            主体像素面积（测量值，不参与窗口推导）
    crop_box   = 主主体紧框 / 宿主窗 / 居中推定
    M          = max(crop_w, crop_h)         补白后的画布边长
    scale      = short_min / 短边             （短边不足门槛时才放大）

`predicted_side` 一律是**放大后的短边**（不是长边）：不补白时短边 = `min(crop_w, crop_h)`，
补白时短边 = `M`。这样 `predicted_side ≥ short_side_min` 与 R1 的判据同口径。
"""
from __future__ import annotations

import math

MIN_SIDE = 32          # 裁剪窗最小边长（防退化成 1px）
SCALE_DECIMALS = 3


class Plan(dict):
    """几何计划（dict 便于直接落 evidence）。"""

    @property
    def fixable(self):
        return self.get("unfixable") is None


def solve(width, height, fg_ratio, *, window=None, bbox=None, target_ratio=0.75,
          ratio_min=0.55, ratio_max=0.85, short_side_min=1600, prefer_square=True,
          white_margin=False, white_margin_frac=0.06, ratio_headroom=0.02):
    """算本轮几何计划。不可解时返回 `unfixable=<原因>`，绝不留半截动作。

    输入（全部来自宿主能力网关，插件不解码像素）：

    - `fg_ratio`：`/detect/u2net` 或 `/detect/bbox` 的掩码占比（**只做面积测量**）；
    - `window`：`/detect/bbox` 的 `window`（`{box, ratio, target_ratio}`，源图像素）；
    - `bbox`：`/detect/bbox` 的 `dominant_bbox`（主主体紧框 `[x1,y1,x2,y2]`，源图像素）。

    锚点优先级：`window` > `bbox` > 居中推定（旧宿主兼容）。
    """
    width, height = int(width), int(height)
    plan = Plan(
        source={"width": width, "height": height, "fg_ratio": fg_ratio},
        anchor="center", crop_box=None, target_ratio=1.0, letterbox=False,
        scale=1.0, predicted_ratio=None, predicted_side=min(width, height),
        unfixable=None, reason=None,
    )
    if width < MIN_SIDE or height < MIN_SIDE:
        plan["unfixable"] = "asset_too_small"
        plan["reason"] = "资产小于 %dpx，几何修复无意义" % MIN_SIDE
        return plan

    bbox = _clamp_bbox(bbox, width, height)
    win_box, win_ratio = _window(window, width, height)

    # 主体像素面积：宿主窗内网格占比最精确 → 掩码占比 → 无测量则不可解
    if win_box and win_ratio is not None:
        area = win_ratio * (win_box[2] - win_box[0]) * (win_box[3] - win_box[1])
        plan["evidence_area"] = "window_grid"
    elif fg_ratio is not None:
        area = float(fg_ratio) * width * height
        plan["evidence_area"] = "mask_ratio"
    else:
        plan["unfixable"] = "no_subject_measurement"
        plan["reason"] = "网关没有给出 fg_ratio/宿主窗，无法求解几何"
        return plan

    ratio_now = area / float(width * height)
    crop_box = None

    if win_box and _is_subframe(win_box, width, height) and ratio_now <= ratio_max + 1e-9:
        # 档 1：宿主窗 = 主主体锚定 + 占比二分的结果，照用（插件只补白/放大）
        crop_box, plan["anchor"] = win_box, "window"
    elif ratio_now > ratio_max + 1e-9:
        # 主体占满画面：向外扩窗把背景纳进来（补白成方形后占比才可能落进区间）
        grown = _grow_to_target(area, target_ratio, width, height, bbox, plan)
        if grown is None:
            return plan
        crop_box, plan["anchor"] = grown, plan["anchor"]
    elif ratio_now < ratio_min - 1e-9:
        # 档 2/3：主体紧框 → 居中推定（旧的 1.0 宿主路径）
        if bbox:
            subject = _subject_box(area, target_ratio, ratio_max,
                                  bbox, width, height, plan)
            if subject is None:
                return plan
            crop_box, plan["anchor"] = subject, "bbox"
        else:
            crop_box = _center_box(area, target_ratio, width, height)
    else:
        # 占比已达标：补白成方形会把占比压破下限时才收窗，否则一律不动（别把合格图裁坏）
        canvas_now = max(width, height)
        if (prefer_square and width != height
                and area / float(canvas_now * canvas_now) < ratio_min):
            crop_box = _center_box(area, target_ratio, width, height)

    if white_margin and crop_box:
        # 白底规则待修：裁剪窗必须**自带一圈背景边**，换底后这圈才是纯白。
        # 为什么不能靠 letterbox 成 1:1：补白只补短边（f002：2388×2372 → 2388×2388，左右 0 边），
        # 而 1:1 会把画布撑到 max(边)² → 占比被稀释（实测 0.35，直接掉出 0.55~0.85）。
        # 正解 = 按「主体面积 / 目标占比」反推画布，再让主体居中（两侧自然留出背景），
        # 画布从**原图**裁出来（不裁掉主体、不越界），后面只做等比放大。
        box, meta = _margin_window(crop_box, width, height, area,
                                   target_ratio, ratio_min + ratio_headroom, ratio_max)
        if box is not None:
            crop_box = box
            plan["white_margin"] = meta
        else:
            # 连「占比下限 + 一圈边」都放不下 → 这是**真冲突**（f002 实测形态），
            # 不许为了白底把占比做破，也不许为了占比把已过的白底打破：交给人。
            plan["white_margin_conflict"] = "no_canvas_fits_ratio_and_margin"

    crop_w, crop_h = _box_dims(crop_box) if crop_box else (width, height)
    side = max(crop_w, crop_h)
    predicted_aspect = area / float(crop_w * crop_h)
    predicted_square = area / float(side * side)
    letterbox = (prefer_square and crop_w != crop_h
                 and predicted_square >= ratio_min - 1e-9)
    predicted = predicted_square if letterbox else predicted_aspect

    if predicted > ratio_max + 1e-9:
        plan["unfixable"] = "subject_ratio_above_max"
        plan["reason"] = ("几何修复后占比 %.3f 仍高于上限 %.2f（画布已到 %dpx，"
                          "再加背景会改变构图）" % (predicted, ratio_max, side))
        plan["crop_box"] = None
        return plan
    if predicted < ratio_min - 1e-9:
        plan["unfixable"] = "subject_ratio_below_min_keeping_subject"
        plan["reason"] = ("保住主体（%dx%d 紧框）时占比最高只能到 %.3f < 下限 %.2f；"
                          "再裁就会切到主体，几何修复到此为止"
                          % (crop_w, crop_h, predicted, ratio_min))
        plan["crop_box"] = None
        return plan

    short = side if letterbox else min(crop_w, crop_h)
    scale = 1.0
    if short < int(short_side_min):
        # 向上取整到 1e-3：宁可放大 0.001px，也不许因为四舍五入差 1px 不达标
        step = 10 ** SCALE_DECIMALS
        scale = math.ceil(int(short_side_min) / float(short) * step) / step

    plan["crop_box"] = crop_box
    plan["letterbox"] = letterbox
    plan["target_ratio"] = 1.0
    plan["predicted_ratio"] = round(predicted, 4)
    plan["predicted_ratio_aspect"] = round(predicted_aspect, 4)
    plan["scale"] = scale
    plan["predicted_side"] = max(int(short_side_min), int(round(short * scale)))
    if crop_w != crop_h and not letterbox:
        # 不补白：产物保持窗口比例（1:1 会压破占比下限，见文件头）
        plan["letterbox_skipped"] = round(predicted_square, 4)
    return plan


def _margin_window(bbox, width, height, area, prefer_ratio, ratio_min, ratio_max,
                   headroom=0.02):
    """在「占比预算」内给主体留出尽可能大的背景边，返回 `(box, meta)`。

    做法：`mask/bbox` 决定这个主体最多能撑到多高占比；把目标占比压到它下面
    `headroom`（默认 0.02）就是还能留出的边，画布按 `主体面积 / 目标占比` 反推、
    居中于主体，并夹进原图。放不下（夹边后有一边没背景 / 占比越界）就退到更小的边。

    为什么不能固定百分比放边：f002 实测 `mask/bbox ≈ 0.58`，占比下限 0.55 只剩
    ~2% 线性余量，固定 6% 放边会直接把占比做破（0.52 < 0.55）。
    """
    x1, y1, x2, y2 = [int(v) for v in bbox]
    bw, bh = x2 - x1, y2 - y1
    if bw <= 0 or bh <= 0 or area <= 0:
        return None, None
    bbox_area = float(bw * bh)
    ratio_at_bbox = float(area) / bbox_area
    if ratio_at_bbox < ratio_min:
        return None, None                    # 主体在紧框里都不够占比 → 先放大/换构图，放边免谈
    floor = ratio_min + 0.005
    top = prefer_ratio if ratio_min <= prefer_ratio <= ratio_max else ratio_max
    aspect = bw / float(bh)
    for frac in (1.0, 0.75, 0.5, 0.25):
        target = min(top, ratio_at_bbox - headroom * frac)
        if target < floor:
            target = floor
        if target >= ratio_at_bbox:
            continue                         # 没有放边余地
        need = float(area) / target
        cw = int(round((need * aspect) ** 0.5))
        ch = int(round(need / float(max(cw, 1))))
        cw, ch = max(cw, bw), max(ch, bh)
        if cw > width or ch > height:
            continue
        cx, cy = (x1 + x2) / 2.0, (y1 + y2) / 2.0
        nx1 = int(round(min(max(0.0, cx - cw / 2.0), width - cw)))
        ny1 = int(round(min(max(0.0, cy - ch / 2.0), height - ch)))
        box = [nx1, ny1, nx1 + cw, ny1 + ch]
        margins = [x1 - box[0], y1 - box[1], box[2] - x2, box[3] - y2]
        ratio = float(area) / float(cw * ch)
        if min(margins) > 0 and ratio_min <= ratio <= ratio_max:
            return box, {"ratio": round(ratio, 4), "canvas": [cw, ch],
                         "margins_px": [int(v) for v in margins]}
    return None, None


def _margin_box(box, area, width, height, ratio_floor, frac):
    """把裁剪窗向外放一圈背景（左右/上下各按 `frac` 比例），占比不得掉到 `ratio_floor` 以下。

    返回 `(box, 实际用的 frac)`；放不动（会压破占比下限或已到画面边界）时 `frac=0`，
    由调用方落 `white_margin_skipped` —— 不许为了一味补白把主体占比做破。
    """
    x1, y1, x2, y2 = [int(v) for v in box]
    for f in (frac, 0.04, 0.02, 0.01):
        if f <= 0:
            break
        mx, my = int(round((x2 - x1) * f)), int(round((y2 - y1) * f))
        if mx <= 0 or my <= 0:
            continue
        nx1, ny1 = max(0, x1 - mx), max(0, y1 - my)
        nx2, ny2 = min(width, x2 + mx), min(height, y2 + my)
        if (nx2 - nx1) <= (x2 - x1) and (ny2 - ny1) <= (y2 - y1):
            continue                     # 已在画面边界，放不出去
        if area / float((nx2 - nx1) * (ny2 - ny1)) + 1e-9 < ratio_floor:
            continue
        return [nx1, ny1, nx2, ny2], f
    return box, 0.0


def _box_dims(box):
    return int(box[2]) - int(box[0]), int(box[3]) - int(box[1])


def _window(window, width, height):
    """宿主窗：钳进画面（左闭右开）；退化/非法 → (None, None) 回退下一档锚点。"""
    if not window or not window.get("box"):
        return None, None
    box = _clamp_bbox(window.get("box"), width, height)
    if not box:
        return None, None
    raw = window.get("ratio")
    ratio = float(raw) if isinstance(raw, (int, float)) and float(raw) > 0 else None
    return box, ratio


def _clamp_bbox(bbox, width, height):
    """把宿主的 `dominant_bbox` 钳进画面（左闭右开）；退化/非法 → None。"""
    if not bbox:
        return None
    try:
        x1, y1, x2, y2 = (int(round(float(v))) for v in bbox[:4])
    except (TypeError, ValueError):
        return None
    x1, y1 = max(0, min(x1, width - 1)), max(0, min(y1, height - 1))
    x2, y2 = max(x1 + 1, min(x2, width)), max(y1 + 1, min(y2, height))
    if x2 - x1 < MIN_SIDE or y2 - y1 < MIN_SIDE:
        return None
    return [x1, y1, x2, y2]


def _is_subframe(box, width, height):
    return tuple(box) != (0, 0, width, height)


def _center_of(bbox, width, height):
    if bbox:
        return (bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0
    return width / 2.0, height / 2.0


def _subject_box(area, target_ratio, ratio_max, bbox, width, height, plan):
    """档 2：裁到主主体紧框；占比偏高（紧框太满）才等比扩窗到 target。

    返回 `None` 表示扩到画面边界也到不了 target（交 `_grow_to_target` 的不可解分支，
    这里直接给 `unfixable`）。
    """
    bw, bh = _box_dims(bbox)
    crop_w, crop_h = min(width, bw), min(height, bh)
    predicted = area / float(crop_w * crop_h)
    if predicted > ratio_max + 1e-9:
        side = int(math.ceil(math.sqrt(area / float(target_ratio)) - 1e-9))
        side = max(side, bw, bh)
        if side > max(width, height):
            plan["unfixable"] = "subject_ratio_above_max"
            plan["reason"] = ("主主体占比 %.3f 过高，扩到 %dpx 也超出画面，几何无法降低占比"
                              % (predicted, side))
            return None
        crop_w, crop_h = min(width, side), min(height, side)
    cx, cy = _center_of(bbox, width, height)
    return _anchor_box(crop_w, crop_h, width, height, cx=cx, cy=cy)


def _grow_to_target(area, target_ratio, width, height, bbox, plan):
    """占比超上限：以主体中心为锚扩窗，把占比压回 target。"""
    side = int(math.ceil(math.sqrt(area / float(target_ratio)) - 1e-9))
    if side > max(width, height):
        plan["unfixable"] = "subject_ratio_above_max"
        plan["reason"] = ("主体占比 %.3f 过高，扩窗需 %dpx > 画面 %dpx，几何无法降低占比"
                          % (area / float(width * height), side, max(width, height)))
        return None
    crop_w, crop_h = min(width, side), min(height, side)
    cx, cy = _center_of(bbox, width, height)
    plan["anchor"] = "bbox" if bbox else "center"
    return _anchor_box(crop_w, crop_h, width, height, cx=cx, cy=cy)


def _center_box(area, target_ratio, width, height):
    """档 3：居中推定（1.0 宿主，无主体框可用）。"""
    ideal = int(math.floor(math.sqrt(area / float(target_ratio))))
    crop_w = min(width, max(MIN_SIDE, ideal))
    crop_h = min(height, max(MIN_SIDE, ideal))
    if crop_w == width and crop_h == height:
        return None
    return _anchor_box(crop_w, crop_h, width, height,
                       cx=width / 2.0, cy=height / 2.0)


def _anchor_box(crop_w, crop_h, width, height, *, cx, cy):
    """窗口放在 (cx,cy) 居中处并整体平移回画面内（不缩窗）。"""
    crop_w, crop_h = min(int(crop_w), width), min(int(crop_h), height)
    x1 = int(round(cx - crop_w / 2.0))
    y1 = int(round(cy - crop_h / 2.0))
    x1 = max(0, min(x1, width - crop_w))
    y1 = max(0, min(y1, height - crop_h))
    return [x1, y1, x1 + crop_w, y1 + crop_h]


def needs_letterbox(crop_box, width, height):
    """裁剪窗不是正方形时才「可以」补白；是否真的补白看 `plan["letterbox"]`。"""
    if not crop_box:
        return width != height
    cw = crop_box[2] - crop_box[0]
    ch = crop_box[3] - crop_box[1]
    return cw != ch


def is_noop(plan):
    """已经满足：不裁、不补、不放大。"""
    if plan.get("crop_box") is not None or plan.get("scale") != 1.0:
        return False
    if plan.get("letterbox"):
        return False
    src = plan.get("source") or {}
    return (int(plan.get("predicted_side") or 0) == int(src.get("width") or 0)
            and src.get("width") == src.get("height"))
