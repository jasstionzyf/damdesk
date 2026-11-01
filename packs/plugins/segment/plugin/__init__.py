#!/usr/bin/env python3
"""抠图 / 换背景行业包（官方包）—— 插件入口（`docs/industry-pack-protocol.md` API v1.1）。

启动形态：宿主 `pack-host` 以 stdio 行式 JSON-RPC 拉起本进程
（`python3 plugin/__init__.py`，cwd=包目录，env 带 `DAMPACK_GATEWAY_URL/TOKEN`）。

本文件三件事：
1. **协议壳**：收行 → 校验 → 派发 `HANDLERS` → 回帧；stdout 只走协议行，日志一律 stderr。
2. **编排**（`process` = 宿主眼里的 `judge`）：`/io/read` 尺寸 → `/detect/u2net` 掩码 +
   主体框 → 只裁主体框那一块（`/op/crop` 出 PNG 副本 + 掩码副本，省掉全图解码）→
   包内 alpha 合成（`composite.py`）→ 产物落 `options.workdir`。
3. **能力调用**：网关是唯一对外出口（`gw()`），包自己不解码原图、不出网、不装依赖。

产物形态（结果契约 §4.4，`outputs=["artifact"]` → **没有 verdict**）：
- `bgMode=transparent` → RGBA **PNG**（alpha = 掩码斜坡，RGB 非预乘原色，无白边）；
- `bgMode=white|color` → 包内合成 RGB → 借 `/op/crop` 让**宿主**编码成 **JPEG**
  （包内不写 JPEG 编码器：纯 Python 的 DCT/Huffman 又慢又难对齐质量口径）。
- 中间件（裁块 / 掩码副本 / 合成 PNG）跑完即删，只留最终产物：一个资产一次一个文件，
  不新建目录树、不落平行资产表。

三道守卫（K07-FIX-N `t_5c968bc9`，阈值与实测标定见 README「守卫」）：
1. **已白底跳过**（目标底色是纯白时）：宿主 `/detect/white_stats` 实测输入边缘带
   （band=16px，⊇ 门禁 4px 口径）worst_dev ≤ 3 → `result=ok` + `skipReason="already_white"`，
   **不产生产物**（原图就是想要的形态；更不去跑 u2net）。宿主管不到这项统计（老网关 /
   未授权 `detect.white`）时**不猜**：退化成照常处理，evidence 标 `unavailable`。
2. **掩码低置信度拒绝**：`fg_ratio` 落到 `(0, 0.02)` 或 `0.95+` 这类退化区间（u2net 把整图
   判成主体 / 只抓到噪点）→ `result=skipped` + `skipReason="mask_low_confidence"`，
   不拿一张错掩码去刷图。
3. **主体损失红线**：替换后**主体不透明核心**（`alpha==255`，原色照搬的那部分）被改动的像素
   占比 > 5% → `result=failed` + `error.code="subject_loss"`，不落产物
   （防坐标映射/合成错位这类回归）。斜坡像素 `0<alpha<255` 本来就该混背景色，不计入。
"""
from __future__ import annotations

import hashlib
import http.client
import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request

API_VERSION = "2.0"
PACK_VERSION = "2.0.0"
PACK_ID = "com.soujpg.pack.segment"

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import composite         # noqa: E402
import pngio             # noqa: E402

_SAFE = re.compile(r"[^0-9A-Za-z_.-]+")

# ---------------------------------------------------------------- 动作面（N6e）
# 宿主动作链按 `options.action` 选动作（`crates/svc-pipeline/src/compliance_chain.rs`），
# 权威清单 = 本文件的 `pack.info.actions[].id`（`crates/app-web/src/compliance_api.rs`
# 的 `PluginRuntime::open` 读它做入参校验：未声明的动作一律 400 点名）。
#
# 本包**只声明一个动作**：`white_bg_fill`（= 抠图 + 合成纯白底，与语料/门禁 need 名、
# 界面「不可逆动作」表同一套词）。透明底 / 指定色底不编动作名 —— 它们是**参数**
# （`bgMode=transparent|color`），参数面已经够表达，多编动作只会让同一件事有两个入口。
#
# `action` 刻意**不进 `params.schema`**：schema 的默认值会被宿主注入每一次执行
# （`ParamSchema::resolve_json`），一旦声明了 `action` 默认值，`bgMode=transparent`
# 的普通规格也会被注入「白底动作」→ 语义被静默改掉。链内由宿主在调用时补
# `options.action`（`PluginRuntime::action_asset`），不走 schema。
#
# 清单**唯一声明处 = `resources/actions.json`**（不是本文件）：宿主 `spec_seed` 也要读同一份
# 声明来派生「官方默认方案」（图库里「送去处理 → 抠图/换背景」默认要能选到一条方案）。
# 写在 Python 里宿主读不到（读它必须拉起子进程），写在资源里两边都能读 —— 一处声明两处消费。
ACTIONS_PATH = os.path.join(os.path.dirname(_HERE), "resources", "actions.json")
try:
    with open(ACTIONS_PATH, encoding="utf-8") as _fh:
        ACTIONS_INFO = [dict(item) for item in json.load(_fh)["actions"]]
except (OSError, ValueError, KeyError, TypeError) as _exc:      # 缺清单比运行时 400 好查一万倍
    raise RuntimeError("动作清单装载失败（%s）：%s" % (ACTIONS_PATH, _exc))
if not ACTIONS_INFO or not all(str(a.get("id") or "").strip() for a in ACTIONS_INFO):
    raise RuntimeError("动作清单为空或缺 id（%s）" % ACTIONS_PATH)
ACTION_IDS = tuple(str(a["id"]) for a in ACTIONS_INFO)

