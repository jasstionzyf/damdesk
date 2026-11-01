#!/usr/bin/env python3
"""imaging 插件 · 编码动作组（N6b `t_83afd618`）：`convert` + `compress`。

范围（只做这两件事）
    1. `convert`：**按目标格式重编码**——jpg/png/webp/avif 互转；透明源转 png/avif/webp
       必须保留 alpha（转 jpg 只能按背景色合成，如实报告 `alpha=flattened`）；
       EXIF 方向默认摆正（`convertAutoOrient`）；色彩空间由宿主 vips 解到 sRGB。
    2. `compress`：**质量档**（单趟指定 quality）或**目标体积**（二分质量 + 必要时降采样，
       都不低于 `compressMinQuality`）——质量底线不可破：达不到目标体积就响亮失败，
       不靠"糊到 0.1 分"凑数。

硬边界（本卡口径，改之前先看卡）
    - 不碰几何 resize/crop（N6a）、不碰水印/重命名（N6c）、不写 manifest（N6d 装配）。
    - 所有像素与编码都过宿主 `/op/encode`（+ 降采样用 `/op/upscale` 的 `scale<=1` 档）；
      插件零第三方依赖、自己不解码不解码表。
    - 产物只落宿主给的 `workdir`；结果行**无 verdict**（处理类，N5a 契约）。
    - 中间产物（降采样档）用完即删：一个资产一次动作只留一张产物。

装配契约（N6d 用）
    >>> import codec
    >>> codec.ACTIONS                    # {"convert": convert_action, "compress": compress_action}
    >>> codec.SCHEMA                     # 参数 schema 片段（group=codec）
    >>> codec.convert_action(asset, params, gw, work_dir)
    handler(asset, params, gw, work_dir) -> row(dict)

纯策略部分（`plan_convert` / `plan_compress` / `bisect_quality` / `downscale_ladder`）
**不碰网关、不碰磁盘**：给同样的源元数据和参数，出同样的计划/同样的编码序列，
可单测、可复现（宿主只负责按计划编码）。
"""
from __future__ import annotations

import hashlib
import os

ACTION_NAMES = ("convert", "compress")
KINDS = ("process",)              # 处理类：无审核列
TAG = "codec"

# 可编码容器（与宿主 `/op/encode` 支持的格式闭集逐字一致）
FORMATS = ("jpg", "png", "webp", "avif")
# 能存 alpha 的容器（jpg 不能 → 必须按背景色合成）
ALPHA_FORMATS = ("png", "webp", "avif")
# 无损容器（quality 不生效——不假装生效，evidence 里写清）
LOSSLESS_FORMATS = ("png", "webp")

DEFAULT_CONVERT_FORMAT = "jpg"
DEFAULT_CONVERT_QUALITY = 85
DEFAULT_BACKGROUND = "#ffffff"

# 目标体积模式默认值：200KB / 质量底线 60 / 质量目标 80 / 允许降采样 / 长边不低于 640
DEFAULT_TARGET_BYTES = 200 * 1024
DEFAULT_MIN_QUALITY = 60
DEFAULT_PREFERRED_QUALITY = 80
DEFAULT_MAX_QUALITY = 95
DEFAULT_DOWNSCALE_STEP = 0.85     # 每级缩到 85%（等比）
DEFAULT_MIN_LONG_EDGE = 640
MAX_DOWNSCALE_STEPS = 8           # 最多缩 8 级（0.85^8 ≈ 0.27）；再缩就是"糊图凑体积"

_FALLBACK_EXT = ".jpg"


