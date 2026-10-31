#!/usr/bin/env python3
"""超分包（`com.soujpg.pack.upscale`）—— Real-ESRGAN 2×/4×，`kinds: ["process"]`。

一句话：**把图放大 2 倍或 4 倍**，产物是资产工作版本的 PNG（无损），**没有审核结论**
（处理类不伪造 pass）。

引擎两档（`engine` 参数，同一插件内不改变算法家族，只换执行档）：
    - `full`：宿主网关 `/op/upscale scale=4` → `engine-restore` 的 realesr-general-x4v3
      （ONNX，4.7MB，CPU 可跑；单张 2~5s，峰值内存 ~700MB/4MP 输入）。2× 的实现是
      「4× 模型 + `scale=0.5` 重采样」——模型只有 4× 权重，硬合 2× 权重是另一件事。
    - `light`：宿主网关 `/op/upscale scale=N` 单趟重采样（毫秒级，无模型依赖）。
      耗时/体积差两个数量级，所以放在**同一个参数**里由用户选，而不是拆成两个插件。

分块（`tile`）：模型单次输入护栏 4MP，超了网关会退回重采样（静默降质）。`tile=auto`
只在大图（>4MP）时按 1536² 分块；显式给块边长则照走分块路径。拼接在包内做
（协议没有 composite 端点），见 `plugin/tiling.py`。

并发闸门：`engine=full` 进 CPU 闸门（默认 1 槽，跨进程 flock），等待期间发
`progress{step:"waiting"}` 让任务中心显示「等待中」；`engine=light` 不进闸门。

零第三方依赖（`requirements.txt` 只有注释）：宿主装包不 `pip install`，所以像素编解码
自带 `plugin/pngio.py`（8bit RGB/RGBA，五种 filter），推理与几何一律走宿主网关
（协议 §7 铁律 3：包内不实现算法）。
"""
from __future__ import annotations

import errno
import hashlib
import json
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cpulock                      # noqa: E402
import gw as gateway_mod            # noqa: E402
import pngio                        # noqa: E402
import tiling                       # noqa: E402

PACK_VERSION = "1.0.1"
API_VERSION = "1.1"

# ---------------------------------------------------------------- 动作面（N6e / KI-40）
# 清单**唯一声明处 = `resources/actions.json`**（与 segment 包同一机制）：宿主的动作链与
# 「官方默认方案」派生（spec_seed）都读它。本包只干一件事（高清放大），2×/4×、engine、
# tile 都是**参数**（params.schema），不编第二个动作名。
ACTIONS_PATH = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                            "resources", "actions.json")
try:
    with open(ACTIONS_PATH, encoding="utf-8") as _fh:
        ACTIONS_INFO = [dict(item) for item in json.load(_fh)["actions"]]
except (OSError, ValueError, KeyError, TypeError) as _exc:      # 缺清单比运行时 400 好查一万倍
    raise RuntimeError("动作清单装载失败（%s）：%s" % (ACTIONS_PATH, _exc))
if not ACTIONS_INFO or not all(str(a.get("id") or "").strip() for a in ACTIONS_INFO):
    raise RuntimeError("动作清单为空或缺 id（%s）" % ACTIONS_PATH)
ACTION_IDS = tuple(str(a["id"]) for a in ACTIONS_INFO)

PACK_INFO = {
    "id": "com.soujpg.pack.upscale",
    "name": "超分（Real-ESRGAN 2x/4x）",
    "version": PACK_VERSION,
    "api_version": API_VERSION,
    # 处理类包没有「规则」：唯一的动作就是「按方案参数高清放大」。
    "rules": [],
    # N6e 动作清单：宿主动作链按 `options.action` 选动作，权威清单就是这里。
    "actions": [dict(action) for action in ACTIONS_INFO],
    "capabilities": ["io.read", "op.geometric", "report.emit"],
    "deterministic": True,
    "timeout_secs": 600,
    "kinds": ["process"],
}

# 与宿主 engine-restore::upscale::MAX_INPUT_PIXELS 同口径（4MP）：超了网关不走模型。
MODEL_INPUT_CAP = 4_000_000
# 与宿主 gateway::MAX_OUTPUT_PIXELS / UPSCALE_MAX_LONG_EDGE 同口径（产物硬门槛）
MAX_OUTPUT_PIXELS = 100_000_000
MAX_LONG_EDGE = 20_000