PACK_INFO = {
    "id": PACK_ID,
    "name": "抠图 / 换背景（u2net 主体分割）",
    "version": PACK_VERSION,
    "api_version": API_VERSION,
    # 处理类包没有「规则」：唯一的动作就是「抠图并按背景模式出图」。
    "rules": [],
    # N6e 动作清单：宿主的动作链（`POST /api/jobs` 带 `actions[]`）按 `options.action`
    # 选动作，权威清单就是这里。本包只干一件事（抠图 + 白底），所以只声明一个动作；
    # 透明底 / 指定色底仍走参数面（`bgMode`），不编第二个动作名。
    "actions": [dict(action) for action in ACTIONS_INFO],
    "capabilities": [
        "io.read",
        "detect.u2net",
        "detect.white",     # 只用于守卫②「输入是否已是白底」（宿主 classic white_edge，确定性、不占 VL）
        "op.geometric",     # 只用于「裁主体块 / 让宿主把合成结果编码成 JPEG」
        "report.emit",
    ],
    # 同一张图同一组参数 → 同字节产物（N14「重跑 sha256 一致」）
    "deterministic": True,
    # 实测最慢一档 24MP ≈ 6s（README「性能」有原始数据）；300s 是给慢盘/低配机的余量
    "timeout_secs": 180,
    "outputs": ["artifact"],
}

BG_MODES = ("white", "transparent", "color")
DEFAULTS = {"bgMode": "white", "color": "#ffffff", "feather": 2, "edgeThreshold": 128,
            # N19 大图保护：长边上限（0 = 不缩放，按原尺寸出图）。
            # 为什么要有它：本包的像素活（裁块解码 / 合成 / PNG 编码 / 主体损失统计）
            # 都是 O(像素) 的纯 Python —— 24MP 一张就够慢，真机上再叠加慢盘/低配机
            # 就会撞宿主预算（N15 真机 6/10 超时）。给了上限就先把大图缩到上限再干活。
            "maxSide": 0}
MAX_SIDE_LIMIT = 8192

# --- 守卫阈值（实测标定见 README「守卫」；改这四个数必须先重跑语料门禁）------------
# 已白底判定：band=16px 的边框**带**（含 4px 门禁口径），宿主的 classic white_edge 实测。
# 用 16 而不是 4：4px 只能证明「最外一圈白」，白框/白边留白图/背景色块贴着边缘的情况
# 会漏判；16px 内环仍全白才敢说「这张图已经是白底」。
ALREADY_WHITE_BAND = 16
ALREADY_WHITE_MAX_DEV = 3.0      # 与门禁 `out_bg_dev_max` / 宿主 white_edge 同一口径
# 无背景色块：白框/白留边包裹彩色照片的图，16px 带全白但 48px 带均值会冲到 30~100
BG_BLOCK_BAND = 48
BG_BLOCK_MAX_MEAN_DEV = 16.0
# 掩码置信度：u2net 退化（整图算主体 / 只抓到噪点）时 fg_ratio 会落到这两个极端
MIN_FG_RATIO = 0.02
MAX_FG_RATIO = 0.95
# 主体损失红线：不透明核心被改动像素占比上限（只数 alpha==255；容差 8 躲编码抖动）
SUBJECT_CHANGE_TOL = 8
MAX_SUBJECT_CHANGE_RATIO = 0.05

_STATE_LOCK = threading.Lock()
_CANCEL = {}        # job_id -> threading.Event
_REPORTS = {}       # job_id -> [ {assetId, result, artifacts, evidence, fields} ]


def _stderr_fallback_path():
    """stderr 写不进去时的日志落点（同 STAGE_LOG 目录，便于一起取证）。"""
    stage = os.environ.get(STAGE_LOG_ENV)
    if stage:
        return stage + ".stderr.log"
    work = os.environ.get("DAMPACK_WORK_DIR") or os.getcwd()
    return os.path.join(work, "segment-stderr.log")


def log(*parts):
    """包内日志。**绝不抛异常** —— 真机踩过：stderr 句柄被 Windows 打坏之后，`log()` 在
    `Stages.mark()` 里抛出，异常一路冒到工作线程的 `except` 分支、那里再 `log()` 又抛一次，
    线程就这么无声消失（宿主拿不到应答，只能报 plugin_timeout，包内一条日志都没有）。
    日志是诊断设施，坏了只能降级（落到文件），不能把业务打断。
    """
    msg = "[segment] " + " ".join(str(p) for p in parts)
    try:
        sys.stderr.write(msg + "\n")
        sys.stderr.flush()
        return
    except Exception as exc:  # noqa: BLE001
        reason = "stderr 写失败：%r" % (exc,)
    try:
        with open(_stderr_fallback_path(), "a", encoding="utf-8") as fh:
            fh.write(msg + "    <<" + reason + ">>\n")
    except Exception:  # noqa: BLE001 —— 两个落点都坏：只能放弃，但绝不抛
        pass


# --- 分阶段耗时（N19） --------------------------------------------------------
# 为什么包内自己记：真机超时的时候宿主只回一句「超过 N ms」，用户/排障者不知道钱花在
# 哪一段（宿主解码? u2net? 纯 Python 合成? 落盘?）。这里每一段打一条 stderr（宿主会
# 留最近若干行，超时文案里能带上最后一段）+ 汇总进 evidence.stages（逐图可查）。
# 另外 `DAMPACK_STAGE_LOG=<path>` 时追加 JSONL：**进程被 kill 也留得下来**
# （超时是最需要证据的场景，而超时恰恰意味着应答不会回来）。
STAGE_LOG_ENV = "DAMPACK_STAGE_LOG"
STAGE_KEYS = ("ioRead", "whiteProbe", "preScale", "detectU2net", "crop",
              "decode", "composite", "subjectGuard", "encode", "hash")