# ---------------------------------------------------------------- 参数 schema
# 片段格式与 `crates/pack-host/src/params.rs` 一致（schemaVersion 1 / when = 「另一参数 ∈ 值列表」）。
SCHEMA = {
    "schemaVersion": 1,
    "groups": [{"key": "codec", "title": "编码（转格式 / 压缩）"}],
    "properties": {
        "convertFormat": {
            "type": "enum",
            "title": "目标格式",
            "options": list(FORMATS),
            "default": DEFAULT_CONVERT_FORMAT,
            "group": "codec",
        },
        "convertQuality": {
            "type": "integer",
            "title": "质量（仅 jpg / avif 生效）",
            "min": 1,
            "max": 100,
            "step": 1,
            "default": DEFAULT_CONVERT_QUALITY,
            "group": "codec",
        },
        "convertKeepAlpha": {
            "type": "boolean",
            "title": "保留透明通道（目标格式不支持时按背景色合成）",
            "default": True,
            "group": "codec",
        },
        "convertBackground": {
            "type": "color",
            "title": "合成背景色（目标格式无 alpha / 关掉保留时用）",
            "default": DEFAULT_BACKGROUND,
            "group": "codec",
        },
        "convertAutoOrient": {
            "type": "boolean",
            "title": "按 EXIF 方向摆正",
            "default": True,
            "group": "codec",
        },
        "compressMode": {
            "type": "enum",
            "title": "压缩方式",
            "options": ["target_bytes", "quality"],
            "default": "target_bytes",
            "group": "codec",
        },
        "compressTargetBytes": {
            "type": "integer",
            "title": "目标体积上限",
            "min": 10 * 1024,
            "max": 20 * 1024 * 1024,
            "step": 1024,
            "default": DEFAULT_TARGET_BYTES,
            "unit": "B",
            "group": "codec",
            "when": {"compressMode": ["target_bytes"]},
        },
        "compressMinQuality": {
            "type": "integer",
            "title": "质量底线（不低于它；达不到体积就报失败）",
            "min": 20,
            "max": 95,
            "step": 1,
            "default": DEFAULT_MIN_QUALITY,
            "group": "codec",
            "when": {"compressMode": ["target_bytes"]},
        },
        "compressPreferredQuality": {
            "type": "integer",
            "title": "质量目标（达不到就让图小一点，而不是把质量压到底线）",
            "min": 40,
            "max": 95,
            "step": 1,
            "default": DEFAULT_PREFERRED_QUALITY,
            "group": "codec",
            "when": {"compressMode": ["target_bytes"]},
        },
        "compressAllowDownscale": {
            "type": "boolean",
            "title": "质量到底线仍超标时允许降采样",
            "default": True,
            "group": "codec",
            "when": {"compressMode": ["target_bytes"]},
        },
        "compressMinLongEdge": {
            "type": "integer",
            "title": "降采样长边下限",
            "min": 128,
            "max": 20000,
            "step": 64,
            "default": DEFAULT_MIN_LONG_EDGE,
            "unit": "px",
            "group": "codec",
            "when": {"compressMode": ["target_bytes"]},
        },
        "compressQuality": {
            "type": "integer",
            "title": "质量档",
            "min": 1,
            "max": 100,
            "step": 1,
            "default": 82,
            "group": "codec",
            "when": {"compressMode": ["quality"]},
        },
        "compressFormat": {
            "type": "enum",
            "title": "输出格式",
            "options": ["preserve", "jpg", "webp", "png", "avif"],
            "default": "preserve",
            "group": "codec",
        },
    },
    "required": [],
}


# ---------------------------------------------------------------- 工具函数
def _got(obj, key, default=None):
    """网关应答取值：dict 直接用，兼容对象属性（测试桩/未来 SDK 类型）。"""
    if isinstance(obj, dict):
        return obj.get(key, default)
    return getattr(obj, key, default)


def _rpc_code(exc):
    """网关异常 → 协议错误码（§5 闭集）：403→-32001、404→-32002、400→-32003、其余 -32000。"""
    code = getattr(exc, "code", None)
    if isinstance(code, int):
        return code
    status = getattr(exc, "status", None)
    if not isinstance(status, int):
        return -32000
    return {403: -32001, 404: -32002, 400: -32003}.get(status, -32000)


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
    return {"assetId": _asset_id(asset), "result": "ok", "fields": fields, "evidence": evidence}


def _skipped_row(asset, reason, fields):
    return {"assetId": _asset_id(asset), "result": "skipped", "skipReason": str(reason),
            "fields": fields}


