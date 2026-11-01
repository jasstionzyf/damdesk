"""行式 JSON-RPC 服务（协议 §2/§7）：装饰器注册 + stdout 保护 + 取消 + 兜底错误帧。

用它写包只需要三段：

    pack = PackServer(pack_id=..., name=..., version=..., rules=[...], capabilities=[...])

    @pack.method("run")
    def run(params): ...

    if __name__ == "__main__":
        pack.serve()

SDK 自动接管四件事（插件作者**不该**再各写一遍）：
    1. `pack.info` —— 由上面那些元数据生成，与 manifest 同源，不会漂移；
    2. `health`   —— 立即应答（宿主的就绪探针，超时即判启动失败）；
    3. `cancel`   —— 置位 + 让正在跑的 `run` 在下一次 `check_cancel` 处抛
                     `ERR_CANCELED`（协议 §2：取消后不得再发 verdict / report/emit）；
    4. `report`   —— 默认按 `emit()` 的账本汇总（assets/entries/rows + csv）。

stdout 保护（协议 §7 铁律 4）：`serve()` 一旦接手就把真 fd1 存下来、把 fd1 接到
stderr，之后**任何库里/插件里的 `print` 都落 stderr**，协议帧只从保存的那条流出去。
这比「启动器挡一次」更严：插件跑起来之后第三方库再 print 也污染不了协议。
Windows 上显式 `newline="\\n"`（默认会把一行 JSON-RPC 写成 `\\r\\n`，宿主按行切没问题，
但 `ensure_ascii`/换行口径要统一，免得两边对不上）。
"""
from __future__ import annotations

import json
import os
import sys
import threading
import traceback

from .errors import RpcError
from .gateway import Gateway
from .protocol import (
    API_VERSION,
    ERR_CANCELED,
    ERR_INVALID_PARAMS,
    ERR_METHOD_NOT_FOUND,
    ERR_OTHER,
    ERR_PARSE,
    KNOWN_CAPABILITIES,
    NOTIFICATIONS,
    validate_pack_info,
    validate_verdict,
)

JSONRPC = "2.0"
_BUILTIN_METHODS = ("pack.info", "health", "cancel", "report")


def log(*parts):
    """日志只走 stderr：stdout 是协议通道，一行都不能脏。"""
    sys.stderr.write(" ".join(str(p) for p in parts) + "\n")
    sys.stderr.flush()


def protect_stdout():
    """把 fd1 接到 stderr，返回写协议帧用的真 stdout 流。

    **必须在起服务前调用一次且只调一次**；返回值是唯一合法的协议写出通道。
    """
    real_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return os.fdopen(real_fd, "w", encoding="utf-8", newline="\n")


class _FrameWriter:
    """一次一帧、串行写（多线程 handler 并发应答时不许交错）。"""

    def __init__(self, stream):
        self._stream = stream
        self._lock = threading.Lock()

    def send(self, frame):
        line = json.dumps(frame, ensure_ascii=False)
        with self._lock:
            self._stream.write(line + "\n")
            self._stream.flush()

    def result(self, req_id, result):
        self.send({"jsonrpc": JSONRPC, "id": req_id, "result": result})

    def error(self, req_id, code, message):
        self.send({"jsonrpc": JSONRPC, "id": req_id,
                   "error": {"code": int(code), "message": str(message)}})

    def close(self):
        try:
            self._stream.flush()
        except Exception:  # noqa: BLE001 —— 关流失败不该盖住真正的退出原因
            pass