SCALES = (2, 4)
ENGINES = ("full", "light")
TILES = ("auto", "384", "768", "1536")

_ERR_CAPABILITY = -32001
_ERR_INVALID = -32003
_ERR_OTHER = -32000

_STATE_LOCK = threading.Lock()
_REPORTS = {}
_CANCEL = {}
# serve() 装进来的协议通知出口（progress/log）
NOTIFY = None


class RpcError(Exception):
    """直通协议 §5 错误码的业务错误（宿主原样上报，不吞成成功）。"""

    def __init__(self, code, message, data=None):
        super().__init__(message)
        self.code = int(code)
        self.message = message
        self.data = data


def log(message):
    # 协议 §9：日志走 stderr（stdout 被 serve() 挪去当协议流）
    sys.stderr.write("[upscale] %s\n" % (message,))
    sys.stderr.flush()


def notify(payload):
    if NOTIFY is None:
        return
    try:
        NOTIFY({"jsonrpc": "2.0", "method": "progress", "params": payload})
    except Exception as exc:  # noqa: BLE001 —— 通知失败不能影响正事
        log("progress 通知失败：%r" % (exc,))


def gw(path, payload):
    """统一网关出口（测试用 `_STUB_GATEWAY` 替换）。"""
    if _STUB_GATEWAY is not None:
        return _STUB_GATEWAY.call(path, payload)
    return gateway_mod.shared().call(path, payload)


_STUB_GATEWAY = None


def _cancel_flag(job_id):
    with _STATE_LOCK:
        ev = _CANCEL.get(job_id)
        if ev is None:
            ev = threading.Event()
            _CANCEL[job_id] = ev
        return ev


def _check_cancel(job_id, step):
    if job_id and _cancel_flag(job_id).is_set():
        raise RpcError(-32004, "已被宿主取消（%s 阶段）" % (step,))


# --- 参数 --------------------------------------------------------------------
def normalize_options(options):
    """把宿主下发的全量生效值收敛成内部结构（宿主已按 schema 校验，这里只兜格式）。

    越界/非法一律 -32003：**不做静默纠正**（把 scale=3 悄悄当 2 跑是最坏的降级）。
    """
    options = dict(options or {})
    workdir = options.get("workdir") or os.environ.get("DAMPACK_WORK_DIR") or os.getcwd()

    raw_scale = options.get("scale", 2)
    try:
        scale = int(raw_scale)
    except (TypeError, ValueError):
        raise RpcError(_ERR_INVALID, "scale 必须是整数（2 或 4），收到 %r" % (raw_scale,))
    if scale not in SCALES:
        raise RpcError(_ERR_INVALID, "scale 只支持 2 或 4（收到 %d）："
                                     "模型是 4× 权重，2× 由 4×→0.5 重采样落" % scale)

    engine = str(options.get("engine", "full"))
    if engine not in ENGINES:
        raise RpcError(_ERR_INVALID, "engine 只支持 %s（收到 %r）" % ("/".join(ENGINES), engine))

    raw_tile = options.get("tile", "auto")
    tile = "auto" if raw_tile is None else str(raw_tile)
    if tile not in TILES:
        raise RpcError(_ERR_INVALID, "tile 只支持 %s（收到 %r）" % ("/".join(TILES), tile))
    if engine == "light" and tile != "auto":
        raise RpcError(_ERR_INVALID, "tile 只对 engine=full 生效：重采样档（light）没有"
                                     "模型输入护栏，不需要分块；要分块请把 engine 设成 full")

    raw_denoise = options.get("denoise", 0)
    try:
        denoise = float(raw_denoise)
    except (TypeError, ValueError):
        raise RpcError(_ERR_INVALID, "denoise 必须是 0..1 的数，收到 %r" % (raw_denoise,))
    if denoise < 0 or denoise > 1:
        raise RpcError(_ERR_INVALID, "denoise 越界：%s（允许 0..1）" % (denoise,))
    if denoise > 0 and engine != "full":
        raise RpcError(_ERR_INVALID, "denoise>0 需要 engine=full：降噪是本包走的 x4v3 模型"
                                     "自带能力，重采样档没有降噪通道（不做静默忽略）")

    return {
        "workdir": workdir,
        "scale": scale,
        "engine": engine,
        "tile": tile,
        "denoise": denoise,
    }


