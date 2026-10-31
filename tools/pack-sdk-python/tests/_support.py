"""单测共用支撑：桩网关（HTTP）+ 把 SDK 包当真实子进程跑的 JSON-RPC 客户端。

刻意不用 mock 打桩 SDK 内部：网关错误映射、stdout 保护、取消、帧编码这些
**只有在真实 socket + 真实 fd + 真实子进程**下才有意义（mock 会把它们全绕过去）。
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
SDK_DIR = os.path.dirname(HERE)                       # tools/pack-sdk-python
REPO_ROOT = os.path.abspath(os.path.join(SDK_DIR, "..", ".."))
CONF_DIR = os.path.join(REPO_ROOT, "tools", "pack-conformance")

for extra in (SDK_DIR, CONF_DIR):
    if extra not in sys.path:
        sys.path.insert(0, extra)


# ---------------------------------------------------------------------------
# 桩网关：记录每次命中，按 `responses` 表回状态码/载荷
# ---------------------------------------------------------------------------
class StubGateway:
    """最小网关桩。`responses` = {path: (status, payload)}；未配置的路径回 200 {}。"""

    def __init__(self, responses=None, token="stub-token"):
        self.responses = dict(responses or {})
        self.token = token
        self.requests = []
        self._lock = threading.Lock()
        self._httpd = None

    def start(self):
        gw = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _send(self, code, payload):
                body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("content-type", "application/json; charset=utf-8")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except ValueError:
                    payload = {}
                token = (self.headers.get("authorization") or "").replace("Bearer ", "").strip()
                with gw._lock:
                    gw.requests.append({"path": self.path, "payload": payload, "token": token})
                if token != gw.token:
                    return self._send(401, {"error": "unauthorized"})
                status, result = gw.responses.get(self.path, (200, {}))
                return self._send(status, result)

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

    def hits(self, path=None):
        with self._lock:
            return [r for r in self.requests if path is None or r["path"] == path]

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()
        return False


# ---------------------------------------------------------------------------
# 夹具包：写一个临时包目录（manifest + plugin/__init__.py）
# ---------------------------------------------------------------------------
FIXTURE_PLUGIN = r'''
import os
import sys
import time

sys.path.insert(0, {sdk_dir!r})
from dampack import PackServer, RpcError, ERR_INVALID_PARAMS, verdict

pack = PackServer(pack_id="com.test.fixture", name="夹具包", version="1.0.0",
                  rules=[{{"id": "T1"}}], capabilities=["io.read", "report.emit"],
                  deterministic=True, timeout_secs=30)


@pack.method("judge")
def judge(params):
    if params.get("bad_verdict"):
        return {{"asset_id": "x", "verdicts": [{{"ruleId": "T1"}}]}}
    print("这行 print 必须落 stderr，不许污染协议流")
    job_id = str(params.get("job_id") or "")
    if params.get("slow"):
        deadline = time.time() + 5.0
        while time.time() < deadline:
            pack.check_cancel(job_id, "slow")
            time.sleep(0.02)
    return {{"asset_id": "资产-中文", "verdicts": [verdict("T1", "pass", score=0.5,
                                                          label="ok", evidence={{"k": "值"}})]}}


@pack.method("echo")
def echo(params):
    return dict(params)


@pack.method("emit")
def emit(params):
    job_id = str(params.get("job_id") or "j")
    asset = {{"asset_id": params.get("asset_id") or "a1", "path": "/x/a1.png"}}
    ack = pack.emit(job_id, asset, [verdict("T1", "pass", score=1.0)])
    return {{"ack": ack, "job_id": job_id}}


@pack.method("boom")
def boom(params):
    raise RuntimeError("故意炸")


@pack.method("rpc")
def rpc(params):
    raise RpcError(ERR_INVALID_PARAMS, "参数不合法")


def serve():
    return pack.serve()


if __name__ == "__main__":
    sys.exit(serve())
'''


def make_fixture_pack(sdk_dir=SDK_DIR, name="sdk-fixture-pack"):
    """建一个临时夹具包，返回 (pack_dir, manifest)。"""
    pack_dir = tempfile.mkdtemp(prefix=name + "-")
    os.makedirs(os.path.join(pack_dir, "plugin"), exist_ok=True)
    manifest = {
        "id": "com.test.fixture", "name": "夹具包", "version": "1.0.0",
        "api_version": "1.1",
        "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
        "capabilities": ["io.read"], "deterministic": True, "timeout_secs": 30,
    }
    with open(os.path.join(pack_dir, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, ensure_ascii=False)
    with open(os.path.join(pack_dir, "plugin", "__init__.py"), "w", encoding="utf-8") as fh:
        fh.write(FIXTURE_PLUGIN.format(sdk_dir=sdk_dir))
    return pack_dir, manifest


# ---------------------------------------------------------------------------
# 子进程客户端
# ---------------------------------------------------------------------------
class Response:
    def __init__(self, frame):
        self.frame = frame or {}
        self.result = self.frame.get("result")
        self.error = self.frame.get("error")

    @property
    def code(self):
        return (self.error or {}).get("code")

    @property
    def message(self):
        return (self.error or {}).get("message", "")

    def __repr__(self):
        return "err(%s)" % self.code if self.error else "ok"


class PackProcess:
    """拉起包子进程 + 行式 JSON-RPC 客户端（带超时，超时抛 `TimeoutError`）。"""

    def __init__(self, pack_dir, env=None, python=None):
        self.pack_dir = os.path.abspath(pack_dir)
        with open(os.path.join(self.pack_dir, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        cmd = [python or sys.executable] + [
            arg if os.path.isabs(arg) else os.path.join(self.pack_dir, arg)
            for arg in manifest["entrypoint"]["args"]]
        full_env = dict(os.environ)
        full_env.update({"PYTHONUNBUFFERED": "1",
                         "PYTHONPATH": os.pathsep.join([SDK_DIR]),
                         "NO_PROXY": "127.0.0.1,localhost",
                         "no_proxy": "127.0.0.1,localhost"})
        full_env.update(env or {})
        self.proc = subprocess.Popen(cmd, cwd=self.pack_dir, env=full_env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True,
                                     encoding="utf-8", errors="replace", bufsize=1)
        self.frames = []
        self.stray = []
        self.stderr_lines = []
        self._pending = {}
        self._lock = threading.Lock()
        self._seq = 0
        threading.Thread(target=self._read_out, daemon=True).start()
        threading.Thread(target=self._read_err, daemon=True).start()

    def _read_out(self):
        for line in self.proc.stdout:
            raw = line.rstrip("\n")
            if not raw.strip():
                continue
            try:
                frame = json.loads(raw)
            except ValueError:
                self.stray.append(raw)
                continue
            if not isinstance(frame, dict) or "jsonrpc" not in frame:
                self.stray.append(raw)
                continue
            self.frames.append(frame)
            if frame.get("id") is None:
                continue
            with self._lock:
                slot = self._pending.pop(frame.get("id"), None)
            if slot is not None:
                slot["frame"] = frame
                slot["event"].set()

    def _read_err(self):
        for line in self.proc.stderr:
            self.stderr_lines.append(line.rstrip("\n"))
            del self.stderr_lines[:-200]

    def send(self, method, params=None, timeout=10.0):
        with self._lock:
            self._seq += 1
            mid = self._seq
            slot = {"event": threading.Event(), "frame": None}
            self._pending[mid] = slot
        line = json.dumps({"jsonrpc": "2.0", "id": mid, "method": method,
                           "params": params or {}}, ensure_ascii=False) + "\n"
        self.proc.stdin.write(line)
        self.proc.stdin.flush()
        if not slot["event"].wait(timeout):
            raise TimeoutError("%s 未在 %.1fs 内应答" % (method, timeout))
        return Response(slot["frame"])

    def notify(self, method, params=None):
        line = json.dumps({"jsonrpc": "2.0", "method": method,
                           "params": params or {}}, ensure_ascii=False) + "\n"
        self.proc.stdin.write(line)
        self.proc.stdin.flush()

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False
