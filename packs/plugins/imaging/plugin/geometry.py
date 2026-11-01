#!/usr/bin/env python3
"""imaging 插件 · 几何动作组（N6a `t_17e0a672`）：`resize` + `crop`。

范围（只做这两件事）
    1. `resize`：长边 / 短边 / 固定宽 / 固定高 / 等比系数；**默认只缩不放**（`resizeAllowUpscale`）。
    2. `crop`：目标比例（1:1 / 4:3 / 3:4 / 16:9 / 9:16 / 3:2 / 2:3 / 自定义）+ 锚点；
       **主体 bbox 必须完整落在结果内**——裁得下就裁，裁不下就补边（crop+pad），绝不切主体。

硬边界（本卡口径，改之前先看卡）
    - 不碰格式/质量（N6b 编码组）、不碰水印/重命名（N6c 叠加组）、不写 manifest（N6d 装配）。
    - 产物只落宿主给的 `workdir`（协议 §4.3：插件不拥有图片，原图只读）。
    - 结果行**无 verdict**（处理类；N5a 契约：处理类回 verdict → 宿主整批拒收）。
    - 像素解码/编码一律过宿主能力网关（`io.read` / `detect.bbox` / `op.crop` /
      `op.letterbox` / `op.upscale` 的 `scale<=1` 重采样档）——插件零第三方依赖、不解大图。

装配契约（N6d 用）
    >>> import geometry
    >>> geometry.ACTIONS                      # {"resize": resize_action, "crop": crop_action}
    >>> geometry.SCHEMA                       # 参数 schema 片段（group=geometry）
    >>> geometry.resize_action(asset, params, gw, work_dir)
    handler(asset, params, gw, work_dir) -> row(dict)     # 结果行，见 `_row_*`

纯几何部分（`plan_resize` / `plan_crop` / `parse_aspect`）**不碰网关、不碰磁盘**：
宿主给什么尺寸/主体框就出什么方案，同图同参数同结果（可复现、可单测、可直接对语料真值）。
"""
from __future__ import annotations

import hashlib
import math
import os

ACTIONS = ("resize", "crop")
KINDS = ("process",)          # 处理类：无审核列
TAG = "geometry"

# 缩放方式 → 目标值参数键（schema 与 plan 共用，避免两处漂移）
RESIZE_MODES = {
    "long_edge": "resizeLongEdge",
    "short_edge": "resizeShortEdge",
    "width": "resizeWidth",
    "height": "resizeHeight",
    "scale": "resizeScale",
}

# 语料/产品默认：长边 1600（电商主图/web 交付的通用上限档）
DEFAULT_LONG_EDGE = 1600
DEFAULT_PADDING = 0.05        # 主体四周留白（占源图短边比例）——见 plan_crop 留白不变量的由来
DEFAULT_BACKGROUND = "#ffffff"

# 输出文件名后缀：本卡不动编码，按源扩展名落盘（格式/质量归 N6b）
_FALLBACK_EXT = ".jpg"


