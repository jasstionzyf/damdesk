#!/usr/bin/env python3
"""imaging 插件 · 叠加动作组（N6c `task`）：`watermark` + `rename`。

范围（只做这两件事）
    1. `watermark`：文字水印叠加 —— 五锚点 + 透明度 + 字号 + 颜色 + 边距。
       像素合成**全部走宿主** `/op/watermark`（`core-media::watermark_text`：ab_glyph +
       内嵌 Noto Sans SC/OFL）。插件不自写字体光栅化、不写 JPEG/PNG 编解码：
       处理类包「零第三方依赖」这条铁律不能靠包自己写一套像素栈来破。
    2. `rename`：模板改名（`{name}{ext}{width}{height}{date}{index}{assetId}`），
       **只出名字、不改文件字节**（`artifacts=[]`）：产物是「资产名」，不是新文件。
       同批重复名按调用序确定性递增 `-2`/`-3`…（同一批同序 = 同一结果）。

硬边界（本卡口径，改之前先看卡）
    - 不碰格式/质量（N6b 编码组）、不碰几何/裁剪（N6a）、不写 manifest（N6d 装配）。
    - 产物只落宿主给的 `workdir`（协议 §4.3：插件不拥有图片，原图只读）。
    - 结果行**无 verdict**（处理类；N5a 契约：处理类回 verdict → 宿主整批拒收）。
    - 网关错**不吞**：403 能力缺失 → `-32001` 失败行，绝不假装「水印加上了」。

装配契约（N6d 用）
    >>> import overlay
    >>> overlay.ACTIONS                    # {"watermark": watermark_action, "rename": rename_action}
    >>> overlay.SCHEMA                     # 参数 schema 片段（group=overlay）
    >>> overlay.watermark_action(asset, params, gw, work_dir)
    handler(asset, params, gw, work_dir) -> row(dict)     # 结果行，见 geometry._ok_row

纯计划部分（`plan_watermark` / `plan_rename` / `render_template` / `sanitize_stem`）
**不碰网关、不碰磁盘**：同参数同输入 → 同计划（可复现、可单测、可直接对语料真值）。
"""
from __future__ import annotations

import datetime
import os
import re

# 结果行口径与几何组**共用一份实现**：N6d 会把两个 group 装进同一个 entry，
# 行形状在这里漂移 = 宿主按 `row_*` 违例整批拒收。复制粘贴 6 个小 helper 才是风险源。
from geometry import (                            # noqa: E402  (同目录 plugin/)
    _failed_row,
    _got,
    _ok_row,
    _out_path,
    _rpc_code,
    _sha256,
)

TAG = "overlay"
KINDS = ("process",)              # 处理类：无审核列

# ---------------------------------------------------------------- 水印参数口径
ANCHORS = ("top_left", "top_right", "center", "bottom_left", "bottom_right")

# 别名表：产品里人嘴上的 `br`/`右下`、旧 YAML 的 `bottom_right` 都能进来，但落库只有一个值。
ANCHOR_ALIASES = {
    "tl": "top_left", "top_left": "top_left", "topleft": "top_left", "top-left": "top_left",
    "左上": "top_left",
    "tr": "top_right", "top_right": "top_right", "topright": "top_right", "top-right": "top_right",
    "右上": "top_right",
    "c": "center", "center": "center", "centre": "center", "middle": "center", "居中": "center",
    "bl": "bottom_left", "bottom_left": "bottom_left", "bottomleft": "bottom_left",
    "bottom-left": "bottom_left", "左下": "bottom_left",
    "br": "bottom_right", "bottom_right": "bottom_right", "bottomright": "bottom_right",
    "bottom-right": "bottom_right", "右下": "bottom_right",
}

DEFAULT_TEXT = "DAMDesk"
DEFAULT_ANCHOR = "bottom_right"
DEFAULT_OPACITY = 0.5             # schema 口径 0..1；发宿主时换算成 1..100 整数百分比
DEFAULT_FONT_SIZE = 48            # px（与旧 C4 watermark 变换同缺省）
DEFAULT_MARGIN = 16               # px
DEFAULT_COLOR = "#ffffff"
FONT_SIZE_RANGE = (8, 512)        # 与宿主 /op/watermark 校验一致
OPACITY_MIN_RATIO = 0.01          # 0.01 以下按参数错拒绝（= 不出墨，产物与输入无法区分）