def _failed_row(asset, code, message, fields=None):
    return {"assetId": _asset_id(asset), "result": "failed",
            "error": {"code": str(code), "message": str(message)},
            "fields": fields or {}}


def _stem_of(path):
    stem = os.path.splitext(os.path.basename(str(path)))[0]
    return "".join(ch if (ch.isalnum() or ch in "._-") else "_" for ch in stem) or "asset"


def _out_path(work_dir, asset, action, source_path, ext):
    """产物路径：workdir 下的确定性名字（同资产同动作同格式 → 同路径，重跑可复现）。"""
    return os.path.join(str(work_dir), "%s.%s.%s" % (_stem_of(source_path), action, ext.lstrip(".")))


def _tmp_path(work_dir, action, source_path, suffix):
    """中间产物路径（隐藏文件；用完即删，不留在产物目录里）。"""
    return os.path.join(str(work_dir), ".%s.%s%s" % (_stem_of(source_path), action, suffix))


def _as_int(value, name, low, high, default):
    try:
        n = int(round(float(value)))
    except (TypeError, ValueError):
        raise ValueError("%s 必须是数字，收到 %r" % (name, value))
    if not (low <= n <= high):
        raise ValueError("%s=%s 越界（允许 %s..%s）" % (name, n, low, high))
    return n


def _as_bool(value, default):
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("1", "true", "yes", "on")


def source_format_of(path):
    """源格式：扩展名规范化（jpeg→jpg）；不在可编码闭集里 → 原样小写（用于报告）。"""
    ext = os.path.splitext(str(path))[1].lstrip(".").lower()
    return "jpg" if ext == "jpeg" else (ext or "unknown")


# ---------------------------------------------------------------- convert 计划
def plan_convert(meta, params=None):
    """算 convert 方案（纯函数）：`{in, fmt, srcFmt, quality, keepAlpha, alphaMode, ...}`。

    `meta` = `{path, width, height, bytes, format?}`（宿主 /io/read + 本地 stat）。
    `alphaMode`：
      - `keep`：目标容器支持 alpha 且 keepAlpha=True（alpha 通道原样保留）；
      - `flatten`：目标是无 alpha 容器，或用户主动关掉保留 → 按 background 合成；
      - `auto`：交给宿主判（源无透明像素时宿主回 `absent`）。
    """
    params = dict(params or {})
    path = str(meta.get("path") or "")
    src_fmt = str(meta.get("format") or source_format_of(path)).lower()
    fmt = str(params.get("convertFormat") or DEFAULT_CONVERT_FORMAT).strip().lower()
    if fmt not in FORMATS:
        raise ValueError("convertFormat=%r 不在 %s 内" % (fmt, sorted(FORMATS)))
    keep_alpha = _as_bool(params.get("convertKeepAlpha"), True)
    background = str(params.get("convertBackground") or DEFAULT_BACKGROUND).strip()
    if not background.startswith("#") or len(background) not in (4, 7):
        raise ValueError("convertBackground 必须是 #rgb / #rrggbb，收到 %r" % (background,))
    quality = _as_int(params.get("convertQuality", DEFAULT_CONVERT_QUALITY),
                      "convertQuality", 1, 100, DEFAULT_CONVERT_QUALITY)
    auto_orient = _as_bool(params.get("convertAutoOrient"), True)
    alpha_mode = "keep" if (keep_alpha and fmt in ALPHA_FORMATS) else "flatten"
    return {
        "in": [int(meta.get("width") or 0), int(meta.get("height") or 0)],
        "inBytes": int(meta.get("bytes") or 0),
        "srcFmt": src_fmt,
        "fmt": fmt,
        "quality": quality,
        "qualityApplies": fmt not in LOSSLESS_FORMATS,
        "keepAlpha": keep_alpha,
        "alphaMode": alpha_mode,
        "background": background,
        "autoOrient": auto_orient,
        "sameFormat": src_fmt == fmt,
    }