def _workdir(opts):
    workdir = opts["workdir"]
    os.makedirs(workdir, exist_ok=True)
    return workdir


def _safe(name):
    keep = "-_.()"
    return "".join(c if (c.isalnum() or c in keep) else "_" for c in str(name))[:80] or "asset"


def _artifact_path(workdir, asset_id, scale, engine):
    return os.path.join(workdir, "%s-upscale-%dx-%s.png" % (_safe(asset_id), scale, engine))


def _remove_quietly(*paths):
    for path in paths:
        try:
            os.remove(path)
        except OSError:
            pass


# 行级错误码（协议 §4.4：`error.code` 是**稳定词字符串**，不是 RPC 数字码 ——
# 数字码属于 JSON-RPC error 帧；结果行给词，宿主/界面按词聚合）。
_ROW_CODE_WORDS = {
    _ERR_CAPABILITY: "capability_missing",
    _ERR_INVALID: "invalid_params",
    _ERR_OTHER: "internal",
}


def _failed(asset_id, code, message, evidence):
    """结构化失败行：`error{code,message}` 必带（协议 §4.4），绝不只报 invalid_params。"""
    word = code if isinstance(code, str) else _ROW_CODE_WORDS.get(int(code), "internal")
    return {"assetId": asset_id, "result": "failed",
            "error": {"code": word, "message": message},
            "evidence": evidence}


# --- 执行面 ------------------------------------------------------------------
def _model_used(resp, step):
    """确认这一步真的动了模型；不是就响亮失败（engine=full 的静默降级 = 骗人）。"""
    op = str(resp.get("op") or "")
    if not op.startswith("upscale.realesr"):
        raise RpcError(
            _ERR_CAPABILITY,
            "engine=full 要求走 Real-ESRGAN 模型，但宿主在 %s 阶段报了 op=%r（重采样）。"
            "常见原因：模型 realesr-general-x4v3 未安装、本构建未编入 local-infer、"
            "或输入 %d px 超过模型护栏 %d px。请先装模型/改用 tile 分块或 engine=light。"
            % (step, op or "?", 0, MODEL_INPUT_CAP),
        )
    return op


def _tiled_upscale4(job_id, path, width, height, block, workdir, tag, opts):
    """分块走 4× 模型 → 4× 画布（bytes）。返回 `(canvas, cw, ch, tiles, steps)`。"""
    try:
        canvas, cw, ch = tiling.new_canvas(width, height, 4)
    except ValueError as exc:
        raise RpcError(_ERR_INVALID, str(exc))
    boxes = tiling.grid(width, height, block)
    steps = []
    for index, box in enumerate(boxes):
        _check_cancel(job_id, "tile-%d" % index)
        pbox = tiling.padded_box(box, width, height)
        tile_png = os.path.join(workdir, "%s-tile%02d.png" % (tag, index))
        up_png = os.path.join(workdir, "%s-tile%02d-4x.png" % (tag, index))
        try:
            gw("/op/crop", {"path": path, "box": list(pbox), "out": tile_png})
            resp = gw("/op/upscale", {"path": tile_png, "scale": 4, "out": up_png})
            op = _model_used(resp, "分块 #%d" % index)
            tw, th, tch, raw = pngio.decode_path(up_png)
            dst_x, dst_y, src_x, src_y, w, h = tiling.stitch_offsets(box, pbox, 4)
            tiling.blit(canvas, cw, ch, tw, th, tch, raw, dst_x, dst_y,
                        src_x, src_y, w, h)
            steps.append({"tile": index, "box": list(box), "cropBox": list(pbox),
                          "op": op, "out": [tw, th]})
        finally:
            _remove_quietly(tile_png, up_png)
    return canvas, cw, ch, boxes, steps