class Stages:
    """一段一段记账：`mark(name)` = 记下「距上一次 mark」的毫秒。"""

    def __init__(self, asset_id=""):
        self.asset_id = asset_id
        self.started = time.perf_counter()
        self.last = self.started
        self.items = {}

    def mark(self, name):
        now = time.perf_counter()
        self.items[name] = int(round((now - self.last) * 1000))
        self.last = now
        log("stage", self.asset_id or "-", name, self.items[name], "ms")
        # 每条也落一行 JSONL：超时 = 进程会被 kill、应答不会回来 —— 只有「边跑边落」
        # 的日志能留下「卡在哪一段」这个关键事实（汇总行是拿不到的）。
        self._append({"event": "stage", "assetId": self.asset_id, "stage": name,
                      "ms": self.items[name], "totalMs": self.total_ms()})
        return self.items[name]

    def _append(self, row):
        path = os.environ.get(STAGE_LOG_ENV)
        if not path:
            return
        try:
            with open(path, "a", encoding="utf-8") as fh:
                fh.write(json.dumps(row, ensure_ascii=False) + "\n")
                fh.flush()
        except OSError as exc:      # 记不下来不许影响业务
            log("阶段日志写不进去（%s）：%s" % (path, exc))

    def total_ms(self):
        return int(round((time.perf_counter() - self.started) * 1000))

    def record(self, **extra):
        row = {
            "event": "result",
            "assetId": self.asset_id,
            "stages": dict(self.items),
            "totalMs": self.total_ms(),
        }
        row.update(extra)
        self._append(row)
        return row


def _prescale(path, workdir, tag, max_side, width, height):
    """长边 > `max_side` 时先让宿主缩一份工作副本（`/op/resize`，只缩不放）。

    为什么缩在**宿主**：包不解码原图（协议 §6），缩放要真像素；宿主 `core-media`
    一次就够，包内纯 Python 缩 24MP 只会更慢。
    产物中间件（`*-scaled.png`）用完即删。
    """
    if not max_side or max(width, height) <= max_side:
        return None, width, height
    out = os.path.join(workdir, tag + "-scaled.png")
    resized = gw("/op/resize", {"path": path, "out": out, "max_side": int(max_side)})
    return out, int(resized["width"]), int(resized["height"])



class RpcError(Exception):
    def __init__(self, code, message, data=None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.data = data


# --- 网关客户端（唯一能力出口） ----------------------------------------------
def _base_url():
    return (os.environ.get("DAMPACK_GATEWAY_URL") or "").rstrip("/")


# 单次网关调用的超时（秒）。默认 60s。
#
# 为什么不是「整套 judge 预算」（180s，manifest.timeout_secs）：环回网关的正常最坏耗时是
# `/detect/u2net` 4s / `/op/crop`+JPEG 编码 1s 这一档（12MP 实测），60s 已是量级余量。
# 把它设成「整套预算」的后果在卡 t_55ce222d 真机上演过：一次调用卡在 `getresponse` 里，
# 等满整套预算时宿主也刚好超时 → 只能报 `plugin_timeout`，产物静默丢失，连「是哪一步卡住」
# 都看不出来。短超时 + 一次性重试（单跳最坏 60s×2）比长超时 + 静默超时有用得多。
# manifest.timeout_secs=180 就是按这条算的：单跳最坏 120s + 其余分段 ~15s ≈ 135s < 180s。
GW_CALL_TIMEOUT_DEFAULT = 60.0


def _http_timeout():
    raw = os.environ.get("DAMPACK_GW_CALL_TIMEOUT_SECS")
    try:
        return max(5.0, float(raw)) if raw else GW_CALL_TIMEOUT_DEFAULT
    except ValueError:
        return GW_CALL_TIMEOUT_DEFAULT


def _retry_enabled():
    raw = os.environ.get("DAMPACK_GW_RETRY")
    return str(raw).strip().lower() not in ("0", "off", "false", "no")


# 不可重试端点：`/report/emit` 是**追加**语义（宿主把上报行存进 emits），重试会多记一条。
_NON_IDEMPOTENT = ("/report/emit",)


def gw(path, payload):
    """调网关端点；错误映射严格按协议 §5（不吞 403 / 不重试业务错误）。

    **唯一的重试面是「传输层没有任何应答」**（KI-29 真机形态：Windows 上应答写出后客户端
    读不到，`getresponse` 一直阻塞到超时）。这类失败与「网关说不」完全不同：没有 HTTP 状态、
    没有语义副作用，换一条新连接重试一次就是安全的；`/report/emit` 除外（追加语义）。
    """
    base = _base_url()
    if not base:
        raise RpcError(-32000, "DAMPACK_GATEWAY_URL 未注入（宿主必须提供）")
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        base + path, data=body, method="POST",
        headers={"content-type": "application/json; charset=utf-8",
                 "authorization": "Bearer " + (os.environ.get("DAMPACK_GATEWAY_TOKEN") or "")},
    )
    attempts = 2 if (_retry_enabled() and path not in _NON_IDEMPOTENT) else 1
    last = None
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(request, timeout=_http_timeout()) as resp:
                raw = resp.read().decode("utf-8")
                if attempt:
                    log("网关 %s 第 %d 次尝试成功（前一次传输失败：%r）"
                        % (path, attempt + 1, last))
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as exc:
            detail = ""
            try:
                detail = json.loads(exc.read().decode("utf-8")).get("error") or ""
            except Exception:  # noqa: BLE001
                detail = ""
            if exc.code == 403:
                raise RpcError(-32001, "能力未授权（403）%s" % detail)
            if exc.code in (404, 410):
                raise RpcError(-32002, "资产不可读（%d）%s" % (exc.code, detail))
            if exc.code == 400:
                raise RpcError(-32003, "参数非法（400）%s" % detail)
            if exc.code == 401:
                raise RpcError(-32000, "网关鉴权失败（401）%s" % detail)
            raise RpcError(-32000, "网关错误 %s %s" % (exc.code, detail))
        except (urllib.error.URLError, TimeoutError, OSError,
                http.client.HTTPException) as exc:
            # 传输层失败：连接被拒/被重置/读到半截/超时（URLError.reason 里包 socket.timeout）
            last = exc
            if attempt + 1 >= attempts:
                raise RpcError(-32000, "网关调用失败（%s，共 %d 次）：%r"
                               % (path, attempt + 1, exc))
            log("网关 %s 传输失败（%r），换新连接重试一次" % (path, exc))
    raise RpcError(-32000, "网关调用失败（%s）：%r" % (path, last))