def convert_action(asset, params, gw, work_dir):
    """`convert`：过宿主 `/op/encode` 重编码到目标格式（alpha / 方向 / 质量由宿主执行）。"""
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
    except Exception as exc:                       # noqa: BLE001 —— 网关错成行
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "convert"})
    meta = {
        "path": path,
        "width": int(_got(info, "width") or 0),
        "height": int(_got(info, "height") or 0),
        "bytes": int(_got(info, "size") or 0),
        "format": source_format_of(path),
    }
    try:
        plan = plan_convert(meta, params)
    except Exception as exc:                       # noqa: BLE001 —— 参数错 → -32003
        return _failed_row(asset, -32003, str(exc), {"action": "convert"})

    out_path = _out_path(work_dir, asset, "convert", path, plan["fmt"])
    fields = {
        "action": "convert",
        "source": path,
        "in": plan["in"],
        "inBytes": plan["inBytes"],
        "srcFormat": plan["srcFmt"],
        "format": plan["fmt"],
        "quality": plan["quality"],
        "qualityApplies": plan["qualityApplies"],
        "requestedKeepAlpha": plan["keepAlpha"],
        "alphaMode": plan["alphaMode"],
        "outPath": out_path,
    }
    try:
        reply = gw.encode(path, out_path, quality=plan["quality"], background=plan["background"],
                          auto_orient=plan["autoOrient"])
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), fields)

    out_size = [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)]
    got_alpha = _got(reply, "alpha")
    fields.update({
        "out": out_size,
        "outBytes": int(_got(reply, "bytes") or 0),
        "alpha": got_alpha,
        "orientation": _got(reply, "orientation"),
        "qualityApplied": bool(_got(reply, "quality_applied")),
        "sha256": _sha256(out_path) if os.path.exists(out_path) else None,
    })
    # 用户要保留 alpha 而宿主回来讲"合成掉了" → 这是能力/参数冲突，如实报失败，
    # 别把「悄悄丢了透明通道」当成功。
    if plan["alphaMode"] == "keep" and got_alpha == "flattened":
        return _failed_row(asset, -32000,
                           "要求保留 alpha 但宿主合成了背景色（目标 %s）" % plan["fmt"], fields)
    return _ok_row(asset, fields, {
        "ops": ["/io/read", "/op/encode"],
        "opDetail": _got(reply, "op"),
        "formatMatchesPlan": (_got(reply, "format") or "").lower() == plan["fmt"],
        "alpha": got_alpha,
        "orientationApplied": int(_got(reply, "orientation") or 1) != 1,
        "qualityApplied": bool(_got(reply, "quality_applied")),
        "sameFormat": plan["sameFormat"],
    })


# ---------------------------------------------------------------- compress 计划
def plan_compress(meta, params=None):
    """算 compress 政策（纯函数）：`{mode, targetBytes, minQuality, maxQuality, ...}`。"""
    params = dict(params or {})
    path = str(meta.get("path") or "")
    in_bytes = int(meta.get("bytes") or 0)
    src_fmt = str(meta.get("format") or source_format_of(path)).lower()
    mode = str(params.get("compressMode") or "target_bytes").strip()
    if mode not in ("target_bytes", "quality"):
        raise ValueError("compressMode=%r 只能是 target_bytes / quality" % (mode,))
    fmt = str(params.get("compressFormat") or "preserve").strip().lower()
    if fmt == "preserve":
        fmt = src_fmt if src_fmt in FORMATS else "jpg"
    elif fmt not in FORMATS:
        raise ValueError("compressFormat=%r 不在 %s 内" % (fmt, sorted(FORMATS) + ["preserve"]))
    plan = {
        "mode": mode,
        "format": fmt,
        "srcFormat": src_fmt,
        "inBytes": in_bytes,
        "in": [int(meta.get("width") or 0), int(meta.get("height") or 0)],
        "skip": None,
    }
    if mode == "quality":
        plan["quality"] = _as_int(params.get("compressQuality", 82), "compressQuality", 1, 100, 82)
        return plan
    target = _as_int(params.get("compressTargetBytes", DEFAULT_TARGET_BYTES),
                     "compressTargetBytes", 10 * 1024, 20 * 1024 * 1024, DEFAULT_TARGET_BYTES)
    min_q = _as_int(params.get("compressMinQuality", DEFAULT_MIN_QUALITY),
                    "compressMinQuality", 20, 95, DEFAULT_MIN_QUALITY)
    pref_q = _as_int(params.get("compressPreferredQuality", DEFAULT_PREFERRED_QUALITY),
                     "compressPreferredQuality", 40, 95, DEFAULT_PREFERRED_QUALITY)
    if pref_q < min_q:
        raise ValueError("compressPreferredQuality=%d 不能低于 compressMinQuality=%d"
                         % (pref_q, min_q))
    plan.update({
        "targetBytes": target,
        "minQuality": min_q,
        "preferredQuality": pref_q,
        "maxQuality": max(DEFAULT_MAX_QUALITY, pref_q),
        "allowDownscale": _as_bool(params.get("compressAllowDownscale"), True),
        "minLongEdge": _as_int(params.get("compressMinLongEdge", DEFAULT_MIN_LONG_EDGE),
                               "compressMinLongEdge", 128, 20000, DEFAULT_MIN_LONG_EDGE),
    })
    if in_bytes and in_bytes <= target:
        plan["skip"] = "already_within_target"     # 已经不超标：不动它（不为了"压过"再糟一遍）
    return plan