# ---------------------------------------------------------------- 改名参数口径
RENAME_DEFAULT_TEMPLATE = "{name}_{width}x{height}"
RENAME_MAX_STEM = 120             # 落库/落盘都别造超长名字（Windows 路径上限友好）
_PLACEHOLDER_RE = re.compile(r"\{([A-Za-z_][A-Za-z0-9_]*)\}")
# 允许的占位符：`{name}`/`{origName}`、`{w}`/`{width}` 是同义别名（旧 YAML 用前者，规格文案用后者）
PLACEHOLDERS = ("name", "origName", "ext", "width", "w", "height", "h",
                "date", "index", "assetId")


# ---------------------------------------------------------------- 水印：纯计划
def parse_anchor(raw):
    """锚点 → 五个规范值之一。空值取默认；认不出的值响亮报错（不静默回退）。"""
    if raw is None or (isinstance(raw, str) and not raw.strip()):
        return DEFAULT_ANCHOR
    key = str(raw).strip().lower().replace(" ", "")
    if key in ANCHOR_ALIASES:
        return ANCHOR_ALIASES[key]
    raise ValueError("锚点 %r 不认识（支持 %s，别名 br/bl/tr/tl/c）"
                     % (raw, "/".join(ANCHORS)))


def parse_opacity(raw):
    """透明度 → 1..100 整数百分比。接受 0..1 比例（产品/schema 口径）与 1..100 百分比（旧 YAML）。"""
    if raw is None:
        return int(round(DEFAULT_OPACITY * 100))
    if isinstance(raw, bool):                        # True/False 不是透明度
        raise ValueError("透明度不接受布尔值（给 0..1 比例或 1..100 百分比）")
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise ValueError("透明度必须是数字（0..1 比例或 1..100 百分比），收到 %r" % (raw,))
    if not (value == value) or value in (float("inf"), float("-inf")):
        raise ValueError("透明度必须是有限数字，收到 %r" % (raw,))
    pct = value * 100.0 if value <= 1.0 else value    # ≤1 一律按比例读：0.35→35，1→100
    whole = int(pct + 0.5)                           # 半上取整（不用银行家舍入，与宿主一致）
    if whole < max(1, int(round(OPACITY_MIN_RATIO * 100))) or whole > 100:
        raise ValueError("透明度 %r 越界（比例 0.01..1 或百分比 1..100；0 等于不出墨）" % (raw,))
    return whole


def parse_color(raw):
    """颜色 → `rrggbb`（小写、无 `#`）。只收六位十六进制：三位缩写会悄悄换色，不接受。"""
    if raw is None or (isinstance(raw, str) and not raw.strip()):
        return DEFAULT_COLOR.lstrip("#").lower()
    text = str(raw).strip().lower()
    if text.startswith("#"):
        text = text[1:]
    if len(text) != 6 or any(ch not in "0123456789abcdef" for ch in text):
        raise ValueError("颜色 %r 不合法（要求六位十六进制，如 #ffffff）" % (raw,))
    return text