# --- 取消语义 ----------------------------------------------------------------
def _cancel_flag(job_id):
    with _STATE_LOCK:
        flag = _CANCEL.get(job_id)
        if flag is None:
            flag = threading.Event()
            _CANCEL[job_id] = flag
        return flag


def _check_cancel(job_id, stage=""):
    flag = _cancel_flag(job_id)
    if flag.is_set():
        raise RpcError(-32004, "任务已取消%s" % (("（" + stage + "）") if stage else ""))


# --- 参数 --------------------------------------------------------------------
def _safe(name, fallback="asset"):
    cleaned = _SAFE.sub("_", str(name or "")).strip("_")
    return (cleaned or fallback)[:64]


def _parse_color(raw):
    text = str(raw or "").strip().lstrip("#")
    if len(text) != 6 or any(c not in "0123456789abcdefABCDEF" for c in text):
        raise RpcError(-32003, "color 必须是 RRGGBB 六位十六进制（实得 %r）" % (raw,))
    return (int(text[0:2], 16), int(text[2:4], 16), int(text[4:6], 16)), text.lower()


def normalize_options(options):
    """宿主发的是**全量生效值**；这里再校一次（协议 §9：防绕过与旧客户端）。

    `action` 只从**动作链**进来（宿主 `action_asset` 在调用时补 `options.action`）；
    普通规格没有这个键，走了就等于用户/链明确点了动作名 —— 因此要和 `bgMode` 对齐。
    """
    opts = dict(DEFAULTS)
    if isinstance(options, dict):
        for key, value in options.items():
            if key in DEFAULTS or key in ("workdir",):
                opts[key] = value
            elif key == "action":
                # 动作名由本包自己的清单把关（不在 params.schema 里，见文件头 ACTIONS_INFO）
                opts["action"] = value
            elif key in ("job_id",):
                continue
            else:
                raise RpcError(-32003, "未知参数 %r（schema 只声明 %s）"
                               % (key, sorted(DEFAULTS)))
    mode = opts.get("bgMode")
    if mode not in BG_MODES:
        raise RpcError(-32003, "bgMode 只接受 %s（实得 %r）" % (list(BG_MODES), mode))
    action = opts.get("action")
    if action is not None:
        if action not in ACTION_IDS:
            raise RpcError(-32003, "未知动作 %r（本包声明的动作：%s）"
                           % (action, "/".join(ACTION_IDS)))
        # `white_bg_fill` 就是「bgMode=white」的可读名字。两者冲突时必须响亮失败：
        # 静默按其中一个执行，调用方以为跑的是另一件事（链的下一步会跟着错下去）。
        if mode != "white":
            raise RpcError(-32003, "动作 %r 与参数 bgMode=%r 冲突：白底动作只能配 bgMode=white"
                           % (action, mode))

    try:
        feather = int(opts.get("feather"))
        threshold = int(opts.get("edgeThreshold"))
        max_side = int(opts.get("maxSide") or 0)
    except (TypeError, ValueError):
        raise RpcError(-32003, "feather/edgeThreshold/maxSide 必须是整数")
    if not (0 <= feather <= 8):
        raise RpcError(-32003, "feather 须在 0..8（实得 %r）" % (feather,))
    if not (0 <= threshold <= 255):
        raise RpcError(-32003, "edgeThreshold 须在 0..255（实得 %r）" % (threshold,))
    if not (0 <= max_side <= MAX_SIDE_LIMIT):
        raise RpcError(-32003, "maxSide 须在 0..%d（0 = 不缩放；实得 %r）"
                       % (MAX_SIDE_LIMIT, max_side))
    bg, color_hex = _parse_color(opts.get("color") or "#ffffff")
    return {
        "bgMode": mode,
        "feather": feather,
        "edgeThreshold": threshold,
        "bg": bg,
        "color": color_hex,
        "maxSide": max_side,
        "action": action,
        "workdir": opts.get("workdir") or os.environ.get("DAMPACK_WORK_DIR") or "",
    }


def _workdir(opts):
    raw = opts.get("workdir")
    if not raw:
        raise RpcError(-32003, "缺少 workdir（宿主注入 options.workdir）")
    os.makedirs(raw, exist_ok=True)
    return raw


# --- 守卫②：输入已经是白底吗 -------------------------------------------------
class _GuardReject(Exception):
    """守卫命中 → 直接回这一行（中间件照常由 finally 清掉，产物一个不留）。"""

    def __init__(self, row):
        super().__init__(row.get("result"))
        self.row = row


def _target_is_white(opts):
    """本次要的底色是不是纯白（`white` 或 `color=#ffffff`）。只有这种情况「已是白底」
    才等于「什么都不用做」；`transparent` / 指定色底即使输入是白底也必须重做。"""
    if opts["bgMode"] == "white":
        return True
    return opts["bgMode"] == "color" and tuple(opts["bg"]) == (255, 255, 255)


def _white_stats(path, band):
    """宿主像素统计（`/detect/white_stats`，能力 `detect.white`，classic `white_edge` 同一份实现）。

    宿主管不到（老网关没这个端点 / 部署方没授权）→ 回 None：**量不到不等于已是白底**，
    守卫退化成「照常处理」，并把 `unavailable` 落 evidence（不假装测过）。
    """
    try:
        return gw("/detect/white_stats", {"path": path, "band": band,
                                          "max_dev": ALREADY_WHITE_MAX_DEV})
    except RpcError as exc:
        if exc.code == -32001 or "no_such_endpoint" in (exc.message or ""):
            log("white_stats 不可用（%s）→ 跳过「已白底」判定，照常处理" % (exc.message,))
            return None
        raise