def next_downscale_long_edge(cur_long, bytes_now, target_bytes, min_long_edge):
    """按「字节 ≈ 面积」反算下一档长边：`new_long = cur_long * sqrt(target/bytes)`。

    为什么不用固定 0.85 死阶梯：33MP 的图降到 200KB 要跨 6 档，每档都要真编一遍
    （解码 + 编码各几百毫秒），10 张就是几分钟。按体积比反算，通常 1~2 档就落到
    目标附近；仍然留 5% 安全余量（编码字节不是严格 ∝ 面积），并保证**单调缩小**
    （`≤ 0.95 * cur_long`）、不破长边下限。

    返回下一档长边（int）；已经到下限或算出来不缩 → 返回 None（该放弃了）。
    """
    cur_long = int(cur_long)
    target_bytes = max(int(target_bytes), 1)
    bytes_now = max(int(bytes_now), 1)
    floor = max(int(min_long_edge), 1)
    ratio = (target_bytes / float(bytes_now)) ** 0.5
    nxt = int(round(cur_long * ratio * 0.95))
    nxt = min(nxt, int(cur_long * 0.95))          # 至少缩 5%，避免原地打转
    if nxt < floor or nxt >= cur_long:
        return None
    return nxt


def downscale_ladder(size, min_long_edge, step=DEFAULT_DOWNSCALE_STEP,
                     max_steps=MAX_DOWNSCALE_STEPS):
    """**兜底**死阶梯（等比缩长边）：返回 [(scale, long_edge), ...]，第一档是 1.0。

    主搜索用 [`next_downscale_long_edge`]（按体积反算）；本函数只提供
    「每级至少缩多少 / 最多几级 / 长边下限」的边界，单测也用它锁边界。
    """
    w, h = int(size[0]), int(size[1])
    long_edge = max(w, h)
    if long_edge <= 0:
        raise ValueError("源图尺寸非法：%rx%r" % (size[0], size[1]))
    ladder = [(1.0, long_edge)]
    cur = long_edge
    for _ in range(max_steps):
        nxt = int(round(cur * step))
        if nxt < max(min_long_edge, 1) or nxt >= cur:
            break
        ladder.append((round(nxt / float(long_edge), 6), nxt))
        cur = nxt
    return ladder


