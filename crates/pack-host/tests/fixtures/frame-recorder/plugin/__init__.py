"""夹具：把宿主发来的**每一行原始字节**落到 `$DAMPACK_WORK_DIR/frames.log`。

用途：`crates/pack-host/tests/protocol_frames.rs` 断言宿主侧的帧编码
（单行 JSON、`jsonrpc`、id 单调、cancel 是无 id 通知、params.jobId 透传）。

顺带模拟三种协议现场：
- **stdout 污染**：serve() 里往协议流写一行非 JSON（协议 §7 铁律 4：宿主记 stray_stdout，
  不得当应答）；
- **无 id 通知**：`{"jsonrpc":"2.0","method":"log",...}`（宿主忽略，不得当应答）；
- **读线程与工作线程分离**（协议 §9 铁律 3）：主循环只读 stdin，请求交给工作线程执行，
  否则 `slow` 执行期间既读不到 cancel、也记录不到取消帧。

方法表：
- `health` / `pack.info`：正常应答（握手用）
- `echo`：把 params 原样回送（验 params 透传）
- `slow`：睡 0.4s 再应答（验超时 → cancel 通知 → 宽限）；收到 cancel **不**收尾
  （模拟「收到 cancel 但没按 -32004 结束」的插件，宿主必须宽限后 kill）
- `ghost`：用 **mid+1000** 的 id 应答（验「id 不匹配的帧不被当成本次调用的结果」）
- `naked`：只回 `{"jsonrpc":"2.0","id":mid}`（既无 result 也无 error → 宿主必须报协议违例）
- `rpc_error`：回 -32002（验业务错误码原样上抛、不算 undetermined）
"""
import json
import os
import sys
import threading
import time

PACK_INFO = {
    "id": "test.frame-recorder",
    "name": "协议帧录制夹具",
    "version": "1.0.0",
    "api_version": "2.0",
    "outputs": ["verdict"],
    "rules": [{"id": "R_frame", "name": "帧编码", "deterministic": True}],
    "capabilities": ["io.read"],
    "deterministic": True,
    "timeout_secs": 5,
}

_STATE = {"frames": None, "lock": threading.Lock()}


def _record(line):
    """逐字记录宿主发来的一行（含行尾换行），落盘后马上 flush。"""
    frames = _STATE["frames"]
    if frames is not None:
        with _STATE["lock"]:
            frames.write(line)
            frames.flush()


def _handle(req, send):
    mid = req.get("id")
    method = req.get("method")
    params = req.get("params") or {}

    if method == "health":
        send({"jsonrpc": "2.0", "id": mid,
              "result": {"status": "ok", "pid": os.getpid(), "api_version": "2.0"}})
    elif method == "pack.info":
        send({"jsonrpc": "2.0", "id": mid, "result": dict(PACK_INFO)})
    elif method == "echo":
        send({"jsonrpc": "2.0", "id": mid, "result": {"echo": params}})
    elif method == "slow":
        # N19：真机超时的定位靠的就是包内阶段日志（宿主把它摘进超时错误）
        sys.stderr.write("[fixture] stage slow-sleep 0 ms\n")
        sys.stderr.flush()
        time.sleep(0.4)
        send({"jsonrpc": "2.0", "id": mid, "result": {"slept": 0.4}})
    elif method == "ghost":
        send({"jsonrpc": "2.0", "id": mid + 1000, "result": {"wrong_id": True}})
    elif method == "naked":
        send({"jsonrpc": "2.0", "id": mid})
    elif method == "rpc_error":
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32002, "message": "资产不可读（夹具）"}})
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found: %s" % method}})


def serve():
    raw_out = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")
    os.dup2(2, 1)                      # 之后任何 print 都落 stderr，不污染协议流
    sys.stdout = sys.stderr
    out_lock = threading.Lock()

    def send(obj):
        with out_lock:
            raw_out.write(json.dumps(obj, ensure_ascii=False) + "\n")
            raw_out.flush()

    # 进协议流的原始行（不排队打印）——宿主必须容错
    raw_out.write("boot: this line is not a frame\n")
    raw_out.write(json.dumps({"jsonrpc": "2.0", "method": "log",
                              "params": {"msg": "ready"}}, ensure_ascii=False) + "\n")
    raw_out.flush()

    work = os.environ.get("DAMPACK_WORK_DIR") or os.getcwd()
    try:
        os.makedirs(work, exist_ok=True)
    except OSError:
        pass
    _STATE["frames"] = open(
        os.path.join(work, "frames.log"), "a", encoding="utf-8", newline="\n"
    )

    try:
        for line in sys.stdin:
            _record(line)                  # 读线程：先记账，再分发
            stripped = line.strip()
            if not stripped:
                continue
            try:
                req = json.loads(stripped)
            except ValueError:
                send({"jsonrpc": "2.0", "id": None,
                      "error": {"code": -32700, "message": "parse error"}})
                continue
            if req.get("id") is None:
                # 宿主 → 插件 的通知（cancel 是唯一一种）：不回帧
                _record("# notification %s\n" % req.get("method"))
                continue
            threading.Thread(target=_handle, args=(req, send), daemon=True).start()
    finally:
        frames = _STATE["frames"]
        if frames is not None:
            with _STATE["lock"]:
                frames.close()
