"""几何修复闭式解：`crop → letterbox(1:1 白底) → upscale`。

**为什么要闭式解**：旧引擎是「单步 smart_crop → 只复检本条规则」，
裁完把白边/尺寸弄坏也没有下一步动作（`fix-policy-and-a4.md` 的缺陷证据），
所以 A4 修好后 A2/A3 必坏。这里把三件事一次算出来：

    A          = fg_ratio * W * H            主体像素面积（只依赖 fg_ratio，标量）
    M_ideal    = sqrt(A / target)            想达到 target 占比所需的方形画布边长
    crop_box   = 居中的 min(W, M_ideal) × min(H, M_ideal) 窗口
    M          = max(crop 后两向)            letterbox 到 1:1 后的画布边长
    预测占比    = A / M²                     ≥ target 且 ≤ max 才可解
    scale      = short_min / M               （M < 短边门槛时才放大）

**已知精度限制**：网关 `/detect/u2net` 只回 `fg_ratio` 标量与 `mask_path`，
不回主体 bbox（掩码是 RGB PNG，插件没有解码器也不该逐像素读大图），
所以裁剪窗只能**居中**推定。主体贴边的图会被裁掉一部分：
`A` 变小于预测 → 实测占比低于 target → 下一轮按同一公式再收紧（几何收敛，≤3 轮）。
真需要「按掩码 bbox 精确裁」应在网关补 `/detect/bbox` 端点（需升 api_version），
见 README §能力缺口。
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


def solve(width, height, fg_ratio, *, target_ratio=0.75, ratio_min=0.55,
          ratio_max=0.85, short_side_min=1600):
    """算本轮几何计划。不可解时返回 `unfixable=<原因>`，绝不留半截动作。"""
    width, height = int(width), int(height)
    plan = Plan(
        source={"width": width, "height": height, "fg_ratio": fg_ratio},
        crop_box=None, target_ratio=1.0, scale=1.0,
        predicted_ratio=None, predicted_side=min(width, height),
        unfixable=None, reason=None,
    )
    if fg_ratio is None:
        plan["unfixable"] = "no_subject_measurement"
        plan["reason"] = "网关没有给出 fg_ratio，无法求解几何"
        return plan
    if width < MIN_SIDE or height < MIN_SIDE:
        plan["unfixable"] = "asset_too_small"
        plan["reason"] = "资产小于 %dpx，几何修复无意义" % MIN_SIDE
        return plan

    ratio = float(fg_ratio)
    area = ratio * width * height

    # 主体已经占满画面：裁剪只能继续放大占比，几何无解（换底/重绘本体被禁止）
    if ratio > float(ratio_max):
        plan["unfixable"] = "subject_ratio_above_max"
        plan["reason"] = "主体占比 %.3f > 上限 %.2f，裁剪无法降低占比" % (ratio, ratio_max)
        return plan

    # 占比已达标就不许乱裁（修一次就够，别把合格的图裁坏）；
    # 只有「补白成正方形会把占比压破下限」时才必须收窗。
    canvas_now = max(width, height)
    if ratio < float(ratio_min) or area / float(canvas_now * canvas_now) < float(ratio_min):
        ideal = math.sqrt(area / float(target_ratio))
        crop_w = int(min(width, max(MIN_SIDE, math.floor(ideal))))
        crop_h = int(min(height, max(MIN_SIDE, math.floor(ideal))))
    else:
        crop_w, crop_h = width, height

    needs_crop = crop_w < width or crop_h < height
    if needs_crop:
        x1 = max(0, (width - crop_w) // 2)
        y1 = max(0, (height - crop_h) // 2)
        plan["crop_box"] = [x1, y1, x1 + crop_w, y1 + crop_h]

    canvas = max(crop_w, crop_h)          # letterbox 到 1:1 后的边长
    predicted = area / float(canvas * canvas)
    plan["target_ratio"] = 1.0
    plan["predicted_ratio"] = round(predicted, 4)
    plan["predicted_side"] = canvas

    if predicted > float(ratio_max) + 1e-9:
        plan["unfixable"] = "subject_ratio_above_max"
        plan["reason"] = ("几何修复后占比 %.3f 仍高于上限 %.2f（画布已到 %dpx，"
                          "再加白边会改变构图）" % (predicted, ratio_max, canvas))
        plan["crop_box"] = None
        return plan

    scale = 1.0
    if canvas < int(short_side_min):
        # 向上取整到 1e-4：宁可放大 0.01px，也不许因为四舍五入差 1px 不达标
        step = 10 ** SCALE_DECIMALS
        scale = math.ceil(int(short_side_min) / float(canvas) * step) / step
    plan["scale"] = scale
    plan["predicted_side"] = int(round(canvas * scale))
    return plan


def needs_letterbox(crop_box, width, height):
    """裁剪窗不是正方形时才需要补白边（正方形直接进放大）。"""
    if not crop_box:
        return width != height
    cw = crop_box[2] - crop_box[0]
    ch = crop_box[3] - crop_box[1]
    return cw != ch


def is_noop(plan):
    """已经满足：不裁、不补、不放大。"""
    return (plan.get("crop_box") is None and plan.get("scale") == 1.0
            and plan.get("predicted_side", 0) == plan["source"]["width"]
            and plan["source"]["width"] == plan["source"]["height"])