def plan_watermark(params, width, height):
    """参数 + 画布尺寸 → 水印计划（纯函数，不碰网关/磁盘）。

    计划里带的是**发给宿主 `/op/watermark` 的原样字段**（`opacity` 已换算成 1..100），
    这样动作层不用二次换算 —— 参数口径只有一处（单测直接对这里打）。
    """
    params = params or {}
    width, height = int(width), int(height)
    if width <= 0 or height <= 0:
        raise ValueError("画布尺寸非法：%dx%d" % (width, height))
    text = params.get("watermarkText", DEFAULT_TEXT)
    text = DEFAULT_TEXT if text is None else str(text)
    if not text.strip():
        raise ValueError("水印文字不能为空（空文字 = 不出墨，别静默出个跟原图一样的产物）")
    if "\n" in text or "\r" in text:
        raise ValueError("水印文字只支持单行 v1（换行请拆成多次叠加）")
    position = parse_anchor(params.get("watermarkAnchor", DEFAULT_ANCHOR))
    opacity = parse_opacity(params.get("watermarkOpacity", DEFAULT_OPACITY))
    raw_font = params.get("watermarkFontSize", DEFAULT_FONT_SIZE)
    try:
        font_size = int(round(float(raw_font)))
    except (TypeError, ValueError):
        raise ValueError("字号必须是数字（px），收到 %r" % (raw_font,))
    if not FONT_SIZE_RANGE[0] <= font_size <= FONT_SIZE_RANGE[1]:
        raise ValueError("字号 %d 越界（%d..%d px）" % (font_size, FONT_SIZE_RANGE[0],
                                                      FONT_SIZE_RANGE[1]))
    raw_margin = params.get("watermarkMargin", DEFAULT_MARGIN)
    try:
        margin = int(round(float(raw_margin)))
    except (TypeError, ValueError):
        raise ValueError("边距必须是数字（px），收到 %r" % (raw_margin,))
    if margin < 0:
        raise ValueError("边距 %d 不能为负" % margin)
    # 宿主同款门槛：2×margin 必须小于短边，否则水印无处可落（提前报，不让用户看到网关 400）
    if margin * 2 >= min(width, height):
        raise ValueError("边距 %d 过大（图 %dx%d，要求 2×边距 < 短边）"
                         % (margin, width, height))
    return {
        "text": text,
        "position": position,
        "opacity": opacity,            # 1..100，直接发宿主
        "opacityRatio": round(opacity / 100.0, 4),
        "font_size": font_size,
        "margin": margin,
        "color": parse_color(params.get("watermarkColor", DEFAULT_COLOR)),
    }


# ---------------------------------------------------------------- 改名：纯计划
def sanitize_stem(stem):
    """文件名主干消毒：去路径分隔/控制字符/首尾点下划线空格、折叠空白与 `..`、限长。

    **只在主干上做**（扩展名由源文件决定）：改名不该顺手换格式 —— 那是编码组的事。
    消毒后必须还剩**有效字符**（字母/数字/CJK）：模板渲染成 `"/"`、`"..."`、`"   "` 这类
    纯分隔符一律报错，别静默产出一个语义为空的 `_`。
    """
    text = str(stem)
    cleaned = []
    for ch in text:
        if ch in "/\\:*?\"<>|" or ord(ch) < 32:
            cleaned.append("_")
        else:
            cleaned.append(ch)
    out = re.sub(r"\s+", " ", "".join(cleaned))
    while ".." in out:
        out = out.replace("..", ".")
    out = out.strip("._ ")
    if not out or not any(ch.isalnum() for ch in out):
        raise ValueError("改名结果为空或没有有效字符（模板渲染出了 %r）" % (stem,))
    if len(out) > RENAME_MAX_STEM:
        out = out[:RENAME_MAX_STEM].strip("._ ")
        if not out:
            raise ValueError("改名结果截断后为空（模板过长）")
    return out


def render_template(template, ctx):
    """模板渲染：`{占位符}` 逐个替换；出现未声明占位符/落单花括号 → 响亮报错。"""
    template = RENAME_DEFAULT_TEMPLATE if template is None or not str(template).strip() \
        else str(template)
    unknown = []

    def _sub(match):
        key = match.group(1)
        if key not in ctx:
            unknown.append(key)
            return match.group(0)
        return str(ctx[key])

    out = _PLACEHOLDER_RE.sub(_sub, template)
    if unknown:
        raise ValueError("模板含未知占位符 %s（支持：%s）"
                         % (",".join("{%s}" % k for k in sorted(set(unknown))),
                            " ".join("{%s}" % k for k in PLACEHOLDERS)))
    if "{" in out or "}" in out:
        raise ValueError("模板花括号不配对：%r（占位符要写成 {name} 这种形式）" % (template,))
    return out


class RenameRegistry:
    """批内改名登记：给 `{index}` 发号 + 给重复名加 `-2`/`-3` 尾号。

    生命周期 = 插件进程 = 一个 job（launcher 每 job 拉一个进程），所以天然按批隔离。
    **确定性口径**：同一批、同一调用序 → 同一批名字（逐字节相同）；换序则尾号可能不同
    （这是「批内递增」的定义，不是抖动）。要跨序稳定就显式用 `{assetId}` 占位符。
    """

    def __init__(self):
        self._seq = 0
        self._used = {}

    def reset(self):
        self._seq = 0
        self._used = {}

    def next_index(self):
        """批内序号（1 起；含跳过/失败资产 —— 序号跟调用序走，不跟成功数走）。"""
        self._seq += 1
        return self._seq

    def allocate(self, stem):
        """占用一个主干 → `(最终主干, 冲突序号)`；首个占用者序号 1（无尾号）。"""
        stem = str(stem)
        if stem not in self._used:
            self._used[stem] = 1
            return stem, 1
        n = self._used[stem]
        while True:
            n += 1
            candidate = "%s-%d" % (stem, n)
            if candidate not in self._used:
                self._used[stem] = n
                self._used[candidate] = 1
                return candidate, n


