"""夹具：health 正常应答，`judge` 收到后立刻 os._exit(7) —— 模拟运行期崩溃。

宿主应「重启 ×2 → 仍失败 → 冷却」，结果落 `undetermined(plugin_crash)`。
"""
import json
import os
import sys
import threading

PACK_INFO = {
    "id": "test.crash-after-health",
    "name": "crash-after-health",
    "version": "1.0.0",
    "api_version": "2.0",
    "rules": [{"id": "A2_min_short_side", "name": "短边", "deterministic": True}],
    "capabilities": ["io.read"],
    "deterministic": True,
    "timeout_secs": 5,
}

HANDLERS = {
    "pack.info": lambda params: dict(PACK_INFO),
    "health": lambda params: {"status": "ok", "pid": os.getpid(), "api_version": "2.0"},
    "run": lambda params: os._exit(7),
    "judge": lambda params: os._exit(7),
    "repair": lambda params: os._exit(7),
    "report": lambda params: os._exit(7),
    "cancel": lambda params: {"canceled": True, "job_id": params.get("job_id")},
}


def serve():
    raw_out = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    lock = threading.Lock()

    def send(obj):
        with lock:
            raw_out.write(json.dumps(obj, ensure_ascii=False) + "\n")
            raw_out.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        mid = req.get("id")
        fn = HANDLERS.get(req.get("method"))
        if fn is None:
            if mid is not None:
                send({"jsonrpc": "2.0", "id": mid,
                      "error": {"code": -32601, "message": "method not found"}})
            continue
        result = fn(req.get("params") or {})
        if mid is not None:
            send({"jsonrpc": "2.0", "id": mid, "result": result})