def bisect_quality(probe, min_quality, max_quality, target_bytes, first_ok=None):
    """在 [min_quality, max_quality] 里找**最大的**满足 `probe(q).bytes <= target` 的 q。

    `probe(q)` 必须是单调的（q 越大字节越多——宿主 jpeg/avif 编码器的真实语义）。
    `first_ok` = 已经测过的 `probe(min_quality)` 结果（省一次编码）。
    返回 `{quality, bytes, probes}`；连 `min_quality` 都超标 → `{quality: None, ...}`。
    """
    probes = []
    lo, hi = int(min_quality), int(max_quality)
    base = first_ok
    if base is None:
        base = probe(lo)
        probes.append({"quality": lo, "bytes": base})
    if base > target_bytes:
        return {"quality": None, "bytes": base, "probes": probes, "floor": lo}
    best = {"quality": lo, "bytes": base}
    # 不变量：best 一定达标。二分找最大达标 q（hi 是已知不达标/上界）
    while lo < hi:
        mid = (lo + hi + 1) // 2
        size = probe(mid)
        probes.append({"quality": mid, "bytes": size})
        if size <= target_bytes:
            lo = mid
            best = {"quality": mid, "bytes": size}
        else:
            hi = mid - 1
    return {"quality": best["quality"], "bytes": best["bytes"], "probes": probes, "floor": None}