# ---------------------------------------------------------------- 参数 schema
# 片段格式与 `crates/pack-host/src/params.rs` 一致（schemaVersion 1 / 8 种 type /
# when = 「另一个参数 ∈ 值列表」）。N6d 把它并进 `resources/params.schema.json`。
SCHEMA = {
    "schemaVersion": 1,
    "groups": [{"key": "geometry", "title": "几何（缩放 / 裁剪）"}],
    "properties": {
        "resizeMode": {
            "type": "enum",
            "title": "缩放方式",
            "options": ["long_edge", "short_edge", "width", "height", "scale"],
            "default": "long_edge",
            "group": "geometry",
        },
        "resizeLongEdge": {
            "type": "integer",
            "title": "长边上限",
            "min": 64,
            "max": 20000,
            "step": 100,
            "default": DEFAULT_LONG_EDGE,
            "unit": "px",
            "group": "geometry",
            "when": {"resizeMode": ["long_edge"]},
        },
        "resizeShortEdge": {
            "type": "integer",
            "title": "短边上限",
            "min": 64,
            "max": 20000,
            "step": 100,
            "default": DEFAULT_LONG_EDGE,
            "unit": "px",
            "group": "geometry",
            "when": {"resizeMode": ["short_edge"]},
        },
        "resizeWidth": {
            "type": "integer",
            "title": "目标宽",
            "min": 16,
            "max": 20000,
            "step": 10,
            "default": 1600,
            "unit": "px",
            "group": "geometry",
            "when": {"resizeMode": ["width"]},
        },
        "resizeHeight": {
            "type": "integer",
            "title": "目标高",
            "min": 16,
            "max": 20000,
            "step": 10,
            "default": 1600,
            "unit": "px",
            "group": "geometry",
            "when": {"resizeMode": ["height"]},
        },
        "resizeScale": {
            "type": "number",
            "title": "等比系数",
            "min": 0.05,
            "max": 8,
            "step": 0.05,
            "default": 0.5,
            "group": "geometry",
            "when": {"resizeMode": ["scale"]},
        },
        "resizeAllowUpscale": {
            "type": "boolean",
            "title": "允许放大（关闭＝只缩不放）",
            "default": False,
            "group": "geometry",
        },
        "cropAspect": {
            "type": "enum",
            "title": "目标比例",
            "options": ["1:1", "4:3", "3:4", "16:9", "9:16", "3:2", "2:3", "custom"],
            "default": "1:1",
            "group": "geometry",
        },
        "cropAspectCustom": {
            "type": "string",
            "title": "自定义比例（宽:高）",
            "default": "3:2",
            "group": "geometry",
            "when": {"cropAspect": ["custom"]},
        },
        "cropAnchor": {
            "type": "enum",
            "title": "锚点",
            "options": ["center", "top", "bottom", "left", "right"],
            "default": "center",
            "group": "geometry",
        },
        "cropPadding": {
            "type": "number",
            "title": "主体留白",
            "min": 0,
            "max": 0.2,
            "step": 0.01,
            "default": DEFAULT_PADDING,
            "group": "geometry",
        },
        "cropBackground": {
            "type": "color",
            "title": "补边颜色（裁不下目标比例时用）",
            "default": DEFAULT_BACKGROUND,
            "group": "geometry",
        },
    },
    "required": [],
}


# ---------------------------------------------------------------- 工具函数
def _round_half_up(x):
    """四舍五入（**远离零**，与 Rust `f64::round` 同口径）。

    Python 内置 `round` 是银行家舍入：`round(687.5) == 688` 但 `round(688.5) == 688`，
    与宿主 `/op/upscale` 算目标尺寸的口径不一致 → 计划尺寸和产物尺寸会差 1px。
    """
    return int(math.floor(float(x) + 0.5)) if x >= 0 else -int(math.floor(-float(x) + 0.5))


def parse_aspect(spec):
    """比例解析：`"1:1"` / `"4:3"` / `"3:2"` / 数字（宽/高）→ float；非法 → ValueError。"""
    if isinstance(spec, (int, float)):
        value = float(spec)
        if not math.isfinite(value) or value <= 0:
            raise ValueError("比例必须是正数（宽/高），收到 %r" % (spec,))
        return value
    raw = str(spec).strip().replace("：", ":").replace("/", ":")
    if ":" in raw:
        left, _, right = raw.partition(":")
        try:
            w = float(left)
            h = float(right)
        except ValueError:
            raise ValueError("比例格式应为 宽:高（如 4:3），收到 %r" % (spec,))
        if not (math.isfinite(w) and math.isfinite(h)) or w <= 0 or h <= 0:
            raise ValueError("比例的宽高都必须 > 0，收到 %r" % (spec,))
        return w / h
    return parse_aspect(float(raw))


def _as_int(value, name, low, high, default):
    try:
        n = int(round(float(value)))
    except (TypeError, ValueError):
        raise ValueError("%s 必须是数字，收到 %r" % (name, value))
    if not (low <= n <= high):
        raise ValueError("%s=%s 越界（允许 %s..%s）" % (name, n, low, high, ))
    return n


def _as_ratio(value, name, low, high, default):
    try:
        n = float(value)
    except (TypeError, ValueError):
        raise ValueError("%s 必须是数字，收到 %r" % (name, value))
    if not math.isfinite(n) or not (low <= n <= high):
        raise ValueError("%s=%s 越界（允许 %s..%s）" % (name, n, low, high))
    return n