def _already_white_probe(path):
    """守卫② 完整判定 → `(是否已白底, 证据)`。两条都要过：

    1. **边框带全白**：band=16px 的 worst_dev ≤ 3（⊇ 门禁 4px 口径，比门禁更严）；
    2. **无背景色块**：band=48px 的 **均值**偏差 ≤ 16 —— 白框/白留边包裹彩色照片的图，
       16px 带全白但 48px 带均值会冲上 30~100（实测：白框 20px=105.1、40px=28.9；
       白底商品图 0.0~2.2），靠这条把它们挡住。

    任一条量不到（宿主没能力）→ 不跳过（量不到就照常处理，绝不猜）。
    """
    ev = {
        "inputWhiteProbe": "unavailable",
        "inputBorderDev": None, "inputBorderBandPx": ALREADY_WHITE_BAND,
        "allowedBorderDev": ALREADY_WHITE_MAX_DEV,
        "bgBlockMeanDev": None, "bgBlockBandPx": BG_BLOCK_BAND,
        "allowedBgBlockMeanDev": BG_BLOCK_MAX_MEAN_DEV,
    }
    near = _white_stats(path, ALREADY_WHITE_BAND)
    if near is None:
        return False, ev
    try:
        worst = float(near.get("worst_dev"))
    except (TypeError, ValueError):
        return False, ev
    ev.update({"inputWhiteProbe": "measured", "inputBorderDev": worst,
               "inputBorderMeanDev": float(near.get("mean_dev") or 0.0),
               "inputBorderBandPx": int(near.get("band") or ALREADY_WHITE_BAND),
               "edgePixels": near.get("edge_pixels")})
    if worst > ALREADY_WHITE_MAX_DEV:
        return False, ev
    wide = _white_stats(path, BG_BLOCK_BAND)
    if wide is None:
        ev["inputWhiteProbe"] = "partial_no_bg_block_probe"
        return False, ev
    try:
        block_mean = float(wide.get("mean_dev"))
    except (TypeError, ValueError):
        return False, ev
    ev.update({"inputWhiteProbe": "measured",
               "bgBlockMeanDev": block_mean,
               "bgBlockWorstDev": float(wide.get("worst_dev") or 0.0),
               "bgBlockBandPx": int(wide.get("band") or BG_BLOCK_BAND)})
    if block_mean > BG_BLOCK_MAX_MEAN_DEV:
        log("边框是白的，但 %dpx 带均值为白偏差 %.1f > %.1f（背景有色块）→ 照常处理"
            % (BG_BLOCK_BAND, block_mean, BG_BLOCK_MAX_MEAN_DEV))
        return False, ev
    return True, ev


def _already_white_row(asset_id, opts, width, height, probe_ev, started, stages=None):
    """`skipReason=already_white` + **没有 artifacts**（不生成新工作版本）。

    `result` 分两种口径（判据一样，只是宿主对「没产物的步」的读法不同）：

    * **普通规格调用**（没有 `options.action`）→ `ok`：这一行跑完了且没有失败 ——
      按用户意图，原图已经是交付形态；`skipReason` 是给人和后续节点看的机读原因，
      不是结论（处理类没有 verdict）。N14 门禁按这个口径断言（`result=ok` + 零产物）。
    * **动作链调用**（链内每步都带 `options.action`）→ `skipped`：链的契约是
      「没产物的步必须标 skipped 才算通过」（`compliance_chain.rs` 模块头第 3 条：
      `ok` 却没产物 = `action_no_product` 整链失败）。所以链内第二次施加到已白底产物上
      必须报 skipped，让链沿用上一步的图继续，而不是把「本来就不该动」判成失败。
    """
    in_chain = bool(opts.get("action"))
    evidence = {
        "bgMode": opts["bgMode"],
        "bgColor": "#" + opts["color"],
        "width": width, "height": height,
        "guard": "already_white",
        "skippedCalls": ["detect.u2net", "op.crop", "op.crop(encode)"],
        "elapsedMs": int((time.time() - started) * 1000),
    }
    if in_chain:
        evidence["action"] = opts["action"]
        evidence["chainStep"] = "skipped_no_product"
    if stages is not None:
        row = stages.record(width=width, height=height, skipped="already_white")
        evidence["stages"] = row["stages"]
    evidence.update(probe_ev)
    return {
        "assetId": asset_id,
        "result": "skipped" if in_chain else "ok",
        "skipReason": "already_white",
        "evidence": evidence,
        "fields": {
            "背景": "已是纯白（跳过）",
            "输出": "原图即交付形态（未生成新版本）",
        },
    }


# --- 守卫①：掩码可信吗 -------------------------------------------------------
def _mask_low_confidence(fg_ratio):
    return fg_ratio < MIN_FG_RATIO or fg_ratio > MAX_FG_RATIO


def _mask_low_confidence_row(asset_id, opts, width, height, fg_ratio, seg, started):
    return {
        "assetId": asset_id,
        "result": "skipped",
        "skipReason": "mask_low_confidence",
        "evidence": {
            "bgMode": opts["bgMode"],
            "width": width, "height": height,
            "fgRatio": fg_ratio,
            "fgRatioRange": [MIN_FG_RATIO, MAX_FG_RATIO],
            "bbox": seg.get("bbox"),
            "instances": seg.get("instances"),
            "guard": "mask_low_confidence",
            "elapsedMs": int((time.time() - started) * 1000),
        },
        "fields": {
            "背景": "未替换（掩码不可信）",
            "输出": "原图不动",
        },
    }


