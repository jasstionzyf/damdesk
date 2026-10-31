"""包自测夹具：可编排的网关桩 + 按协议拉起插件子进程。

为什么不直接复用 `tools/pack-conformance/gateway.py`：
    conformance 假网关只按**文件名约定**回固定桩（考的是协议面），而本包的单测要
    考**规则语义**（同组重复、场景齐套、几何修复），需要能按文件名编排 VL 答案
    与真实尺寸。两者目的不同，桩也不同；协议面的一致性由 run_tests.sh 里的
    conformance 那一段兜住。
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PACK_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURES = os.path.join(PACK_DIR, "tests", "fixtures")

ENDPOINTS = {
    "/io/read": "io.read",
    "/detect/u2net": "detect.u2net",
    "/detect/ocr": "detect.ocr",
    "/op/crop": "op.geometric",
    "/op/letterbox": "op.geometric",
    "/op/upscale": "op.geometric",
    "/op/replace_bg": "op.generative",
    "/op/inpaint": "op.generative",
    "/vl/judge": "vl.judge",
    "/report/emit": "report.emit",
}

SCENE_PROMPT_MARK = "属于下列哪一类场景"


class StubGateway:
    """10 端点桩：io.read 按真实尺寸表、vl.judge 按文件名编排、op 只落个空壳文件。"""

    def __init__(self, capabilities, workroot, dims=None, domain=None, scenes=None,
                 capabilities_deny=()):
        self.capabilities = list(capabilities or [])
        self.workroot = os.path.abspath(workroot)
        self.dims = dict(dims or {})            # 绝对路径 → (w, h)
        self.domain = dict(domain or {})        # basename → dict(raw=..., undetermined=...)
        self.scenes = dict(scenes or {})        # basename → 场景词
        self.deny = set(capabilities_deny)      # 强制 403 的能力（能力白名单用例）
        self.token = "stub-token"
        self.emits = []
        self.requests = []
        self._lock = threading.Lock()
        self._httpd = None

    # --- 生命周期 -------------------------------------------------------
    def start(self):
        gw = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _send(self, code, payload):
                body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("content-type", "application/json; charset=utf-8")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                size = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(size) if size else b""
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except Exception:
                    payload = {}
                token = (self.headers.get("authorization") or "").replace("Bearer ", "")
                cap = ENDPOINTS.get(self.path)
                with gw._lock:
                    gw.requests.append({"path": self.path, "payload": payload})
                if token.strip() != gw.token:
                    return self._send(401, {"error": "unauthorized"})
                if cap is None:
                    return self._send(404, {"error": "no_such_endpoint", "path": self.path})
                if cap not in gw.capabilities or cap in gw.deny:
                    return self._send(403, {"error": "capability_missing",
                                            "capability": cap})
                try:
                    code, body = gw.dispatch(self.path, payload)
                except _Err as exc:
                    return self._send(exc.status, {"error": exc.reason})
                return self._send(code, body)

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._httpd.serve_forever, daemon=True).start()
        return self

    @property
    def base_url(self):
        host, port = self._httpd.server_address[:2]
        return "http://%s:%d" % (host, port)

    def stop(self):
        if self._httpd is not None:
            self._httpd.shutdown()
            self._httpd.server_close()

    # --- 端点 -----------------------------------------------------------
    def dispatch(self, path, payload):
        if path == "/io/read":
            target = payload.get("path")
            if not os.path.isfile(str(target or "")):
                raise _Err(404, "asset_unreadable")
            width, height = self.dims.get(os.path.abspath(str(target)), (0, 0))
            return 200, {"path": target, "size": os.path.getsize(target),
                         "width": width, "height": height, "mime": "image/jpeg"}
        if path == "/vl/judge":
            return self._vl(payload)
        if path.startswith("/op/"):
            return self._op(path, payload)
        if path == "/report/emit":
            with self._lock:
                self.emits.append(dict(payload))
            return 200, {"ok": True, "job_id": payload.get("job_id")}
        raise _Err(404, "no_such_endpoint")

    def _vl(self, payload):
        target = str(payload.get("path") or "")
        if not os.path.isfile(target):
            raise _Err(404, "asset_unreadable")
        name = os.path.basename(target)
        prompt = str(payload.get("prompt") or "")
        if SCENE_PROMPT_MARK in prompt:
            scene = self.scenes.get(name)
            if scene is None:
                return 200, {"path": target, "undetermined": True,
                             "reason": "vl_parse_failed", "raw": ""}
            return 200, {"path": target, "undetermined": False, "raw": scene}
        answer = self.domain.get(name) or {"raw": "是"}
        if answer.get("undetermined"):
            return 200, {"path": target, "undetermined": True,
                         "reason": answer.get("reason") or "vl_parse_failed", "raw": ""}
        body = {"path": target, "undetermined": False, "raw": answer.get("raw") or "是"}
        if answer.get("status"):
            body["rules"] = [{"status": answer["status"], "label": "realestate",
                              "score": answer.get("score", 0.9)}]
        return 200, body

    def _op(self, path, payload):
        out = str(payload.get("out") or "")
        if not out:
            raise _Err(400, "invalid_params")
        out_abs = os.path.abspath(out)
        if not out_abs.startswith(self.workroot + os.sep):
            raise _Err(400, "out_of_workroot")
        src = str(payload.get("path") or "")
        if not os.path.isfile(src):
            raise _Err(404, "asset_unreadable")
        width, height = self.dims.get(os.path.abspath(src), (0, 0))
        if path == "/op/upscale":
            scale = float(payload.get("scale") or 1.0)
            width, height = int(round(width * scale)), int(round(height * scale))
        os.makedirs(os.path.dirname(out_abs), exist_ok=True)
        with open(out_abs, "wb") as fh:
            fh.write(b"\xff\xd8stub-artifact\xff\xd9")
        self.dims[out_abs] = (width, height)
        return 200, {"path": out_abs, "width": width, "height": height,
                     "op": path.rsplit("/", 1)[-1]}

    def emits_for(self, job_id):
        with self._lock:
            return [e for e in self.emits if e.get("job_id") == job_id]


class _Err(Exception):
    def __init__(self, status, reason):
        self.status = status
        self.reason = reason


class Pack:
    """按协议拉起插件子进程（stdin/stdout 行式 JSON-RPC），同步收发。"""

    def __init__(self, workroot, gateway=None, capabilities=None, dims=None,
                 domain=None, scenes=None, deny=()):
        self.workroot = os.path.abspath(workroot)
        self.manifest = json.load(open(os.path.join(PACK_DIR, "manifest.json"),
                                      encoding="utf-8"))
        caps = capabilities if capabilities is not None else self.manifest["capabilities"]
        self.gateway = gateway or StubGateway(caps, self.workroot, dims=dims,
                                             domain=domain, scenes=scenes,
                                             capabilities_deny=deny).start()
        self._owns_gateway = gateway is None
        self.proc = None
        self._seq = 0
        self._lock = threading.Lock()
        self.stderr = []

    def __enter__(self):
        env = os.environ.copy()
        env.update({
            "DAMPACK_GATEWAY_URL": self.gateway.base_url,
            "DAMPACK_GATEWAY_TOKEN": self.gateway.token,
            "DAMPACK_WORK_DIR": self.workroot,
            "PYTHONUNBUFFERED": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        })
        env.pop("PYTHONPATH", None)
        entry = self.manifest["entrypoint"]
        cmd = [sys.executable if entry["cmd"] in ("python3", "python") else entry["cmd"]]
        cmd += [a if os.path.isabs(a) else os.path.join(PACK_DIR, a)
                for a in entry.get("args") or []]
        self.proc = subprocess.Popen(cmd, cwd=PACK_DIR, env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True,
                                     encoding="utf-8", bufsize=1)
        threading.Thread(target=self._read_stderr, daemon=True).start()
        return self

    def __exit__(self, *exc):
        if self.proc is not None:
            try:
                self.proc.stdin.close()
                self.proc.wait(timeout=10)
            except Exception:
                self.proc.kill()
        if self._owns_gateway:
            self.gateway.stop()
        return False

    def _read_stderr(self):
        for line in self.proc.stderr:
            self.stderr.append(line.rstrip("\n"))

    def call(self, method, params, timeout=120):
        with self._lock:
            self._seq += 1
            mid = self._seq
            frame = {"jsonrpc": "2.0", "id": mid, "method": method,
                     "params": params}
            self.proc.stdin.write(json.dumps(frame, ensure_ascii=False) + "\n")
            self.proc.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("插件进程退出：%s" % "\n".join(self.stderr[-5:]))
            frame = json.loads(line)
            if frame.get("id") == mid:
                return frame
        raise TimeoutError("等 %s 应答超时" % method)


def asset(path, asset_id=None):
    return {"id": asset_id or os.path.basename(path), "path": path}


def fixture_dims():
    """夹具真实尺寸（io.read 元数据桩用）。"""
    path = os.path.join(FIXTURES, "pillow_reference.json")
    ref = json.load(open(path, encoding="utf-8"))
    return {os.path.abspath(os.path.join(FIXTURES, name)): (row["width"], row["height"])
            for name, row in ref.items()}