class PackServer:
    """一个包的协议壳。元数据同时用于 `pack.info` 与自检。"""

    def __init__(self, *, pack_id, name, version, rules, capabilities,
                 deterministic=True, timeout_secs=60, api_version=API_VERSION,
                 resources=None, gateway=None):
        self.pack_id = _require_text(pack_id, "pack_id")
        self.name = _require_text(name, "name")
        self.version = _require_text(version, "version")
        self.api_version = api_version
        if api_version != API_VERSION:
            raise ValueError("api_version=%r，SDK 只实现 %r" % (api_version, API_VERSION))
        if not isinstance(rules, list) or not rules:
            raise ValueError("rules 必须是非空数组（每条至少带 id）")
        for rule in rules:
            if not isinstance(rule, dict) or not str(rule.get("id") or "").strip():
                raise ValueError("rules 每项必须是带 id 的对象：%r" % (rule,))
        self.rules = [dict(rule) for rule in rules]
        caps = list(capabilities or [])
        unknown = sorted(set(caps) - KNOWN_CAPABILITIES)
        if unknown:
            raise ValueError("未知能力 %s（可用：%s）" % (unknown, sorted(KNOWN_CAPABILITIES)))
        self.capabilities = caps
        self.deterministic = bool(deterministic)
        self.timeout_secs = int(timeout_secs)
        if not 1 <= self.timeout_secs <= 3600:
            raise ValueError("timeout_secs 必须是 1..3600")
        self.resources = list(resources or [])

        self._gateway = gateway
        self._handlers: dict = {
            "pack.info": lambda params: self.pack_info(),
            "health": lambda params: self.health(),
            "cancel": self._cancel,
            "report": self._report,
        }
        self._cancel_flags = {}
        self._reports = {}
        self._lock = threading.Lock()
        self._threads = []

    # --- 元数据 -------------------------------------------------------------
    def pack_info(self):
        info = {
            "id": self.pack_id,
            "name": self.name,
            "version": self.version,
            "api_version": self.api_version,
            "rules": [dict(rule) for rule in self.rules],
            "capabilities": list(self.capabilities),
            "deterministic": self.deterministic,
            "timeout_secs": self.timeout_secs,
        }
        if self.resources:
            info["resources"] = list(self.resources)
        return info

    def health(self):
        return {"status": "ok", "pack": self.pack_id, "version": self.version,
                "api_version": self.api_version, "deterministic": self.deterministic,
                "pid": os.getpid()}

    @property
    def gateway(self):
        if self._gateway is None:
            self._gateway = Gateway.from_env()
        return self._gateway

    # --- 注册 ---------------------------------------------------------------
    def method(self, name):
        """装饰器注册：`@pack.method("run")`。同名后注册者覆盖先注册者（含内置）。"""
        def register(fn):
            if not callable(fn):
                raise TypeError("%s 的 handler 不可调用" % name)
            self._handlers[str(name)] = fn
            return fn
        return register

    def methods(self):
        return sorted(self._handlers)

    # --- 取消 ---------------------------------------------------------------
    def cancel_event(self, job_id):
        with self._lock:
            flag = self._cancel_flags.get(job_id)
            if flag is None:
                flag = threading.Event()
                self._cancel_flags[job_id] = flag
            return flag

    def check_cancel(self, job_id, stage=""):
        flag = self._cancel_flags.get(job_id)
        if flag is not None and flag.is_set():
            raise RpcError(ERR_CANCELED, "任务已取消%s"
                           % (("（%s）" % stage) if stage else ""))

    def clear_cancel(self, job_id):
        with self._lock:
            self._cancel_flags.pop(job_id, None)

    def _cancel(self, params):
        job_id = _job_id(params, required=False)
        if job_id is None:
            raise RpcError(ERR_INVALID_PARAMS, "cancel 需要 job_id")
        self.cancel_event(job_id).set()
        return {"canceled": True, "job_id": job_id}

    # --- report 账本 ---------------------------------------------------------
    def emit(self, job_id, asset, verdicts, extra=None):
        """落 `report/emit` 并记账（返回宿主的应答）。取消后**禁止**再调。"""
        self.check_cancel(job_id, "report/emit")
        self._validate_verdicts(verdicts)
        entry = {
            "asset_id": (asset or {}).get("asset_id") or (asset or {}).get("id"),
            "path": (asset or {}).get("path"),
            "pack": self.pack_id,
            "pack_version": self.version,
            "verdicts": [dict(v) for v in verdicts],
        }
        if extra:
            entry.update(dict(extra))
        with self._lock:
            self._reports.setdefault(job_id, []).append(entry)
        return self.gateway.report_emit(job_id, entry)

    def _report(self, params):
        job_id = _job_id(params)
        with self._lock:
            entries = [dict(e) for e in self._reports.get(job_id, [])]
        assets = []
        for entry in entries:
            if entry.get("asset_id") not in (None, ""):
                assets.append(entry["asset_id"])
        # v2.0：计数中性化为 `rows`（结果行总数，不区分是不是判定行 —— 协议 §3.4）。
        rows = sum(len(entry.get("verdicts", [])) for entry in entries)
        return {"job_id": job_id, "assets": assets, "entries": entries,
                "rows": rows, "csv": _to_csv(entries)}

    # --- 服务 ---------------------------------------------------------------
    def serve(self, stdin=None, stdout=None):
        """起服务（阻塞到 stdin EOF）。`stdin/stdout` 只给单测用。"""
        if stdout is None:
            stream = protect_stdout()
        else:
            stream = stdout
        if stdin is None:
            _force_utf8(sys.stdin)
        writer = _FrameWriter(stream)
        log("[dampack] serving %s@%s (api %s, methods: %s)"
            % (self.pack_id, self.version, self.api_version, ", ".join(self.methods())))
        reading = stdin if stdin is not None else sys.stdin
        try:
            for line in reading:
                line = line.strip()
                if not line:
                    continue
                self._dispatch(line, writer)
        finally:
            for thread in list(self._threads):
                thread.join(timeout=5.0)
            writer.close()
            log("[dampack] stdin EOF，退出")

    def _dispatch(self, line, writer):
        try:
            frame = json.loads(line)
        except ValueError as exc:
            writer.error(None, ERR_PARSE, "解析失败：%s" % (exc,))
            return
        if not isinstance(frame, dict):
            writer.error(None, ERR_PARSE, "帧必须是 JSON 对象")
            return
        req_id = frame.get("id")
        name = frame.get("method")
        params = frame.get("params") or {}
        if not isinstance(name, str) or not name:
            writer.error(req_id, ERR_METHOD_NOT_FOUND if name else ERR_PARSE,
                         "缺 method")
            return
        if req_id is None:
            # 通知：无应答（协议 §2）。插件侧只记一行 stderr，便于排障。
            log("[dampack] notification %s（忽略）" % name)
            return
        if name in NOTIFICATIONS:
            log("[dampack] 收到本应单向的通知 %s，忽略" % name)
            return
        handler = self._handlers.get(name)
        if handler is None:
            writer.error(req_id, ERR_METHOD_NOT_FOUND,
                         "未知方法 %s（已注册：%s）" % (name, ", ".join(self.methods())))
            return
        thread = threading.Thread(target=self._run, args=(name, handler, params, req_id, writer),
                                  name="rpc-%s" % name, daemon=True)
        self._threads = [t for t in self._threads if t.is_alive()]
        self._threads.append(thread)
        thread.start()

    def _run(self, name, handler, params, req_id, writer):
        try:
            if not isinstance(params, dict):
                raise RpcError(ERR_INVALID_PARAMS, "params 必须是对象")
            result = handler(params)
            if name == "run":
                result = self._check_judge(name, result)
            writer.result(req_id, result)
        except RpcError as exc:
            writer.error(req_id, exc.code, exc.message)
        except Exception as exc:  # noqa: BLE001 —— 任何未捕获异常都必须变成错误帧
            log("[dampack] handler %s 异常：\n%s" % (name, traceback.format_exc()))
            writer.error(req_id, ERR_OTHER, "%s 失败：%s: %s"
                         % (name, type(exc).__name__, exc))

    def _check_judge(self, name, result):
        """`run` 的应答必须带合法 verdicts —— 出错了就红，别让宿主收半截。"""
        if not isinstance(result, dict):
            raise RpcError(ERR_OTHER, "%s 必须返回对象" % name)
        verdicts = result.get("verdicts")
        if not isinstance(verdicts, list) or not verdicts:
            raise RpcError(ERR_OTHER, "%s 必须返回非空 verdicts 数组" % name)
        self._validate_verdicts(verdicts)
        return result

    @staticmethod
    def _validate_verdicts(verdicts):
        for index, item in enumerate(verdicts or []):
            problems = validate_verdict(item)
            if problems:
                # 包自己产出的 verdict 不合规是**插件 bug**，不是「参数非法」：
                # 回 -32000 让宿主把这条任务记失败，而不是误导成调用方传错参数。
                raise RpcError(ERR_OTHER,
                               "verdicts[%d] 非法：%s" % (index, "；".join(problems)))