# 进程级登记表（N6d 的 entry 直接引用；单测各自 new 一个以免互相污染）
RENAME_REGISTRY = RenameRegistry()


def plan_rename(original_name, params, width, height, mtime=None,
                index=1, asset_id=None, registry=None):
    """模板 + 上下文 → 改名计划（纯函数；`registry` 只影响重复名尾号）。

    `{date}` 取**源文件 mtime 的日期**（不是 now()）：同一张图今天跑和明天跑必须同名，
    否则「同参数同结果」在改名这一步就不成立了。
    """
    params = params or {}
    template = params.get("renameTemplate", RENAME_DEFAULT_TEMPLATE)
    original = str(original_name or "")
    original_stem, ext = os.path.splitext(os.path.basename(original))
    ext = ext.lstrip(".").lower() or "jpg"
    when = datetime.datetime.fromtimestamp(float(mtime)) if mtime is not None \
        else datetime.datetime.fromtimestamp(0)
    ctx = {
        "name": original_stem or "asset",
        "origName": original_stem or "asset",
        "ext": ext,
        "width": int(width),
        "w": int(width),
        "height": int(height),
        "h": int(height),
        "date": when.strftime("%Y%m%d"),
        "index": int(index),
        "assetId": str(asset_id if asset_id is not None else ""),
    }
    stem = sanitize_stem(render_template(template, ctx))
    reg = registry if registry is not None else RENAME_REGISTRY
    final_stem, occurrence = reg.allocate(stem)
    return {
        "template": str(template),
        "stem": final_stem,
        "ext": ext,
        "name": "%s.%s" % (final_stem, ext),
        "originalName": original,
        "originalStem": original_stem,
        "changed": "%s.%s" % (final_stem, ext) != original,
        "collisionIndex": occurrence,
        "index": int(index),
        "date": ctx["date"],
    }


# ---------------------------------------------------------------- 动作：watermark
def watermark_action(asset, params, gw, work_dir):
    """`watermark`：宿主 `/io/read` 尺寸 → 计划 → 宿主 `/op/watermark` 出叠加图。

    结果行口径：`result=ok` + `fields.outPath/sha256`（产物落宿主 workdir），`artifacts` 留空
    —— 产物变「资产工作版本」由宿主（N7 `/process/*`）建，插件不自己编 `versionId`。
    """
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
        size = (int(_got(info, "width") or 0), int(_got(info, "height") or 0))
    except Exception as exc:                       # noqa: BLE001 —— 网关错成行，不炸进程
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "watermark"})
    try:
        plan = plan_watermark(params, size[0], size[1])
    except Exception as exc:                       # noqa: BLE001 —— 参数错 → -32003
        return _failed_row(asset, -32003, str(exc), {"action": "watermark"})
    fields = {
        "action": "watermark",
        "in": list(size),
        "text": plan["text"],
        "anchor": plan["position"],
        "opacity": plan["opacityRatio"],
        "opacityPct": plan["opacity"],
        "fontSize": plan["font_size"],
        "margin": plan["margin"],
        "color": "#" + plan["color"],
        "source": path,
    }
    out_path = _out_path(work_dir, asset, "watermark", path)
    try:
        reply = gw.watermark(path, plan["text"], out_path,
                             position=plan["position"], opacity=plan["opacity"],
                             font_size=plan["font_size"], margin=plan["margin"],
                             color=plan["color"])
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), fields)
    out_size = [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)]
    if out_size != list(size):
        # 叠加不是变形：尺寸变了说明宿主/端点语义被改坏（宁可失败也不交付一张偷缩的图）
        return _failed_row(asset, -32000,
                          "水印产物尺寸 %s ≠ 源图 %s（叠加不应改变尺寸）"
                          % (out_size, list(size)), fields)
    fields.update({
        "out": out_size,
        "outPath": out_path,
        "op": _got(reply, "op"),
        "bytes": os.path.getsize(out_path) if os.path.exists(out_path) else 0,
        "sha256": _sha256(out_path) if os.path.exists(out_path) else None,
    })
    return _ok_row(asset, fields, {
        "ops": ["/io/read", "/op/watermark"],
        "opDetail": _got(reply, "op"),
        "coverage": _got(reply, "coverage"),
        "sizeUnchanged": out_size == list(size),
        "anchor": plan["position"],
        "opacityPct": plan["opacity"],
    })