def upscale_asset(job_id, asset, opts):
    """一个资产 → 一行结果（处理类：result/artifacts/evidence，无 verdict）。"""
    asset = asset or {}
    path = asset.get("path")
    if not isinstance(path, str) or not path.strip():
        raise RpcError(_ERR_INVALID, "asset.path 必填")
    asset_id = asset.get("id") or os.path.basename(path)
    workdir = _workdir(opts)
    _check_cancel(job_id, "start")
    scale, engine, tile, denoise = opts["scale"], opts["engine"], opts["tile"], opts["denoise"]
    started = time.time()
    stamp = "%s-%s" % (_safe(asset_id), hashlib.sha256(
        ("%s|%s|%s|%s|%s" % (path, scale, engine, tile, denoise)).encode("utf-8")).hexdigest()[:8])

    info = gw("/io/read", {"path": path})
    width, height = int(info["width"]), int(info["height"])
    out_path = _artifact_path(workdir, asset_id, scale, engine)
    expect_w, expect_h = width * scale, height * scale

    evidence = {
        "engine": engine,
        "scale": scale,
        "denoise": denoise,
        "denoiseApplied": "x4v3-builtin" if denoise > 0 else "none",
        "tile": tile,
        "inputWidth": width, "inputHeight": height,
        "expectedWidth": expect_w, "expectedHeight": expect_h,
        "modelGuardPx": MODEL_INPUT_CAP,
        "cpuSlots": cpulock.slots_from_env(),
    }
    if expect_w * expect_h > MAX_OUTPUT_PIXELS or max(expect_w, expect_h) > MAX_LONG_EDGE:
        max_scale = min((MAX_OUTPUT_PIXELS / float(width * height)) ** 0.5,
                        MAX_LONG_EDGE / float(max(width, height)))
        message = ("目标 %dx%d（%d 像素）超产物上限：像素 ≤%d、长边 ≤%d。"
                   "该源图的可用上限 scale≤%.2f；请调小 scale 或先缩小源图"
                   % (expect_w, expect_h, expect_w * expect_h, MAX_OUTPUT_PIXELS,
                      MAX_LONG_EDGE, max_scale))
        evidence["limit"] = {"maxOutputPixels": MAX_OUTPUT_PIXELS,
                             "maxLongEdge": MAX_LONG_EDGE, "maxScale": round(max_scale, 3)}
        return _failed(asset_id, "target_over_limit", message, evidence)

    steps = []
    try:
        if engine == "light":
            gate_ms = 0
            resp = gw("/op/upscale", {"path": path, "scale": scale, "out": out_path})
            steps.append({"mode": "resample", "op": resp.get("op"), "scale": scale,
                          "out": [int(resp["width"]), int(resp["height"])]})
        else:
            block = tiling.block_for(tile, width, height, MODEL_INPUT_CAP)
            evidence["tileBlock"] = block
            with cpulock.CpuGate(notify=notify, job_id=job_id) as gate:
                gate_ms = 0
                if block is None:
                    if scale == 4:
                        resp = gw("/op/upscale", {"path": path, "scale": 4, "out": out_path})
                        steps.append({"mode": "model", "op": _model_used(resp, "整图"),
                                      "scale": 4,
                                      "out": [int(resp["width"]), int(resp["height"])]})
                    else:
                        mid = os.path.join(workdir, "%s-4x.png" % stamp)
                        try:
                            resp = gw("/op/upscale", {"path": path, "scale": 4, "out": mid})
                            steps.append({"mode": "model", "op": _model_used(resp, "整图"),
                                          "scale": 4,
                                          "out": [int(resp["width"]), int(resp["height"])]})
                            resp = gw("/op/upscale", {"path": mid, "scale": 0.5, "out": out_path})
                            steps.append({"mode": "resample", "op": resp.get("op"),
                                          "scale": 0.5,
                                          "out": [int(resp["width"]), int(resp["height"])]})
                        finally:
                            _remove_quietly(mid)
                else:
                    canvas, cw, ch, boxes, tile_steps = _tiled_upscale4(
                        job_id, path, width, height, block, workdir, stamp, opts)
                    evidence["tiles"] = len(boxes)
                    steps.extend(tile_steps)
                    if scale == 4:
                        pngio.write_png(out_path, cw, ch, 3, canvas)
                        steps.append({"mode": "stitch", "op": "upscale.tiled",
                                      "scale": 4, "out": [cw, ch]})
                    else:
                        canvas_png = os.path.join(workdir, "%s-canvas.png" % stamp)
                        try:
                            pngio.write_png(canvas_png, cw, ch, 3, canvas)
                            resp = gw("/op/upscale", {"path": canvas_png, "scale": 0.5,
                                                     "out": out_path})
                            steps.append({"mode": "resample", "op": resp.get("op"),
                                          "scale": 0.5,
                                          "out": [int(resp["width"]), int(resp["height"])]})
                        finally:
                            _remove_quietly(canvas_png)
                gate_ms = gate.wait_ms
        _check_cancel(job_id, "encode")
    except gateway_mod.GatewayError as exc:
        evidence["elapsedMs"] = int((time.time() - started) * 1000)
        evidence["steps"] = steps
        return _failed(asset_id, exc.code, "宿主网关调用失败：%s" % (exc,), evidence)
    except RpcError as exc:
        # 单资产级失败（能力缺失 / 目标超限）→ 落 failed 行，不炸整批；取消必须继续上抛
        if exc.code == -32004:
            raise
        evidence["elapsedMs"] = int((time.time() - started) * 1000)
        evidence["steps"] = steps
        return _failed(asset_id, exc.code, exc.message, evidence)
    except MemoryError:
        # 分块画布/块解码是本包唯一的大内存点（4× 画布 ≤1e8 px ≈ 300MB）。
        # 落**结构化失败行**（-32000 兜底 + 可读 message + 能照做的建议），
        # 不是把整批炸成一条「内部错误」。
        evidence["elapsedMs"] = int((time.time() - started) * 1000)
        evidence["steps"] = steps
        evidence["resource"] = {"kind": "memory", "failedAtStep": len(steps)}
        return _failed(asset_id, _ERR_OTHER,
                       "内存不足：画布/块解码吃满了本机可用内存（已做 %d 步）。"
                       "请改小 tile（384 或 768）、降低 scale，或分批处理" % len(steps),
                       evidence)
    except OSError as exc:
        if exc.errno != errno.ENOSPC:
            raise
        evidence["elapsedMs"] = int((time.time() - started) * 1000)
        evidence["steps"] = steps
        evidence["resource"] = {"kind": "disk", "failedAtStep": len(steps)}
        return _failed(asset_id, _ERR_OTHER,
                       "磁盘写满：产物或中间块写不进 %s（%s）。请清理工作版本目录或换盘后重试"
                       % (workdir, exc), evidence)

    with open(out_path, "rb") as fh:
        digest = hashlib.sha256(fh.read()).hexdigest()
    out_w, out_h, _channels = pngio.detect_size(out_path)
    elapsed_ms = int((time.time() - started) * 1000)

    evidence.update({
        "outWidth": out_w, "outHeight": out_h,
        "sizeExact": (out_w, out_h) == (expect_w, expect_h),
        "format": "png",
        "steps": steps,
        "waitMs": gate_ms,
        "elapsedMs": elapsed_ms,
        "startedAtMs": int(started * 1000),
        "sha256": digest,
    })
    engine_label = "Real-ESRGAN x4v3" if engine == "full" else "重采样（fast）"
    return {
        "assetId": asset_id,
        "result": "ok",
        "artifacts": [{"kind": "export_file", "path": out_path, "sha256": digest}],
        "evidence": evidence,
        "fields": {
            # 产物的**机读去处**（协议 §4.4 `fields`，KI-38 对齐 segment 口径）：
            # 宿主动作链的链式衔接与「链尾落工作版本」读的就是这几个键
            # （`compliance_chain::ChainStep::from_outcome` 读 outPath/sha256/out/format/bytes，
            # 缺 `outPath` 会把「成功」判成 `action_no_product`）。中文列给人看，两套并存。
            "outPath": out_path,
            "sha256": digest,
            "out": [out_w, out_h],
            "format": "png",
            "bytes": os.path.getsize(out_path) if os.path.isfile(out_path) else None,
            "放大": "%d×（%s）" % (scale, engine_label),
            "输出": "%dx%d PNG" % (out_w, out_h),
        },
    }