def _subject_loss_row(asset_id, opts, width, height, subject_px, changed_px, started):
    ratio = (changed_px / float(subject_px)) if subject_px else 0.0
    return {
        "assetId": asset_id,
        "result": "failed",
        "error": {
            "code": "subject_loss",
            "message": ("主体不透明核心被改动 %.2f%%（上限 %.0f%%；%d/%d 像素）——拒绝落产物"
                        % (ratio * 100.0, MAX_SUBJECT_CHANGE_RATIO * 100.0,
                           changed_px, subject_px)),
        },
        "evidence": {
            "bgMode": opts["bgMode"],
            "width": width, "height": height,
            "subjectOpaquePixels": subject_px,
            "subjectChangedPixels": changed_px,
            "subjectChangedRatio": round(ratio, 4),
            "subjectChangedMaxRatio": MAX_SUBJECT_CHANGE_RATIO,
            "guard": "subject_loss",
            "elapsedMs": int((time.time() - started) * 1000),
        },
        "fields": {
            "背景": "未替换（主体会被吃掉）",
            "输出": "无产物",
        },
    }


# --- 抠图主流程 --------------------------------------------------------------
def _artifact_path(workdir, asset_id, mode):
    ext = "png" if mode == "transparent" else "jpg"
    return os.path.join(workdir, "%s-segment-%s.%s" % (_safe(asset_id), mode, ext))


def _clip_box(bbox, width, height, margin):
    x1, y1, x2, y2 = [int(round(float(v))) for v in bbox]
    x1 = max(0, min(width - 1, x1 - margin))
    y1 = max(0, min(height - 1, y1 - margin))
    x2 = max(x1 + 1, min(width, x2 + margin))
    y2 = max(y1 + 1, min(height, y2 + margin))
    return [x1, y1, x2, y2]


def _remove_quietly(*paths):
    for path in paths:
        try:
            os.remove(path)
        except OSError:
            pass