# ---------------------------------------------------------------- resize 计划
def plan_resize(size, params=None):
    """算 resize 方案：返回 `{in, out, scale, mode, target, skip}`。

    `skip` 非空 = 这张图不需要动（结果行 `skipped` + `skipReason`）：
      - `no_upscale`：要放大但没开 `resizeAllowUpscale`（默认只缩不放）；
      - `already_satisfied`：算下来就是原尺寸。
    """
    params = dict(params or {})
    w, h = int(size[0]), int(size[1])
    if w <= 0 or h <= 0:
        raise ValueError("源图尺寸非法：%rx%r" % (size[0], size[1]))
    mode = str(params.get("resizeMode") or "long_edge").strip()
    if mode not in RESIZE_MODES:
        raise ValueError("resizeMode=%r 不在 %s 内" % (mode, sorted(RESIZE_MODES)))
    key = RESIZE_MODES[mode]
    if mode == "scale":
        raw = params.get(key, None)
        scale = _as_ratio(raw if raw is not None else 0.5, key, 0.05, 8.0, 0.5)
    elif mode == "long_edge":
        target = _as_int(params.get(key, DEFAULT_LONG_EDGE), key, 64, 20000, DEFAULT_LONG_EDGE)
        scale = target / float(max(w, h))
    elif mode == "short_edge":
        target = _as_int(params.get(key, DEFAULT_LONG_EDGE), key, 64, 20000, DEFAULT_LONG_EDGE)
        scale = target / float(min(w, h))
    elif mode == "width":
        target = _as_int(params.get(key, 1600), key, 16, 20000, 1600)
        scale = target / float(w)
    else:  # height
        target = _as_int(params.get(key, 1600), key, 16, 20000, 1600)
        scale = target / float(h)

    allow_upscale = bool(params.get("resizeAllowUpscale", False))
    out_w = max(1, _round_half_up(w * scale))
    out_h = max(1, _round_half_up(h * scale))
    plan = {
        "mode": mode,
        "in": [w, h],
        "out": [out_w, out_h],
        "scale": round(scale, 6),
        "allowUpscale": allow_upscale,
        "skip": None,
    }
    if scale > 1.0 + 1e-9 and not allow_upscale:
        plan["skip"] = "no_upscale"
        plan["out"] = [w, h]
        plan["scale"] = 1.0
    elif out_w == w and out_h == h:
        plan["skip"] = "already_satisfied"
        plan["scale"] = 1.0
    return plan


# ---------------------------------------------------------------- crop 计划
def _max_window(w, h, aspect):
    """图内能放下的、比例最接近 aspect 的最大窗口（宽, 高）。"""
    if w / float(h) > aspect:            # 图更宽 → 高度受限
        bh = h
        bw = min(w, max(1, _round_half_up(aspect * h)))
    else:                                 # 图更高/相等 → 宽度受限
        bw = w
        bh = min(h, max(1, _round_half_up(w / aspect)))
    return bw, bh


def _letterbox_canvas(w, h, aspect):
    """宿主 `/op/letterbox` 同一算法：内接 aspect 的最小画布（只补边、不缩放）。"""
    wf, hf = float(w), float(h)
    if wf / hf > aspect:                  # 比目标更宽 → 竖直补边
        return w, max(h, _round_half_up(wf / aspect))
    return max(w, _round_half_up(hf * aspect)), h


def _place(anchor, lo, hi, want_lo, want_hi, win):
    """在一维可行区间 [lo, hi] 内按锚点摆窗口起点（返回 int，恒落在区间内）。"""
    if lo > hi:                            # 退化（理论上不可达）→ 取 lo，交给上层校验兜住
        return int(lo)
    if anchor in ("left", "top", "tl", "bl"):
        return int(min(hi, max(lo, want_lo)))       # 主体贴该侧
    if anchor in ("right", "bottom", "tr", "br"):
        return int(min(hi, max(lo, want_hi - win)))
    center = (want_lo + want_hi) / 2.0
    return int(min(hi, max(lo, _round_half_up(center - win / 2.0))))


def _anchor_axes(anchor):
    """锚点 → (水平锚, 垂直锚)。产品枚举是 5 个，这里兼容 tl/tr/bl/br 全拼。"""
    table = {
        "center": ("center", "center"),
        "top": ("center", "top"),
        "bottom": ("center", "bottom"),
        "left": ("left", "center"),
        "right": ("right", "center"),
        "tl": ("left", "top"),
        "tr": ("right", "top"),
        "bl": ("left", "bottom"),
        "br": ("right", "bottom"),
    }
    return table.get(str(anchor or "center").strip().lower(), ("center", "center"))


