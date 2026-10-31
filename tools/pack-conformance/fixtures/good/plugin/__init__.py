#!/usr/bin/env python3
"""参考实现行业包插件（故意正确）。

协议：`docs/industry-pack-protocol.md`。本文件既是模块（导出 PACK_INFO /
HANDLERS），也能直接作为脚本运行（stdio JSON-RPC 服务端）。

三条必守（Python 插件）：
1. 先 `proto_out = os.dup(1)` 再把 fd1 接到 stderr —— 否则任何库里的 `print`
   都会污染协议流；
2. 输出 utf-8 + `ensure_ascii=False` + 逐行 flush（Windows 显式 `newline="\\n"`）；
3. 每个请求一个工作线程 —— judge 执行期间仍必须能处理 `cancel`。
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

API_VERSION = "1.1"

# 缺省错误码（变体夹具会改写它们来模拟违规实现）。
UNKNOWN_METHOD_CODE = -32601
PARSE_ERROR_CODE = -32700

PACK_INFO = {
    "id": "conformance.good",
    "name": "Conformance Reference Pack",
    "version": "1.0.0",
    "api_version": API_VERSION,
    "rules": [
        {"id": "A2_min_short_side", "name": "主图短边 ≥1600", "deterministic": True},
        {"id": "A4_subject_ratio", "name": "主体占比 ≥0.55", "deterministic": True},
        {"id": "A5_overlay_text", "name": "无促销叠加文字", "deterministic": False},
        {"id": "A6_category", "name": "类目一致", "deterministic": False},
    ],
    "capabilities": ["io.read", "detect.u2net", "detect.ocr", "op.geometric",
                     "op.generative", "vl.judge", "report.emit"],
    "deterministic": False,
    "timeout_secs": 60,
}

SHORT_SIDE_MIN = 1600
SUBJECT_RATIO_MIN = 0.55

_LOCK = threading.Lock()
_CANCEL = {}      # job_id -> threading.Event
_REPORTS = {}     # job_id -> [ {assetId, verdicts} ]


class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


# --- 网关调用（唯一算力入口；不直接联网做业务） --------------------------
def gw(path, payload):
    base = os.environ.get("DAMPACK_GATEWAY_URL", "").rstrip("/")
    if not base:
        raise RpcError(-32000, "DAMPACK_GATEWAY_URL 未注入（宿主必须提供）")
    token = os.environ.get("DAMPACK_GATEWAY_TOKEN", "")
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(base + path, data=data, headers={
        "content-type": "application/json",
        "authorization": "Bearer " + token,
    })
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", "replace")[:400]
        if exc.code == 403:
            raise RpcError(-32001, "capability missing: " + body)
        if exc.code in (404, 410):
            raise RpcError(-32002, "asset unreadable: " + body)
        if exc.code == 400:
            raise RpcError(-32003, "invalid params: " + body)
        raise RpcError(-32000, "gateway http %d: %s" % (exc.code, body))
    except (urllib.error.URLError, OSError) as exc:
        raise RpcError(-32000, "gateway unreachable: %s" % (exc,))


# --- 取消位 -------------------------------------------------------------
def _cancel_event(job_id):
    with _LOCK:
        return _CANCEL.setdefault(job_id, threading.Event())


def check_cancel(job_id):
    if _cancel_event(job_id).is_set():
        raise RpcError(-32004, "canceled")


# --- verdict 构造 -------------------------------------------------------
def verdict(rule_id, status, score=None, label="", evidence=None,
            fixed=False, note=None):
    return {
        "ruleId": rule_id,
        "status": status,
        "score": score,
        "label": label,
        "evidence": evidence or {},
        "fixed": fixed,
        "note": note,
    }


# --- 判定 ---------------------------------------------------------------
def judge(job_id, asset, rules=None, options=None):
    asset = asset or {}
    path = asset.get("path")
    if not isinstance(path, str) or not path:
        raise RpcError(-32003, "asset.path required")

    # slow 夹具：给宿主留出 cancel 窗口，并在窗口内检查取消位。
    if "slow" in os.path.basename(path):
        for _ in range(200):
            check_cancel(job_id)
            time.sleep(0.05)

    info = gw("/io/read", {"path": path})
    rule_ids = list(rules) if rules else [r["id"] for r in PACK_INFO["rules"]]
    verdicts = []
    for rule_id in rule_ids:
        check_cancel(job_id)
        verdicts.append(_judge_one(rule_id, path, info))

    gw("/report/emit", {"job_id": job_id,
                        "entry": {"asset_id": asset.get("id"), "verdicts": verdicts}})
    with _LOCK:
        _REPORTS.setdefault(job_id, []).append(
            {"assetId": asset.get("id"), "verdicts": verdicts})
    return {"asset_id": asset.get("id"), "verdicts": verdicts}


def _judge_one(rule_id, path, info):
    if rule_id == "A2_min_short_side":
        short = min(info["width"], info["height"])
        ok = short >= SHORT_SIDE_MIN
        return verdict(rule_id, "pass" if ok else "fail",
                       score=round(short / SHORT_SIDE_MIN, 3) if not ok else 1.0,
                       label="short_side",
                       evidence={"width": info["width"], "height": info["height"],
                                 "short_side": short, "threshold": SHORT_SIDE_MIN})

    if rule_id == "A4_subject_ratio":
        seg = gw("/detect/u2net", {"path": path})
        ratio = float(seg["fg_ratio"])
        ok = ratio >= SUBJECT_RATIO_MIN
        return verdict(rule_id, "pass" if ok else "fail", score=ratio,
                       label="subject_ratio",
                       evidence={"fg_ratio": ratio, "threshold": SUBJECT_RATIO_MIN,
                                 "mask_path": seg.get("mask_path")})

    if rule_id == "A5_overlay_text":
        ocr = gw("/detect/ocr", {"path": path, "lang": "chi_sim+eng"})
        if ocr.get("lines"):
            return verdict(rule_id, "fail", score=1.0, label="overlay_text",
                           evidence={"lines": ocr["lines"], "source": "ocr"})
        vl = gw("/vl/judge", {"path": path, "prompt": "主图是否含促销叠加文字？",
                              "max_tokens": 4096})
        if vl.get("undetermined"):
            return verdict(rule_id, "undetermined", score=None, label="vl",
                           evidence={"reason": vl.get("reason"), "source": "vl"},
                           note="vl_undetermined")
        first = (vl.get("rules") or [{}])[0]
        return verdict(rule_id, first.get("status", "undetermined"),
                       score=first.get("score"), label=first.get("label", "vl"),
                       evidence={"source": "vl", "raw": vl.get("raw", "")})

    if rule_id == "A6_category":
        vl = gw("/vl/judge", {"path": path, "prompt": "这张图属于服装类目吗？",
                              "max_tokens": 4096})
        if vl.get("undetermined"):
            return verdict(rule_id, "undetermined", score=None, label="vl",
                           evidence={"reason": vl.get("reason")}, note="vl_undetermined")
        first = (vl.get("rules") or [{}])[0]
        return verdict(rule_id, first.get("status", "undetermined"),
                       score=first.get("score"), label=first.get("label", "vl"),
                       evidence={"source": "vl", "raw": vl.get("raw", "")})

    return verdict(rule_id, "skipped", note="unknown_rule")


# --- 修复（几何闭式：crop → letterbox → upscale） -----------------------
def repair(job_id, asset, rules=None, options=None):
    asset = asset or {}
    path = asset.get("path")
    if not isinstance(path, str) or not path:
        raise RpcError(-32003, "asset.path required")
    options = options or {}
    workdir = options.get("workdir") or os.environ.get("DAMPACK_WORK_DIR") \
        or tempfile.mkdtemp(prefix="dampack-")
    os.makedirs(workdir, exist_ok=True)

    rule_ids = list(rules) if rules else ["A2_min_short_side", "A4_subject_ratio"]
    verdicts, artifacts = [], []
    for rule_id in rule_ids:
        check_cancel(job_id)
        if rule_id not in ("A2_min_short_side", "A4_subject_ratio"):
            verdicts.append(verdict(rule_id, "skipped", note="fix_policy=judge_only"))
            continue
        crop = gw("/op/crop", {"path": path, "box": [0, 0, 1000, 1000],
                               "out": os.path.join(workdir, rule_id + "-1-crop.png")})
        letterbox = gw("/op/letterbox", {"path": crop["path"], "target_ratio": 1.0,
                                         "background": "#ffffff",
                                         "out": os.path.join(workdir, rule_id + "-2-letterbox.png")})
        up = gw("/op/upscale", {"path": letterbox["path"], "scale": 2.0,
                                "out": os.path.join(workdir, rule_id + "-3-upscale.png")})
        artifacts.append({"rule_id": rule_id, "path": up["path"],
                          "width": up["width"], "height": up["height"],
                          "kind": "image/png"})
        verdicts.append(verdict(rule_id, "pass", score=1.0, label="fixed",
                                evidence={"artifact": up["path"]}, fixed=True,
                                note="geometric_closed_form"))
    return {"asset_id": asset.get("id"), "verdicts": verdicts, "artifacts": artifacts}


# --- 报告 / 取消 --------------------------------------------------------
def report(job_id, **_):
    with _LOCK:
        entries = list(_REPORTS.get(job_id, []))
    return {
        "job_id": job_id,
        "judged": sum(len(e["verdicts"]) for e in entries),
        "assets": [e["assetId"] for e in entries],
        "entries": entries,
    }


def cancel(job_id, **_):
    _cancel_event(job_id).set()
    return {"canceled": True, "job_id": job_id}


# --- 分发 ---------------------------------------------------------------
HANDLERS = {
    "pack.info": lambda params: dict(PACK_INFO),
    "health": lambda params: {"status": "ok", "pid": os.getpid(),
                              "api_version": API_VERSION},
    "judge": lambda params: judge(params.get("job_id") or "",
                                  params.get("asset"),
                                  params.get("rules"),
                                  params.get("options") or {}),
    "repair": lambda params: repair(params.get("job_id") or "",
                                    params.get("asset"),
                                    params.get("rules"),
                                    params.get("options") or {}),
    "report": lambda params: report(params.get("job_id") or ""),
    "cancel": lambda params: cancel(params.get("job_id") or ""),
}


def serve():
    raw_out = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")
    os.dup2(2, 1)              # 之后的 print 一律落 stderr
    sys.stdout = sys.stderr
    write_lock = threading.Lock()

    def send(obj):
        line = json.dumps(obj, ensure_ascii=False)
        with write_lock:
            raw_out.write(line + "\n")
            raw_out.flush()

    def handle(req):
        mid = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}
        try:
            fn = HANDLERS.get(method)
            if fn is None:
                raise RpcError(UNKNOWN_METHOD_CODE, "method not found: %s" % method)
            result = fn(params)
        except RpcError as exc:
            if mid is not None:
                send({"jsonrpc": "2.0", "id": mid,
                      "error": {"code": exc.code, "message": exc.message}})
            return
        except Exception as exc:  # noqa: BLE001 —— 任何异常都必须变成合法错误应答
            if mid is not None:
                send({"jsonrpc": "2.0", "id": mid,
                      "error": {"code": -32000, "message": "internal: %r" % (exc,)}})
            return
        if mid is not None:
            send({"jsonrpc": "2.0", "id": mid, "result": result})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception:
            send({"jsonrpc": "2.0", "id": None,
                  "error": {"code": PARSE_ERROR_CODE, "message": "parse error"}})
            continue
        threading.Thread(target=handle, args=(req,), daemon=True).start()


if __name__ == "__main__":
    serve()
