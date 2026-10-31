#!/usr/bin/env python3
"""imaging 图片处理插件 —— 插件入口（N6d `task` 装配）。

一个包、六个动作（`resize` / `crop` / `convert` / `compress` / `watermark` / `rename`），
三组算法分别在 `geometry.py`（N6a）/ `codec.py`（N6b）/ `overlay.py`（N6c）。
本文件**只做装配**，不写任何算法：

1. **协议壳**：stdio 行式 JSON-RPC，收行 → 派发 → 回帧；stdout 只走协议行，日志一律 stderr。
   六个方法 `pack.info` / `health` / `judge` / `repair` / `report` / `cancel` 的分工见下。
2. **动作分派**：`judge` 的 `options.action`（schema 里的 `action` 枚举）选中六个动作之一，
   调对应组的 `handler(asset, options, gw, work_dir) -> row`。**一次调用一个动作**——
   多动作串联（先裁剪再压到 200KB）由宿主按规格的动作链编排，插件不做链式循环
   （`plugin-design-and-ui.md` §8.1：插件之间/动作之间不互相调用）。
3. **能力出口**：一切重活过宿主网关（`gw.Gateway`，端点见协议 §6），插件零第三方依赖、
   自己不解码大图、不出网。

`kinds = ["process"]`（处理类）的三条硬口径（协议 §4.4 / N5a `task`）：
    - 结果行**绝不带 `verdict`**（带了宿主整批拒收 `row_verdict_not_allowed`）；
    - 行的 `result` 只取 `ok`/`failed`/`skipped`，`failed` 必带 `error{code,message}`；
    - 产物只落宿主给的 `workdir`（`options.workdir`，宿主保留键），产物登记为
      `export_file{path,sha256}` 由宿主建工作版本；`rename` 不出产物（只回 `fields.assetName`）。

`repair` 对处理类包**不实现**：修复语义属于判定类（把不合格的图修好，`fix_policy` 见
`damdesk-product/references/plugin-design-and-ui.md` §3），处理类包拿到「修」的语义只会
退化成「再跑一遍某个动作」。所以回 `-32601` 并附可读原因 —— 不假装实现，也不静默成功。
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import threading
import time

API_VERSION = "1.1"
PACK_VERSION = "1.0.0"
PACK_ID = "com.soujpg.pack.imaging"
PACK_NAME = "图片处理（缩放 / 裁剪 / 转格式 / 压缩 / 水印 / 改名）"

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import codec             # noqa: E402  (N6b 编码组)
import geometry          # noqa: E402  (N6a 几何组)
import overlay           # noqa: E402  (N6c 叠加组)
from gw import Gateway   # noqa: E402  (共用网关客户端，三组同一份)

# 能力清单 = 六个动作实际用到的端点能力（协议 §6），一个不多一个不少：
#   io.read        —— 所有动作先读尺寸/字节/sha256（/io/read）
#   detect.u2net   —— crop 要主体框（/detect/bbox）
#   op.geometric   —— crop/letterbox/upscale(scale<=1)/encode（几何 + 编码）
#   op.overlay     —— watermark（/op/watermark）
CAPABILITIES = ["io.read", "detect.u2net", "op.geometric", "op.overlay"]
DETERMINISTIC = True     # 六组算法全是确定性算子（无 VL、无生成式），同输入同输出
TIMEOUT_SECS = 120       # 单图单动作预算；compress 的目标体积二分最费时
PARAMS_PATH = "resources/params.schema.json"

# ---------------------------------------------------------------- 动作注册表
# 一个动作 = 所属组的 handler + 展示信息。**顺序即 UI 顺序**（也是 `actions[]` 的顺序）：
# 先几何、再编码、最后叠加 —— 与用户「先把图裁对，再压小，最后加水印」的心智一致。
ACTION_SPECS = [
    ("resize", "缩放（默认只缩不放）", "geometry",
     "按长边/短边/固定宽高/等比系数缩放；默认不放大"),
    ("crop", "裁剪（主体不切）", "geometry",
     "按目标比例 + 锚点裁剪；裁不下就补边，主体框完整落在产物内"),
    ("convert", "转格式", "codec",
     "jpg/png/webp/avif 互转；透明通道按目标格式保留或按背景色合成；EXIF 方向摆正"),
    ("compress", "压缩", "codec",
     "质量档或目标体积（二分质量 + 必要时降采样）；达不到体积就响亮失败，不糊图凑数"),
    ("watermark", "加水印", "overlay",
     "文字水印：五锚点 + 透明度 + 字号 + 颜色 + 边距；尺寸不变，宿主侧渲染"),
    ("rename", "改名（不出新文件）", "overlay",
     "模板改名（{name}{ext}{width}{height}{date}{index}{assetId}）；只回名字，不改字节"),
]

# 动作名 → 组模块；`rename` 需要批内登记表（尾号 `-2`/`-3` 由它发号）
_MODULES = {"geometry": geometry, "codec": codec, "overlay": overlay}
_HANDLERS = {}
for _name, _title, _group, _summary in ACTION_SPECS:
    _fn = _MODULES[_group].ACTIONS.get(_name)
    if _fn is None:                      # 装配期就炸：缺动作比运行时 404 好查一万倍
        raise RuntimeError("组 %s 未导出动作 %s" % (_group, _name))
    _HANDLERS[_name] = (_group, _fn)

ACTION_NAMES = [spec[0] for spec in ACTION_SPECS]

# `actions[]`：给宿主/界面看的动作清单（纯增字段，宿主忽略亦不影响调用）。
# `group` 对应 params.schema 的分组键，界面据此只显示该动作相关的控件。
ACTIONS_INFO = [
    {"id": name, "name": title, "group": group, "summary": summary,
     "kinds": ["process"], "paramsGroup": group}
    for name, title, group, summary in ACTION_SPECS
]

# `pack.info.rules`：协议 §3.1 要求非空规则清单。处理类包没有判定规则，这里如实
# 用「动作清单」充当可执行清单（id = 动作名、deterministic 恒 true），不编造规则名。
RULES = [{"id": name, "name": title, "deterministic": True}
         for name, title, _group, _summary in ACTION_SPECS]

PACK_INFO = {
    "id": PACK_ID,
    "name": PACK_NAME,
    "version": PACK_VERSION,
    "api_version": API_VERSION,
    "rules": [dict(rule) for rule in RULES],
    "capabilities": list(CAPABILITIES),
    "deterministic": DETERMINISTIC,
    "timeout_secs": TIMEOUT_SECS,
    "kinds": ["process"],            # 与 manifest 一致（宿主以 manifest 为准，这里只是自述）
    "actions": [dict(action) for action in ACTIONS_INFO],
    "params": PARAMS_PATH,
}


def log(*parts):
    """日志只走 stderr：stdout 是协议通道，一行都不能脏（协议 §2 / conformance 02）。"""
    sys.stderr.write("[imaging] " + " ".join(str(p) for p in parts) + "\n")
    sys.stderr.flush()


# 网关工厂（测试注入桩网关的接缝；生产路径恒为 `Gateway` 按环境变量构造）
GATEWAY_FACTORY = Gateway.from_env


class RpcError(Exception):
    """协议 §5 闭集内的错误（不在这张表里的 code 一律视为违例）。"""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = int(code)
        self.message = str(message)


# --- 运行时状态（进程内；按 job 隔离） ---------------------------------------
_STATE_LOCK = threading.Lock()
_CANCEL = {}      # job_id -> threading.Event
_REPORTS = {}     # job_id -> [entry]
_JOB_SEEN = set()  # 已经见过开头的 job（用于 rename 登记表按 job 重置）


def _cancel_flag(job_id):
    with _STATE_LOCK:
        return _CANCEL.setdefault(str(job_id or ""), threading.Event())


def _begin_job(job_id):
    """每个 job 的第一次调用：reset 改名登记表。

    `overlay.RENAME_REGISTRY` 的生命周期口径是「一个 job」（尾号 `-2`/`-3` 按批内调用序发号）。
    宿主 launcher 通常每 job 拉一个进程，但**同一进程复用多个 job 时**必须在这里重置，
    否则尾号会跨 job 累加（N6c 移交第 4 条）。
    """
    key = str(job_id or "")
    with _STATE_LOCK:
        first = key not in _JOB_SEEN
        _JOB_SEEN.add(key)
        if first:
            overlay.RENAME_REGISTRY.reset()
            _REPORTS.setdefault(key, [])
    return first


def _record(job_id, row, action):
    """一条结果行 → job 报告账本（`report` 方法读它；宿主另有 /report/emit 通道，此处不重复发）。"""
    with _STATE_LOCK:
        _REPORTS.setdefault(str(job_id or ""), []).append({
            "assetId": row.get("assetId"),
            "action": action,
            "result": row.get("result"),
            "error": row.get("error"),
            "skipReason": row.get("skipReason"),
            "outPath": (row.get("fields") or {}).get("outPath"),
            "at": int(time.time() * 1000),
        })


# ---------------------------------------------------------------- 方法实现
def pack_info(_params):
    return dict(PACK_INFO)


def health(_params):
    return {"status": "ok", "pid": os.getpid(), "api_version": API_VERSION,
            "pack": PACK_ID, "version": PACK_VERSION}


def _resolve_action(options):
    """`options.action` → 动作名。缺/未知一律 -32003（参数非法），并列出可用动作。"""
    raw = options.get("action")
    action = str(raw).strip() if isinstance(raw, (str, int, float)) else ""
    if not action:
        raise RpcError(-32003,
                       "缺 options.action（本包六个动作：%s）" % "/".join(ACTION_NAMES))
    if action not in _HANDLERS:
        raise RpcError(-32003, "未知动作 %r（可用：%s）" % (action, "/".join(ACTION_NAMES)))
    return action


def _work_dir(options):
    """产物目录：宿主注入的 `options.workdir`（保留键，协议 §3.4）→ 环境变量兜底。"""
    work_dir = options.get("workdir") or os.environ.get("DAMPACK_WORK_DIR") or ""
    work_dir = str(work_dir)
    if not work_dir:
        raise RpcError(-32003, "缺 options.workdir（宿主必须注入产物目录，协议 §3.4）")
    return work_dir


def _sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _attach_artifact(row):
    """结果行的**产物去处**（协议 §4.4）：处理类只准 `export_file` / `working_version`。

    装配层统一补：各组 handler 只负责写盘并回 `fields.outPath/sha256`，产物登记放在这里，
    免得六个动作各写一遍（漏一个就是「跑成功了但宿主拿不到产物」）。
    `rename` 不出新文件（只改资产名）→ 显式空数组，绝不编 `versionId`。
    """
    fields = row.get("fields") or {}
    out_path = fields.get("outPath")
    if row.get("result") != "ok":
        return row
    if out_path:
        sha = fields.get("sha256") or (_sha256_file(out_path) if os.path.isfile(out_path) else None)
        if sha:
            row["artifacts"] = [{"kind": "export_file", "path": str(out_path), "sha256": sha}]
    else:
        row.setdefault("artifacts", [])
    return row


def _run_action(job_id, asset, options, gw):
    action = _resolve_action(options)
    work_dir = _work_dir(options)
    group, handler = _HANDLERS[action]
    if _cancel_flag(job_id).is_set():
        raise RpcError(-32004, "已取消（job_id=%s）" % (job_id,))
    started = time.time()
    if action == "rename":
        # 批内登记表由 overlay 持有：显式传入，绝不依赖模块级单例被别的 job 污染
        row = handler(asset, options, gw, work_dir, registry=overlay.RENAME_REGISTRY)
    else:
        row = handler(asset, options, gw, work_dir)
    row.setdefault("fields", {})
    row["fields"].setdefault("action", action)
    row["fields"].setdefault("group", group)
    row["fields"]["durationMs"] = int((time.time() - started) * 1000)
    return _attach_artifact(row)


def _make_gateway():
    """网关实例：默认按环境（`DAMPACK_GATEWAY_URL/TOKEN`）建；测试可替换 `GATEWAY_FACTORY`。"""
    return GATEWAY_FACTORY(timeout=float(os.environ.get("DAMPACK_TIMEOUT_SECS") or TIMEOUT_SECS))


def judge(params):
    """一次调用 = 一个资产 × 一个动作 → 一行结果（`rows[]` 形态，协议 §4.4）。"""
    job_id = params.get("job_id") or ""
    asset = params.get("asset") or {}
    options = params.get("options") or {}
    if not isinstance(asset, dict) or not asset.get("path"):
        raise RpcError(-32003, "缺 asset.path（插件只收宿主给的本机绝对路径）")
    if not isinstance(options, dict):
        raise RpcError(-32003, "options 必须是对象（动作与参数都在里面）")
    _begin_job(job_id)
    if _cancel_flag(job_id).is_set():
        raise RpcError(-32004, "已取消（job_id=%s）" % (job_id,))
    gw = _make_gateway()
    row = _run_action(job_id, asset, options, gw)
    _record(job_id, row, row["fields"]["action"])
    return {"asset_id": row.get("assetId"), "rows": [row]}


def repair(_params):
    """处理类包不实现 repair（见模块 docstring 最后一段）：如实回 -32601，不假装修好了。"""
    raise RpcError(-32601,
                   "本包 kinds=[process]（处理类），不实现 repair；"
                   "要用某个动作处理资产请走 judge + options.action")


def report(params):
    job_id = str(params.get("job_id") or "")
    with _STATE_LOCK:
        entries = [dict(e) for e in _REPORTS.get(job_id, [])]
    return {"job_id": job_id or None, "entries": entries, "count": len(entries),
            "pack": PACK_ID, "version": PACK_VERSION}


def cancel(params):
    job_id = str(params.get("job_id") or "")
    _cancel_flag(job_id).set()
    return {"canceled": True, "job_id": job_id or None}


HANDLERS = {
    "pack.info": pack_info,
    "health": health,
    "judge": judge,
    "repair": repair,
    "report": report,
    "cancel": cancel,
}

UNKNOWN_METHOD_CODE = -32601
_WRITE_LOCK = threading.Lock()


def _handle_line(line):
    """一条协议行 → 一帧响应（通知帧返回 None）。"""
    try:
        frame = json.loads(line)
    except Exception:
        return {"jsonrpc": "2.0", "id": None,
                "error": {"code": -32700, "message": "JSON 解析失败"}}
    if not isinstance(frame, dict):
        return {"jsonrpc": "2.0", "id": None,
                "error": {"code": -32700, "message": "帧必须是对象"}}
    method = frame.get("method")
    method = method if isinstance(method, str) else ""
    params = frame.get("params") or {}
    frame_id = frame.get("id")
    if not isinstance(params, dict):
        params = {}
    if method == "cancel":
        job_id = str(params.get("job_id") or "")
        _cancel_flag(job_id).set()
        if frame_id is None:                       # 通知帧：置位即走
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "result": {"canceled": True, "job_id": job_id or None}}
    handler = HANDLERS.get(method)
    if handler is None:
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": UNKNOWN_METHOD_CODE, "message": "未知方法：%s" % method}}
    try:
        result = handler(params)
    except RpcError as exc:
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": exc.code, "message": exc.message}}
    except Exception as exc:                       # 插件自身 bug 一律 -32000，绝不静默成空结果
        log("方法 %s 异常：%r" % (method, exc))
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": -32000, "message": "插件内部错误：%r" % (exc,)}}
    if frame_id is None:
        return None
    return {"jsonrpc": "2.0", "id": frame_id, "result": result}


def serve(stdin=None, stdout=None):
    """主循环：读行即派发到线程（长任务不能挡住 cancel 通知）。"""
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    workers = []

    def run(line, out):
        frame = _handle_line(line)
        if frame is not None:
            with _WRITE_LOCK:
                out.write(json.dumps(frame, ensure_ascii=False) + "\n")
                out.flush()

    log("ready", PACK_ID, PACK_VERSION, "api", API_VERSION,
        "actions", ",".join(ACTION_NAMES))
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        thread = threading.Thread(target=run, args=(line, stdout), daemon=True)
        thread.start()
        workers.append(thread)
        workers = [w for w in workers if w.is_alive()]
    for thread in workers:
        thread.join(timeout=5)
    return 0


if __name__ == "__main__":
    sys.exit(serve())