def plan_crop(size, params=None, subject=None):
    """算 crop 方案（主体不可切）。

    `subject` = 主体外接框 `[x1,y1,x2,y2]`（源图像素，左闭右开，与 `/op/crop` 同口径）；
    None = 没有主体信息（整幅当主体 → 等价普通比例裁切，`subjectDetected=False`）。

    返回：
        mode        "crop"（裁得下）| "crop+pad"（裁不下 → 紧裁主体后补边到目标比例）
        box         交给 `/op/crop` 的像素框
        pad         `crop+pad` 时交给 `/op/letterbox` 的 `{target_ratio, background}`
        out         预测产物尺寸
        aspect/anchor/padding/...
        feasible    只裁不补能否同时满足「比例正确」与「主体完整」
        subjectCovered  主体是否完整落在 `out` 内（本实现恒 True，裁不下就补边）
    """
    params = dict(params or {})
    w, h = int(size[0]), int(size[1])
    if w <= 0 or h <= 0:
        raise ValueError("源图尺寸非法：%rx%r" % (size[0], size[1]))
    aspect_spec = params.get("cropAspect", "1:1")
    if str(aspect_spec).strip() == "custom":
        aspect = parse_aspect(params.get("cropAspectCustom", "3:2"))
    else:
        aspect = parse_aspect(aspect_spec or "1:1")
    anchor = str(params.get("cropAnchor") or "center").strip().lower()
    padding = _as_ratio(params.get("cropPadding", DEFAULT_PADDING), "cropPadding", 0.0, 0.2,
                        DEFAULT_PADDING)
    background = str(params.get("cropBackground") or DEFAULT_BACKGROUND)

    has_subject = subject is not None and len(subject) == 4
    if has_subject:
        assert subject is not None                      # 仅为类型收窄（has_subject 已保证）
        x1, y1, x2, y2 = (int(subject[0]), int(subject[1]), int(subject[2]), int(subject[3]))
        x1, y1 = max(0, min(x1, w)), max(0, min(y1, h))
        x2, y2 = max(x1 + 1, min(x2, w)), max(y1 + 1, min(y2, h))
    else:
        x1, y1, x2, y2 = 0, 0, w, h

    pad_px = _round_half_up(padding * min(w, h))
    bw_max, bh_max = _max_window(w, h, aspect)
    ax, ay = _anchor_axes(anchor)
    base = {
        "aspect": round(aspect, 6),
        "aspectSpec": str(aspect_spec),
        "anchor": anchor,
        "padding": padding,
        "in": [w, h],
        "subject": [x1, y1, x2, y2] if has_subject else None,
        "subjectDetected": bool(has_subject),
        "maxWindow": [bw_max, bh_max],
    }

    if not has_subject:
        # 无主体信息 → 普通比例裁切（最大比例正确的窗口，按锚点摆放），不涉及「保主体」判定
        win_w, win_h = bw_max, bh_max
        bx = 0 if ax == "left" else (w - win_w if ax == "right" else (w - win_w) // 2)
        by = 0 if ay == "top" else (h - win_h if ay == "bottom" else (h - win_h) // 2)
        plan = dict(base)
        plan.update({
            "mode": "crop",
            "feasible": True,
            "box": [bx, by, bx + win_w, by + win_h],
            "pad": None,
            "out": [win_w, win_h],
            "subjectCovered": True,             # 无主体约束 → 视为成立（fields 标 subjectDetected=False）
            "subjectMargin": None,
        })
        return plan

    # 必须完整落在产物内的区域 = 主体 + 四周留白（`cropPadding`，默认 5% 短边）。
    # **留白是硬要求**，原因是实测出来的：u2net 主体框比「非白像素真值」更紧
    # （语料 sc_06 顶边差 51px；sc_03/04/05/07/08 差几像素），没有留白时贴着窗口边裁
    # 就会把真主体切掉。5% 留白覆盖了这类检测松弛（语料 10/10 主体完整，见
    # `tests/corpus_geometry_e2e.py`）；留白放不下就不许「只裁不补」，改走 crop+pad。
    req_x1, req_y1 = max(0, x1 - pad_px), max(0, y1 - pad_px)
    req_x2, req_y2 = min(w, x2 + pad_px), min(h, y2 + pad_px)
    req_w, req_h = req_x2 - req_x1, req_y2 - req_y1
    feasible = bw_max >= req_w and bh_max >= req_h

    if feasible:
        win_w, win_h = bw_max, bh_max
        x_lo, x_hi = max(0, req_x2 - win_w), min(req_x1, w - win_w)
        y_lo, y_hi = max(0, req_y2 - win_h), min(req_y1, h - win_h)
        bx = _place(ax, x_lo, x_hi, req_x1, req_x2, win_w)
        by = _place(ay, y_lo, y_hi, req_y1, req_y2, win_h)
        box = [bx, by, bx + win_w, by + win_h]
        covered = box[0] <= x1 and box[1] <= y1 and box[2] >= x2 and box[3] >= y2
        plan = dict(base)
        plan.update({
            "mode": "crop",
            "feasible": True,
            "box": box,
            "pad": None,
            "out": [win_w, win_h],
            "subjectCovered": bool(covered),
            "subjectMargin": [x1 - box[0], y1 - box[1], box[2] - x2, box[3] - y2],
            "requiredBox": [req_x1, req_y1, req_x2, req_y2],
        })
        if not covered:     # 不变量被破坏 → 响亮失败，绝不悄悄切主体
            raise ValueError(
                "内部不变量破坏：比例 %.4f 的窗口 %s 未完整包含主体 %s（图 %dx%d）"
                % (aspect, box, [x1, y1, x2, y2], w, h))
        return plan

    # 裁不下：紧裁「主体+留白」（不超出原图）→ 补边到目标比例
    tw, th = max(1, req_w), max(1, req_h)
    canvas_w, canvas_h = _letterbox_canvas(tw, th, aspect)
    plan = dict(base)
    plan.update({
        "mode": "crop+pad",
        "feasible": False,
        "box": [req_x1, req_y1, req_x2, req_y2],
        "requiredBox": [req_x1, req_y1, req_x2, req_y2],
        "pad": {"target_ratio": round(aspect, 6), "background": background},
        "out": [canvas_w, canvas_h],
        "subjectCovered": True,
        "subjectMargin": [x1 - req_x1, y1 - req_y1, req_x2 - x2, req_y2 - y2],
        # 补边模式下主体在画布内的偏移（letterbox 居中；宿主同一算法）
        "padOffset": [(canvas_w - tw) // 2, (canvas_h - th) // 2],
    })
    return plan


# ---------------------------------------------------------------- 结果行
def _asset_id(asset):
    aid = asset.get("id")
    if aid not in (None, ""):
        return str(aid)
    path = asset.get("path") or ""
    return os.path.splitext(os.path.basename(str(path)))[0] or "asset"


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _ok_row(asset, fields, evidence):
    """处理行：只有执行结果 + 产物信息，**没有** verdict（N5a 契约）。"""
    return {
        "assetId": _asset_id(asset),
        "result": "ok",
        "fields": fields,
        "evidence": evidence,
    }


def _skipped_row(asset, reason, fields):
    return {"assetId": _asset_id(asset), "result": "skipped", "skipReason": str(reason),
            "fields": fields}


def _failed_row(asset, code, message, fields=None):
    return {"assetId": _asset_id(asset), "result": "failed",
            "error": {"code": str(code), "message": str(message)},
            "fields": fields or {}}


def _out_path(work_dir, asset, action, source_path):
    """产物路径：宿主 workdir 下的确定性文件名（同资产同动作 → 同路径，重跑可复现）。"""
    stem = os.path.splitext(os.path.basename(str(source_path)))[0]
    safe = "".join(ch if (ch.isalnum() or ch in "._-") else "_" for ch in stem) or "asset"
    ext = os.path.splitext(str(source_path))[1].lower() or _FALLBACK_EXT
    return os.path.join(str(work_dir), "%s.%s%s" % (safe, action, ext))


def _got(obj, key, default=None):
    """网关应答取值：dict 直接用，兼容对象属性（测试桩/未来 SDK 类型）。"""
    if isinstance(obj, dict):
        return obj.get(key, default)
    return getattr(obj, key, default)


def _rpc_code(exc):
    """网关异常 → 协议错误码（§5 闭集）：403→-32001 能力缺失、404→-32002 不可读、其余 -32000。"""
    code = getattr(exc, "code", None)
    if isinstance(code, int):
        return code
    status = getattr(exc, "status", None)
    return {403: -32001, 404: -32002, 400: -32003}.get(status, -32000)


# ---------------------------------------------------------------- 动作：resize
def resize_action(asset, params, gw, work_dir):
    """`resize`：按 plan 走宿主 `/op/upscale`（`scale<=1` = 单趟重采样，不碰超分模型）。"""
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
        size = (int(_got(info, "width") or 0), int(_got(info, "height") or 0))
    except Exception as exc:                       # noqa: BLE001 —— 网关错（不可读/能力缺失）成行
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "resize"})
    try:
        plan = plan_resize(size, params)
    except Exception as exc:                       # noqa: BLE001 —— 参数错 → -32003
        return _failed_row(asset, -32003, str(exc), {"action": "resize"})
    fields = {
        "action": "resize",
        "mode": plan["mode"],
        "in": plan["in"],
        "plannedOut": plan["out"],
        "scale": plan["scale"],
        "allowUpscale": plan["allowUpscale"],
        "source": path,
    }
    if plan["skip"]:
        fields["out"] = plan["in"]
        return _skipped_row(asset, plan["skip"], fields)

    out_path = _out_path(work_dir, asset, "resize", path)
    try:
        reply = gw.upscale(path, plan["scale"], out_path)
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), fields)
    out_size = [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)]
    fields.update({
        "out": out_size,
        "outPath": out_path,
        "op": _got(reply, "op"),
        "bytes": os.path.getsize(out_path) if os.path.exists(out_path) else 0,
        "sha256": _sha256(out_path) if os.path.exists(out_path) else None,
    })
    return _ok_row(asset, fields, {
        "ops": ["/io/read", "/op/upscale"],
        "opDetail": _got(reply, "op"),
        "sizeMatchesPlan": out_size == plan["out"],
        "noUpscale": not (out_size[0] > plan["in"][0] or out_size[1] > plan["in"][1])
        if not plan["allowUpscale"] else None,
    })