def segment_asset(job_id, asset, opts):
    """一个资产 → 一行结果（`outputs=["artifact"]`：只有 result/artifacts/evidence）。"""
    asset = asset or {}
    path = asset.get("path")
    if not isinstance(path, str) or not path.strip():
        raise RpcError(-32003, "asset.path 必填")
    asset_id = asset.get("id") or os.path.basename(path)
    workdir = _workdir(opts)
    started = time.time()
    stages = Stages(asset_id)

    info = gw("/io/read", {"path": path})
    width, height = int(info["width"]), int(info["height"])
    source_w, source_h = width, height
    stages.mark("ioRead")
    _check_cancel(job_id, "io.read")

    # --- 守卫②（最便宜、也最该先问）：要白底，可这张图本来就是白底？ -----------
    # 放在 /detect/u2net 之前：命中时连模型都不用跑（省一次 12MP 推理 + 三次裁块）。
    white_ev = {"inputWhiteProbe": "not_applicable"}
    if _target_is_white(opts):
        already_white, white_ev = _already_white_probe(path)
        stages.mark("whiteProbe")
        if already_white:
            return _already_white_row(asset_id, opts, width, height, white_ev, started,
                                      stages=stages)
    else:
        stages.mark("whiteProbe")

    # --- 大图保护（N19）：长边超上限就先缩一份工作副本 -------------------------
    tag = "%s-%s" % (_safe(asset_id), hashlib.sha256(path.encode("utf-8")).hexdigest()[:8])
    work_path, out_canvas_w, out_canvas_h = _prescale(
        path, workdir, tag, opts["maxSide"], width, height)
    pre_scaled = work_path is not None
    if pre_scaled:
        width, height = out_canvas_w, out_canvas_h
        log("大图预缩放 %dx%d → %dx%d（maxSide=%d）"
            % (source_w, source_h, width, height, opts["maxSide"]))
    stages.mark("preScale")

    seg = gw("/detect/u2net", {"path": work_path or path})
    stages.mark("detectU2net")
    fg_ratio = float(seg.get("fg_ratio") or 0.0)
    bbox = seg.get("bbox")
    mask_path = seg.get("mask_path")
    _check_cancel(job_id, "detect.u2net")

    k = composite.cell_size(width, height)
    out_path = _artifact_path(workdir, asset_id, opts["bgMode"])
    if fg_ratio <= 0 or not bbox or not mask_path:
        # 没有主体 = 不该输出「一张全空的图」：如实 skipped，让用户看见而不是拿到假产物
        return {
            "assetId": asset_id,
            "result": "skipped",
            "skipReason": "no_subject",
            "evidence": {"fg_ratio": fg_ratio, "width": width, "height": height,
                         "bgMode": opts["bgMode"]},
        }
    # --- 守卫①：掩码退化（整图算主体 / 只抓到噪点）→ 不拿错掩码刷图 -------------
    if _mask_low_confidence(fg_ratio):
        return _mask_low_confidence_row(asset_id, opts, width, height, fg_ratio, seg, started)

    k = composite.cell_size(width, height)
    margin = k + 2 * opts["feather"] + 2
    box = _clip_box(bbox, width, height, margin)
    cw, ch = box[2] - box[0], box[3] - box[1]

    src_png = os.path.join(workdir, tag + "-src.png")
    msk_png = os.path.join(workdir, tag + "-mask.png")
    composed_png = os.path.join(workdir, tag + "-composed.png")
    try:
        gw("/op/crop", {"path": work_path or path, "box": box, "out": src_png})
        gw("/op/crop", {"path": mask_path, "box": box, "out": msk_png})
        stages.mark("crop")
        _check_cancel(job_id, "crop")

        sw, sh, sch, src = pngio.decode_path(src_png)
        mw, mh, mch, mraw = pngio.decode_path(msk_png)
        stages.mark("decode")
        if (sw, sh) != (cw, ch) or (mw, mh) != (cw, ch):
            raise RpcError(-32000, "宿主裁块尺寸不一致：src=%dx%d mask=%dx%d 期望 %dx%d"
                           % (sw, sh, mw, mh, cw, ch))
        if sch != 3:
            raise RpcError(-32000, "宿主裁块通道数 %r（期望 RGB）" % (sch,))
        mask_plane = bytes(mraw[0::mch]) if mch > 1 else bytes(mraw)
        binary = composite.binarize(mask_plane, opts["edgeThreshold"])
        alpha = composite.build_alpha(binary, cw, ch, opts["feather"])
        stages.mark("composite")
        _check_cancel(job_id, "alpha")

        # 产物必须是**整图尺寸**（资产工作版本 = 原图换个底，不是一张裁块）。
        # 裁块只用来省解码：合成完再贴回原坐标，框外一律按背景色/全透明处理。
        subject_px = changed_px = 0
        if opts["bgMode"] == "transparent":
            rgba = composite.blend_rgba(src, cw, ch, alpha)
            canvas = bytearray(width * height * 4)      # 全零 = 全透明
            for y in range(ch):
                s = y * cw * 4
                dst = ((box[1] + y) * width + box[0]) * 4
                canvas[dst:dst + cw * 4] = rgba[s:s + cw * 4]
            pngio.write_png(out_path, width, height, 4, canvas)
            out_w, out_h, out_format = width, height, "png"
        else:
            patch = composite.blend_rgb(src, cw, ch, alpha, opts["bg"])
            # --- 守卫③：主体区域被动过了吗（坐标错位/合成回归的靶子） -------------
            subject_px, changed_px = composite.subject_change(
                src, patch, alpha, cw, ch, SUBJECT_CHANGE_TOL)
            stages.mark("subjectGuard")
            if subject_px and changed_px / float(subject_px) > MAX_SUBJECT_CHANGE_RATIO:
                raise _GuardReject(
                    _subject_loss_row(asset_id, opts, width, height,
                                      subject_px, changed_px, started))
            rgb = bytearray(bytes(opts["bg"]) * (width * height))
            for y in range(ch):
                dst = ((box[1] + y) * width + box[0]) * 3
                s = y * cw * 3
                rgb[dst:dst + cw * 3] = patch[s:s + cw * 3]
            pngio.write_png(composed_png, width, height, 3, rgb)
            # 借宿主 /op/crop（全框）把合成结果编码成 JPEG：质量口径归宿主，包内不写编码器
            encoded = gw("/op/crop", {"path": composed_png, "box": [0, 0, width, height],
                                      "out": out_path})
            out_w, out_h, out_format = int(encoded["width"]), int(encoded["height"]), "jpg"
        stages.mark("encode")
        _check_cancel(job_id, "encode")
    except _GuardReject as reject:
        _remove_quietly(out_path)          # 守卫命中：绝不留下半成品
        return reject.row
    finally:
        # 中间件不留：一个资产一次一个产物（工作版本目录里不许出现平行资产树）
        if opts["bgMode"] != "transparent":
            _remove_quietly(composed_png)
        _remove_quietly(src_png, msk_png)
        if pre_scaled and work_path:
            _remove_quietly(work_path)

    with open(out_path, "rb") as fh:
        digest = hashlib.sha256(fh.read()).hexdigest()
    stages.mark("hash")
    nonempty = sum(1 for v in alpha if v)
    partial = sum(1 for v in alpha if 0 < v < 255)
    opaque = sum(1 for v in alpha if v == 255)
    stage_row = stages.record(
        sourceWidth=source_w, sourceHeight=source_h,
        width=width, height=height, cropArea=cw * ch, preScaled=pre_scaled)
    return {
        "assetId": asset_id,
        "result": "ok",
        "artifacts": [{"kind": "export_file", "path": out_path, "sha256": digest}],
        "evidence": {
            "bgMode": opts["bgMode"],
            "bgColor": "#" + opts["color"],
            "feather": opts["feather"],
            "edgeThreshold": opts["edgeThreshold"],
            "maxSide": opts["maxSide"],
            # 原图尺寸 vs 工作尺寸：预缩放开了就不一样，两个都要可见（否则「产物为什么变小了」无从解释）
            "sourceWidth": source_w, "sourceHeight": source_h,
            "preScaled": pre_scaled,
            "width": width, "height": height,
            "cropBox": box,
            "outWidth": out_w, "outHeight": out_h,
            "format": out_format,
            "fgRatio": fg_ratio,
            "maskCellPx": k,
            "alphaRampPx": composite.alpha_ramp_px(binary, cw, ch, opts["feather"]),
            "subjectPixels": nonempty,
            "opaquePixels": opaque,
            "cropArea": cw * ch,
            "featherPixels": partial,
            # 守卫② 的实测值照原样记账（不可用/未测也要看得见，不假装测过）
            **white_ev,
            # 守卫③ 的口径值（不透明核心 = 必须原样照搬的那部分）
            "subjectOpaquePixels": subject_px,
            "subjectChangedPixels": changed_px,
            "subjectChangedMaxPixels": int(subject_px * MAX_SUBJECT_CHANGE_RATIO),
            "sha256": digest,
            # N19：逐段耗时（排障/容量评估用；超时的那张图也能从这里看出钱花在哪）
            "stages": stage_row["stages"],
            "elapsedMs": int((time.time() - started) * 1000),
        },
        "fields": {
            # 产物的**机读去处**（协议 §4.4 `fields`）：宿主动作链的链式衔接就是靠这几个键
            # （`compliance_chain::ChainStep::from_outcome` 读 `outPath/sha256/out/format/bytes`，
            # 缺 `outPath` 会把「成功」判成 `action_no_product`）。中文列是给人看的，两套并存。
            "outPath": out_path,
            "sha256": digest,
            "out": [out_w, out_h],
            "format": out_format,
            "bytes": os.path.getsize(out_path) if os.path.isfile(out_path) else None,
            "背景": {"white": "纯白", "transparent": "透明", "color": "指定色"}[opts["bgMode"]],
            "输出": "%dx%d %s" % (out_w, out_h, out_format),
        },
    }