# --- 方法实现 ----------------------------------------------------------------
def _rows_reply(rows):
    return {"rows": rows}


def handle_process(job_id, asset, options, rules=None):
    """`judge` 与 `process` 同一实现：处理类包的宿主入口是 `judge`（协议 v1.1），
    `process` 是同义显式名（向前兼容，不改变语义）。"""
    del rules          # 处理类没有规则选择面
    opts = normalize_options(options)
    row = upscale_asset(job_id, asset, opts)
    with _STATE_LOCK:
        _REPORTS.setdefault(job_id, []).append(row)
    if row.get("result") == "ok":
        try:
            gw("/report/emit", {"job_id": job_id, "assetId": row["assetId"],
                                "kind": "process", "artifacts": row["artifacts"]})
        except Exception as exc:  # noqa: BLE001 —— 上报失败不影响产物，但必须留痕
            log("report/emit 失败：%r" % (exc,))
    return _rows_reply([row])


def _require_job_id(params):
    job_id = params.get("job_id") or (params.get("params") or {}).get("job_id")
    if not isinstance(job_id, str) or not job_id.strip():
        raise RpcError(_ERR_INVALID, "job_id 必填")
    return job_id


def m_judge(params):
    params = params or {}
    return handle_process(_require_job_id(params), params.get("asset"),
                          params.get("options"), params.get("rules"))