# ---------------------------------------------------------------- 动作：rename
def rename_action(asset, params, gw, work_dir, registry=None):
    """`rename`：只出名字（`artifact=[]`、不写盘）；字节不变由 `evidence.sourceSha256` 兜住。"""
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
        width = int(_got(info, "width") or 0)
        height = int(_got(info, "height") or 0)
        sha = _got(info, "sha256")
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "rename"})
    reg = registry if registry is not None else RENAME_REGISTRY
    try:
        index = reg.next_index()
        mtime = os.stat(path).st_mtime
        plan = plan_rename(os.path.basename(path), params, width, height,
                           mtime=mtime, index=index, asset_id=_got(asset, "id"), registry=reg)
    except Exception as exc:                       # noqa: BLE001 —— 模板/文件系统错 → -32003
        return _failed_row(asset, -32003 if isinstance(exc, ValueError) else -32000, str(exc),
                           {"action": "rename"})
    fields = {
        "action": "rename",
        "assetName": plan["name"],
        "stem": plan["stem"],
        "ext": plan["ext"],
        "originalName": plan["originalName"],
        "template": plan["template"],
        "changed": plan["changed"],
        "collisionIndex": plan["collisionIndex"],
        "batchIndex": plan["index"],
        "date": plan["date"],
        "source": path,
    }
    return _ok_row(asset, fields, {
        "ops": ["/io/read"],
        "sourceSha256": sha,
        "sourceWidth": width,
        "sourceHeight": height,
        # 改名不动字节：没有产物文件、没有工作版本，宿主只把 assetName 落到资产名上
        "bytesUnchanged": True,
        "artifactsProduced": 0,
        "note": "rename 只产生名字（插件不写盘、不出产物）",
    })


ACTIONS = {"watermark": watermark_action, "rename": rename_action}


# ---------------------------------------------------------------- 参数 schema
# 片段格式与 `crates/pack-host/src/params.rs` 一致（schemaVersion 1 / 8 种 type /
# when = 「另一个参数 ∈ 值列表」）。N6d 把它并进 `resources/params.schema.json`。
SCHEMA = {
    "schemaVersion": 1,
    "groups": [{"key": "overlay", "title": "叠加（水印 / 重命名）"}],
    "properties": {
        "watermarkText": {
            "type": "string",
            "title": "水印文字",
            "default": DEFAULT_TEXT,
            "group": "overlay",
        },
        "watermarkAnchor": {
            "type": "enum",
            "title": "水印位置",
            "options": list(ANCHORS),
            "default": DEFAULT_ANCHOR,
            "group": "overlay",
        },
        "watermarkOpacity": {
            "type": "number",
            "title": "透明度",
            "min": 0.01,
            "max": 1,
            "step": 0.05,
            "default": DEFAULT_OPACITY,
            "group": "overlay",
        },
        "watermarkFontSize": {
            "type": "integer",
            "title": "字号",
            "min": FONT_SIZE_RANGE[0],
            "max": FONT_SIZE_RANGE[1],
            "step": 1,
            "default": DEFAULT_FONT_SIZE,
            "unit": "px",
            "group": "overlay",
        },
        "watermarkMargin": {
            "type": "integer",
            "title": "边距",
            "min": 0,
            "max": 2000,
            "step": 1,
            "default": DEFAULT_MARGIN,
            "unit": "px",
            "group": "overlay",
        },
        "watermarkColor": {
            "type": "color",
            "title": "水印颜色",
            "default": DEFAULT_COLOR,
            "group": "overlay",
        },
        "renameTemplate": {
            "type": "string",
            "title": "改名模板",
            "default": RENAME_DEFAULT_TEMPLATE,
            "group": "overlay",
        },
    },
    "required": [],
}