# ---------------------------------------------------------------- 动作：crop
def crop_action(asset, params, gw, work_dir):
    """`crop`：主体框从宿主 `/detect/bbox` 来；裁不下目标比例时紧裁 + 补边（不切主体）。"""
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
        size = (int(_got(info, "width") or 0), int(_got(info, "height") or 0))
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "crop"})
    try:
        det = gw.detect_bbox(path)
    except Exception as exc:                       # noqa: BLE001
        # 不吞 403（协议 §6 铁律）：没有主体检测能力就不假装「没切主体」
        return _failed_row(asset, _rpc_code(exc),
                           "主体检测不可用，无法保证不切主体：%s" % (exc,),
                           {"action": "crop", "stage": "detect"})
    subject = _got(det, "bbox") or _got(det, "dominant_bbox")
    try:
        plan = plan_crop(size, params, subject)
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, -32003, str(exc), {"action": "crop"})

    fields = {
        "action": "crop",
        "mode": plan["mode"],
        "aspect": plan["aspect"],
        "aspectSpec": plan["aspectSpec"],
        "anchor": plan["anchor"],
        "in": plan["in"],
        "plannedOut": plan["out"],
        "box": plan["box"],
        "requiredBox": plan.get("requiredBox"),
        "subject": plan["subject"],
        "subjectDetected": plan["subjectDetected"],
        "subjectCovered": plan["subjectCovered"],
        "feasible": plan["feasible"],
        "source": path,
    }
    ops = ["/io/read", "/detect/bbox", "/op/crop"]
    cur_path = _out_path(work_dir, asset, "crop", path)
    try:
        reply = gw.crop(path, plan["box"], cur_path)
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), fields)
    out_size = [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)]
    if plan["pad"]:
        pad_path = _out_path(work_dir, asset, "crop-pad", path)
        try:
            pad_reply = gw.letterbox(cur_path, plan["pad"]["target_ratio"], pad_path,
                                     background=plan["pad"]["background"])
        except Exception as exc:                   # noqa: BLE001
            return _failed_row(asset, _rpc_code(exc), str(exc), fields)
        ops.append("/op/letterbox")
        try:
            os.remove(cur_path)                    # 中间产物用完即删（产物只有一张）
        except OSError:
            pass
        cur_path = pad_path
        out_size = [int(_got(pad_reply, "width") or 0), int(_got(pad_reply, "height") or 0)]
        fields["pad"] = dict(plan["pad"])
        fields["padOffset"] = plan["padOffset"]
    fields.update({
        "out": out_size,
        "outPath": cur_path,
        "bytes": os.path.getsize(cur_path) if os.path.exists(cur_path) else 0,
        "sha256": _sha256(cur_path) if os.path.exists(cur_path) else None,
    })
    aspect_out = (out_size[0] / float(out_size[1])) if out_size[1] else 0.0
    return _ok_row(asset, fields, {
        "ops": ops,
        "sizeMatchesPlan": out_size == plan["out"],
        "aspectOut": round(aspect_out, 4),
        "aspectError": round(abs(aspect_out - plan["aspect"]), 4),
        "subjectCovered": plan["subjectCovered"],
    })


ACTIONS = {"resize": resize_action, "crop": crop_action}