def compress_action(asset, params, gw, work_dir):
    """`compress`：质量档单趟；目标体积模式 = 质量二分（不低于底线）+ 必要时降采样。"""
    path = str(asset.get("path") or "")
    if not path:
        return _failed_row(asset, -32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    try:
        info = gw.io_read(path)
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, _rpc_code(exc), str(exc), {"action": "compress"})
    meta = {
        "path": path,
        "width": int(_got(info, "width") or 0),
        "height": int(_got(info, "height") or 0),
        "bytes": int(_got(info, "size") or 0),
        "format": source_format_of(path),
    }
    try:
        plan = plan_compress(meta, params)
    except Exception as exc:                       # noqa: BLE001
        return _failed_row(asset, -32003, str(exc), {"action": "compress"})

    out_path = _out_path(work_dir, asset, "compress", path, plan["format"])
    fields = {
        "action": "compress",
        "source": path,
        "mode": plan["mode"],
        "format": plan["format"],
        "srcFormat": plan["srcFormat"],
        "in": plan["in"],
        "inBytes": plan["inBytes"],
        "outPath": out_path,
    }
    if plan["skip"]:
        fields["out"] = plan["in"]
        fields["outBytes"] = plan["inBytes"]
        return _skipped_row(asset, plan["skip"], fields)

    ops = ["/io/read"]
    attempts = []
    tmp_files = []
    probe_path = _tmp_path(work_dir, "compress-probe", path, "." + plan["format"])
    tmp_files.append(probe_path)        # 探测用临时产物：最后一定删掉（失败也不留半成品）

    def cleanup():
        for f in list(tmp_files):
            try:
                os.remove(f)
            except OSError:
                pass

    def encode_at(src_path, quality, dest=None):
        """一次真编码（记进 attempts）。返回 (bytes, reply)。"""
        dest = dest or out_path
        reply = gw.encode(src_path, dest, quality=quality)
        attempts.append({"quality": int(quality), "bytes": int(_got(reply, "bytes") or 0),
                         "size": [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)],
                         "downscaled": src_path != path})
        return int(_got(reply, "bytes") or 0), reply

    try:
        need_final = True
        if plan["mode"] == "quality":
            q = plan["quality"]
            ops.append("/op/encode")
            reply = encode_at(path, q)[1]
            fields["quality"] = q
            chosen = {"quality": q, "src": path, "downscaled": False}
            need_final = False           # 质量档就这一趟，别再多编一次
        else:
            target = plan["targetBytes"]
            preferred = plan["preferredQuality"]
            cur_long = max(plan["in"])
            native_long = cur_long
            chosen = None
            probe_src = path
            tried = 0
            while True:
                tried += 1
                if tried > MAX_DOWNSCALE_STEPS + 1:
                    break
                if cur_long < native_long:
                    probe_src = _tmp_path(work_dir, "compress-ds%d" % cur_long, path, ".png")
                    gw.upscale(path, cur_long / float(native_long), probe_src)  # scale<=1 重采样
                    ops.append("/op/upscale")
                    tmp_files.append(probe_src)
                else:
                    probe_src = path
                here = {"src": probe_src, "downscaled": probe_src != path, "longEdge": cur_long}
                first = encode_at(probe_src, plan["minQuality"], probe_path)

                def probe_quality(q, _s=probe_src):
                    return encode_at(_s, q, probe_path)[0]

                found = bisect_quality(probe_quality, plan["minQuality"], plan["maxQuality"],
                                       target, first_ok=first[0])
                fits = found["quality"] is not None
                # 质量优先：这一档能达标且质量 ≥ 质量目标 → 接受（不为了"更大尺寸"把质量压到 60）
                if fits and found["quality"] >= preferred:
                    chosen = dict(here, quality=found["quality"])
                    break
                # 缩不动了（关掉降采样 / 已到长边下限）→ 体积优先兜底：底线质量能达标就收
                if not plan["allowDownscale"] or cur_long <= plan["minLongEdge"]:
                    if fits:
                        chosen = dict(here, quality=found["quality"])
                    break
                # 按「质量目标下的字节数」反算下一档长边（测过就用它，没测到就退到底线的字节数）
                ref_bytes = next((a["bytes"] for a in attempts
                                  if a["quality"] == preferred and a["downscaled"] == here["downscaled"]),
                                 None)
                nxt = next_downscale_long_edge(cur_long, ref_bytes or first[0], target,
                                               plan["minLongEdge"])
                if nxt is None:
                    if cur_long > plan["minLongEdge"]:
                        nxt = plan["minLongEdge"]      # 夹到长边下限，再试最后一次
                    else:
                        if fits:        # 到下限了：能达标就用这一档（质量 ≥ 底线）
                            chosen = dict(here, quality=found["quality"])
                        break
                cur_long = nxt
            if chosen is None:
                best = min((a["bytes"] for a in attempts), default=plan["inBytes"])
                cleanup()
                hint = ""
                if plan["format"] in LOSSLESS_FORMATS:
                    hint = "（%s 是无损容器，quality 不生效；目标体积模式请把 compressFormat 设为 jpg/avif）" \
                        % plan["format"]
                return _failed_row(
                    asset, -32000,
                    "目标体积 %dB 在质量底线 %d 内不可达（已试到长边 %d，最小 %dB）%s；"
                    "请放宽 compressMinQuality 或允许更小长边" % (
                        target, plan["minQuality"], cur_long, best, hint),
                    dict(fields, attempts=attempts))
            fields["quality"] = chosen["quality"]
            fields["downscaled"] = bool(chosen["downscaled"])
            fields["outLongEdge"] = int(chosen.get("longEdge") or max(plan["in"]))

        # 最终产物按"选中的组合"再编一次：保证磁盘上的文件 == 结果行里的数值
        # （质量档模式已经是这条路，不再重复编码）
        if need_final:
            ops.append("/op/encode")
            _, reply = encode_at(chosen["src"], chosen["quality"])
    except Exception as exc:                       # noqa: BLE001
        cleanup()
        return _failed_row(asset, _rpc_code(exc), str(exc), dict(fields, attempts=attempts))

    out_size = [int(_got(reply, "width") or 0), int(_got(reply, "height") or 0)]
    out_bytes = int(_got(reply, "bytes") or 0)
    fields.update({
        "out": out_size,
        "outBytes": out_bytes,
        "ratio": round(plan["inBytes"] / float(out_bytes), 3) if out_bytes else None,
        "attempts": len(attempts),
        "qualityApplied": bool(_got(reply, "quality_applied")),
        "sha256": _sha256(out_path) if os.path.exists(out_path) else None,
    })
    cleanup()                                      # 中间产物用完即删（一次动作只留一张产物）
    evidence = {
        "ops": ops,
        "opDetail": _got(reply, "op"),
        "probes": attempts,
        "qualityApplied": bool(_got(reply, "quality_applied")),
    }
    if plan["mode"] == "target_bytes":
        evidence.update({
            "targetBytes": plan["targetBytes"],
            "minQuality": plan["minQuality"],
            "preferredQuality": plan["preferredQuality"],
            "withinTarget": out_bytes <= plan["targetBytes"],
            "qualityFloorRespected": (fields["quality"] or 0) >= plan["minQuality"]
            if plan["format"] not in LOSSLESS_FORMATS else None,
        })
    return _ok_row(asset, fields, evidence)


ACTIONS = {"convert": convert_action, "compress": compress_action}