# --- 方法实现 ----------------------------------------------------------------
def _rows_reply(rows):
    return {"rows": rows}


def handle_process(job_id, asset, options, rules=None):
    """`judge` 与 `process` 同一个实现：处理类包的宿主入口是 `judge`（协议 v1.1），
    `process` 是同一动作的显式名字（向前兼容，不改变语义）。"""
    del rules          # 处理类没有规则选择面；宿主传什么都不影响
    opts = normalize_options(options)
    row = segment_asset(job_id, asset, opts)
    with _STATE_LOCK:
        _REPORTS.setdefault(job_id, []).append(row)
    if row.get("result") == "ok" and row.get("artifacts"):
        try:
            gw("/report/emit", {"job_id": job_id, "assetId": row["assetId"],
                                "kind": "process", "artifacts": row["artifacts"]})
        except RpcError as exc:
            # 上报失败不影响产物（产物已在磁盘），但要留痕
            log("report/emit 失败：%s" % (exc.message,))
    return _rows_reply([row])


def _require_job_id(params):
    job_id = params.get("job_id") or (params.get("params") or {}).get("job_id")
    if not isinstance(job_id, str) or not job_id.strip():
        raise RpcError(-32003, "job_id 必填")
    return job_id


def m_judge(params):
    params = params or {}
    return handle_process(_require_job_id(params), params.get("asset"),
                          params.get("options"), params.get("rules"))


def m_report(params):
    job_id = str((params or {}).get("job_id") or "")
    with _STATE_LOCK:
        rows = list(_REPORTS.get(job_id, []))
    return {"job_id": job_id, "assets": rows, "rows": len(rows)}


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
    # 握手要带 api_version：宿主拿它与 manifest 严格比对（协议不兼容就拒用，不猜）
    return {"status": "ok", "pid": os.getpid(), "version": PACK_VERSION,
            "api_version": API_VERSION}


HANDLERS = {
    "pack.info": m_info,
    "health": m_health,
    "run": m_judge,
    "report": m_report,
    "cancel": m_cancel,
}


# --- stdio 协议壳（协议 §9：三条必守） ---------------------------------------
_ORIG_STDOUT = None      # main() 里保存被换下来的 stdout 对象（见 main() 的 Windows 铁律）


_ERR_PARSE = -32700
_ERR_METHOD = -32601
_ERR_INTERNAL = -32603


def _reply_fd(stdout):
    """协议流的 fd（文本层写失败时用 fd 直写兜底）。取不到/不可用返回 -1。"""
    try:
        fd = stdout.fileno()
        os.fstat(fd)          # 顺手探活：坏 fd 现在就暴露，别等写的时候
        return fd
    except Exception:         # noqa: BLE001 —— 拿不到就走文本层，不是错误
        return -1


def _write_all(fd, data):
    """整块写进 fd：短写要续写，0 字节 = 对端已关，必须显式报错（不许静默丢应答）。"""
    if fd < 0:
        raise OSError("协议流没有可用 fd")
    while data:
        n = os.write(fd, data)
        if n <= 0:
            raise OSError("协议流写 0 字节（对端已关闭）")
        data = data[n:]


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
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    write_lock = threading.Lock()
    # 应答写失败必须留证：真机（Windows）上出现过「插件已跑完但宿主收不到应答」的形态，
    # 若这里静默吞掉，宿主只会报 plugin_timeout、包内一条日志都没有（卡 t_55ce222d）。
    reply_fd = _reply_fd(stdout)

    def reply(frame):
        line = json.dumps(frame, ensure_ascii=False) + "\n"
        with write_lock:
            try:
                _write_all(reply_fd, line.encode("utf-8"))
                return
            except OSError as exc:
                log("应答写出失败（fd 直写）：%r" % (exc,))
            try:
                # 退回文本层再试一次（两路都失败才真的是链路断了 —— 那时只能留证）
                stdout.write(line)
                stdout.flush()
            except Exception as exc:  # noqa: BLE001
                log("应答写出失败（文本层）：%r" % (exc,))

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
            log("内部错误：%r" % (exc,))
            reply({"jsonrpc": "2.0", "id": req_id,
                   "error": {"code": _ERR_INTERNAL, "message": "内部错误：%s" % (exc,)}})
        except BaseException as exc:  # noqa: BLE001
            # BaseException（SystemExit/KeyboardInterrupt）不会进上面的分支，而
            # threading.excepthook 对 SystemExit 是**静默**的 —— 线程会无声消失、宿主干等。
            # 工作线程里绝不允许出现「没有应答就结束」。
            log("工作线程被 BaseException 中止：%r" % (exc,))
            reply({"jsonrpc": "2.0", "id": req_id,
                   "error": {"code": _ERR_INTERNAL, "message": "线程中止：%r" % (exc,)}})

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
    # Windows 铁律：`dup2(2,1)` 之后**不许**让被换下来的 stdout 对象被 GC —— 它的析构会
    # `close(1)`，而 CRT 里 1 号与 2 号 fd 共用同一个 OS 句柄，一关就把 stderr 一起打断。
    # 后果正是真机那个形态：包内日志从某一刻起全部静默（宿主 stderr 读到 EOF），排查时
    # 看起来像「插件卡死在第一步之前」。Linux 上 dup 出来的 fd 相互独立，踩不到这个坑。
    global _ORIG_STDOUT
    _ORIG_STDOUT = sys.stdout
    sys.stdout = sys.stderr
    try:
        serve(sys.stdin, proto_out)
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