def m_report(params):
    job_id = str((params or {}).get("job_id") or "")
    with _STATE_LOCK:
        rows = list(_REPORTS.get(job_id, []))
    return {"job_id": job_id, "assets": rows, "count": len(rows)}


def m_cancel(params):
    job_id = str((params or {}).get("job_id") or "")
    if job_id:
        _cancel_flag(job_id).set()
    return {"canceled": True}


def m_info(params):
    del params
    return dict(PACK_INFO)


def m_health(params):
    del params
    return {"status": "ok", "pid": os.getpid(), "version": PACK_VERSION,
            "api_version": API_VERSION}


HANDLERS = {
    "pack.info": m_info,
    "health": m_health,
    "judge": m_judge,
    "process": m_judge,
    "report": m_report,
    "cancel": m_cancel,
}


# --- stdio 协议壳（协议 §9：三条必守） ---------------------------------------
_ERR_PARSE = -32700
_ERR_METHOD = -32601


def dispatch(frame):
    method = frame.get("method")
    params = frame.get("params")
    if not isinstance(params, dict):
        params = {}
    handler = HANDLERS.get(method)
    if handler is None:
        raise RpcError(_ERR_METHOD, "未知方法 %r（支持 %s）" % (method, sorted(HANDLERS)))
    return handler(params)


def serve(stdin=None, stdout=None):
    global NOTIFY
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    write_lock = threading.Lock()

    def reply(frame):
        with write_lock:
            stdout.write(json.dumps(frame, ensure_ascii=False) + "\n")
            stdout.flush()

    NOTIFY = reply

    def run(frame):
        req_id = frame.get("id")
        try:
            result = dispatch(frame)
            reply({"jsonrpc": "2.0", "id": req_id, "result": result})
        except RpcError as exc:
            err = {"code": exc.code, "message": exc.message}
            if exc.data is not None:
                err["data"] = exc.data
            reply({"jsonrpc": "2.0", "id": req_id, "error": err})
        except Exception as exc:  # noqa: BLE001 —— 任何未捕获异常都不能吞成成功
            # 协议 §5「禁自造码」：闭集外的 code 一律算违例（曾用 -32603，属自造）。
            # 兜底必须是 -32000 + 可读 message。
            log("内部错误：%r" % (exc,))
            reply({"jsonrpc": "2.0", "id": req_id,
                   "error": {"code": _ERR_OTHER, "message": "内部错误：%s" % (exc,)}})

    for line in stdin:
        line = line.strip()
        if not line:
            continue
        try:
            frame = json.loads(line)
        except ValueError as exc:
            reply({"jsonrpc": "2.0", "id": None,
                   "error": {"code": _ERR_PARSE, "message": "解析失败：%s" % (exc,)}})
            continue
        if "method" not in frame:
            reply({"jsonrpc": "2.0", "id": frame.get("id"),
                   "error": {"code": _ERR_PARSE, "message": "缺 method"}})
            continue
        # 协议 §9 第 3 条：每个请求一个工作线程 —— judge 跑着的 40s 里 `cancel` 必须能进来
        threading.Thread(target=run, args=(frame,), daemon=True).start()


def main():
    # 协议 §9：先保存真 stdout，再把 fd1 接到 stderr —— 任何库里的 print 都污染不了协议流
    proto_out = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    try:
        serve(sys.stdin, proto_out)
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