def _job_id(params, required=True):
    value = (params or {}).get("job_id")
    if value in (None, "") and required:
        raise RpcError(ERR_INVALID_PARAMS, "缺少 job_id")
    return None if value in (None, "") else str(value)


def _require_text(value, field):
    if not isinstance(value, str) or not value.strip():
        raise ValueError("%s 必须是非空字符串" % field)
    return value.strip()


def _to_csv(entries):
    """report 的 csv 列口径（宿主/CI 直接落盘查看，不用再自己拼）。"""
    lines = ["asset_id,rule_id,status,score,fixed,note"]
    for entry in entries:
        asset = entry.get("asset_id")
        for item in entry.get("verdicts", []):
            score = item.get("score")
            lines.append(",".join([
                _csv(asset), _csv(item.get("ruleId")), _csv(item.get("status")),
                "" if score is None else repr(float(score)),
                "1" if item.get("fixed") else "0", _csv(item.get("note")),
            ]))
    return "\n".join(lines)


def _csv(value):
    text = "" if value is None else str(value)
    if any(ch in text for ch in ',"\n'):
        return '"%s"' % text.replace('"', '""')
    return text


def _force_utf8(stream):
    reconfigure = getattr(stream, "reconfigure", None)
    if callable(reconfigure):
        try:
            reconfigure(encoding="utf-8")
        except (ValueError, OSError):
            pass
