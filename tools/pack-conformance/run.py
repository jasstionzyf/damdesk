#!/usr/bin/env python3
"""DAMDesk 行业包 conformance 套件 —— 12 条协议断言。

用法：
    python3 tools/pack-conformance/run.py --plugin tools/pack-conformance/fixtures/good

退出码：0 = 12/12 通过；1 = 至少一条断言失败（逐条打印违规点）。

本套件是**宿主行为的可执行规格**：假网关 + 真实子进程 + 真实 stdio 管道。
它验的是协议面（传输/方法/数据结构/错误码/取消/能力白名单/产物落盘），
不验任何算法正确性（假网关只回固定桩数据）。
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import protocol as proto          # noqa: E402
from gateway import FakeGateway   # noqa: E402

DEFAULT_HOST_TIMEOUT = 10.0
HEALTH_TIMEOUT = 2.0

# --------------------------------------------------------------------------
# 真网关适配层（K03 钩子）：把假网关换成宿主真实能力网关（Rust `pack-host::gateway`）。
#
# 只有**一处**接缝：`Session(gateway=...)` / `_REAL_GATEWAY`。真网关模式下：
#   - 网关由宿主进程起（`127.0.0.1:<随机端口>` + 一次性令牌），套件不启动/不停止它；
#   - 资产必须写成**真 PNG**（真网关要真解码，假网关只按文件名回桩）；
#   - 依赖桩约定的断言不适用，跑真网关时用 `--only` 排除：
#       07 需要「文件名含 vlundet 就回 undetermined」的桩语义（真网关取决于注入的 VL 后端）
#       08 需要「按收窄的 capabilities 重起网关」（外部网关无法中途收窄）
#       11 参考实现固定裁 [0,0,1000,1000]，600x600 资产在真网关会被越界校验拒（假网关不校验）
#       12 需要读网关侧 emit 账本（真网关的账本在宿主进程内存里，不对外暴露）
# --------------------------------------------------------------------------
class RemoteGateway:
    """外部真网关的薄代理：只提供 URL/令牌与空的内省接口。"""

    def __init__(self, base_url, token):
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.denied = []      # 真网关不对外暴露拒绝记录（08 因此不适用）
        self._emits = []

    def start(self):
        return self

    def stop(self):
        pass

    def hits(self, path=None):
        return []

    def emits_for(self, job_id):
        return [e for e in self._emits if e[0] == job_id]


# 真网关模式下 Session 的默认网关与工作目录（由 --gateway-url / --work-dir 设置）
_REAL_GATEWAY = None
_FIXED_WORK_DIR = None

_DIMS_RE = None


def _dims_of_name(name):
    """从资产文件名里取 `<W>x<H>`（与假网关同一约定）。"""
    global _DIMS_RE
    if _DIMS_RE is None:
        import re
        _DIMS_RE = re.compile(r"(\d{2,5})x(\d{2,5})")
    m = _DIMS_RE.search(os.path.basename(name or ""))
    return (int(m.group(1)), int(m.group(2))) if m else (1600, 1600)


def _write_real_png(path, w, h):
    """纯标准库写一张真 PNG（真网关要真解码；实心灰底即可，算法质量不在本套件范围）。"""
    import struct
    import zlib

    row = b"\x00" + bytes([200, 200, 200]) * w
    raw = row * h

    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    blob = b"\x89PNG\r\n\x1a\n"
    blob += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    blob += chunk(b"IDAT", zlib.compress(raw, 6))
    blob += chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(blob)


# --------------------------------------------------------------------------
# 宿主侧会话：拉起插件子进程 + 假网关 + 行式 JSON-RPC 客户端
# --------------------------------------------------------------------------
class RequestTimeout(Exception):
    pass


class Response:
    def __init__(self, frame):
        self.frame = frame
        self.result = frame.get("result")
        self.error = frame.get("error")

    @property
    def code(self):
        return (self.error or {}).get("code")

    @property
    def message(self):
        return (self.error or {}).get("message", "")

    def __repr__(self):
        if self.error:
            return "err(%s,%s)" % (self.code, self.message)
        return "ok"


class Session:
    def __init__(self, pack_dir, manifest_override=None, host_timeout=DEFAULT_HOST_TIMEOUT,
                 gateway=None):
        self.pack_dir = os.path.abspath(pack_dir)
        with open(os.path.join(self.pack_dir, "manifest.json"), encoding="utf-8") as fh:
            self.manifest = json.load(fh)
        if manifest_override:
            self.manifest = {**self.manifest, **manifest_override}
        self.host_timeout = host_timeout

        gateway = gateway or _REAL_GATEWAY
        self.real_gateway = gateway is not None
        if _FIXED_WORK_DIR:
            self.tmpdir = _FIXED_WORK_DIR
            os.makedirs(self.tmpdir, exist_ok=True)
        else:
            self.tmpdir = tempfile.mkdtemp(prefix="pack-conf-")
        self.assets_dir = os.path.join(self.tmpdir, "assets")
        os.makedirs(self.assets_dir, exist_ok=True)

        self._owns_gateway = gateway is None
        if gateway is None:
            gateway = FakeGateway(self.manifest.get("capabilities") or [],
                                  workroot=self.tmpdir)
        self.gateway = gateway.start()

        cmd = self._resolve_entrypoint()
        env = os.environ.copy()
        env.update({
            "DAMPACK_GATEWAY_URL": self.gateway.base_url,
            "DAMPACK_GATEWAY_TOKEN": self.gateway.token,
            "DAMPACK_PACK_DIR": self.pack_dir,
            "DAMPACK_WORK_DIR": self.tmpdir,
            "DAMPACK_LOG_LEVEL": "info",
            "PYTHONUNBUFFERED": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        })
        env.pop("PYTHONPATH", None)

        self.proc = subprocess.Popen(
            cmd, cwd=self.pack_dir, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace", bufsize=1,
        )
        self.stdout_lines = []
        self.stray_lines = []      # 非协议输出（污染）
        self.notifications = []
        self.stderr_lines = []
        self.timeouts = []
        self._seq = 0
        self._pending = {}
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._err_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self._reader.start()
        self._err_reader.start()

    # --- 子进程 ---------------------------------------------------------
    def _resolve_entrypoint(self):
        entry = self.manifest.get("entrypoint") or {}
        cmd = entry.get("cmd") or "python3"
        if cmd in ("python3", "python"):
            cmd = sys.executable
        args = [a if os.path.isabs(a) else os.path.join(self.pack_dir, a)
                for a in (entry.get("args") or [])]
        return [cmd] + args

    def _read_stdout(self):
        for line in self.proc.stdout:
            raw = line.rstrip("\n")
            if not raw.strip():
                continue
            self.stdout_lines.append(raw)
            try:
                frame = json.loads(raw)
            except Exception:
                self.stray_lines.append(raw)
                continue
            if not isinstance(frame, dict) or "jsonrpc" not in frame:
                self.stray_lines.append(raw)
                continue
            mid = frame.get("id")
            if mid is None:
                self.notifications.append(frame)
                continue
            with self._lock:
                fut = self._pending.pop(mid, None)
            if fut is not None:
                fut["response"] = frame
                fut["event"].set()

    def _read_stderr(self):
        for line in self.proc.stderr:
            self.stderr_lines.append(line.rstrip("\n"))
            del self.stderr_lines[:-200]

    def _write(self, obj):
        line = json.dumps(obj, ensure_ascii=False)
        with self._write_lock:
            try:
                self.proc.stdin.write(line + "\n")
                self.proc.stdin.flush()
            except (BrokenPipeError, ValueError):
                pass

    # --- 请求 -----------------------------------------------------------
    def request(self, method, params=None, timeout=None):
        timeout = self.host_timeout if timeout is None else timeout
        with self._lock:
            self._seq += 1
            mid = self._seq
            fut = {"event": threading.Event(), "response": None}
            self._pending[mid] = fut
        self._write({"jsonrpc": "2.0", "id": mid, "method": method,
                     "params": params or {}})
        if fut["event"].wait(timeout):
            return Response(fut["response"])

        with self._lock:
            self._pending.pop(mid, None)
        self.timeouts.append((method, timeout))
        # 宿主超时铁律：cancel → 杀进程 → 结果记 undetermined(plugin_timeout)。
        job_id = params.get("job_id", "") if isinstance(params, dict) else ""
        self._write({"jsonrpc": "2.0", "method": "cancel", "params": {"job_id": job_id}})
        raise RequestTimeout("`%s` 未在 %.1fs 内应答" % (method, timeout))

    def notify(self, method, params=None):
        self._write({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def asset(self, name):
        path = os.path.join(self.assets_dir, name)
        if self.real_gateway:
            # 真网关要真解码：按文件名约定写等尺寸的真 PNG
            w, h = _dims_of_name(name)
            _write_real_png(path, w, h)
        else:
            with open(path, "wb") as fh:
                fh.write(b"\x89PNG\r\n\x1a\nconf-asset")
        return {"id": name, "path": path, "mime": "image/png"}

    def stderr_tail(self, n=3):
        return self.stderr_lines[-n:]

    # --- 生命周期 -------------------------------------------------------
    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=3)
        except Exception:
            self.proc.kill()
            try:
                self.proc.wait(timeout=3)
            except Exception:
                pass
        if self._owns_gateway:
            self.gateway.stop()
        if not _FIXED_WORK_DIR:
            shutil.rmtree(self.tmpdir, ignore_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


# --------------------------------------------------------------------------
# 12 条断言
# --------------------------------------------------------------------------
CHECKS = []


# --------------------------------------------------------------------------
# 规则集解析（K04 起泛化）
#
# 断言原来把参考实现的 rule id 写死（A2_min_short_side / A4_subject_ratio /
# A5_overlay_text / A6_category），于是**只有** fixtures/good 及其违规变体能跑——
# 任何语义不同名的真包（如官方电商包 R0..R5）都会因为「未知规则 → skipped」
# 在 06/07/09/10/11/12 上被动红。K04 的验收要求官方包 12/12，所以这里改成：
#   ① 规则集来源优先级：`manifest.rules`（可选扩展字段）→ 启动时探测 `pack.info`
#      → 最后才回落参考实现的 A* 名（历史行为不变，run_all.sh 四个变体照旧）；
#   ② 07/08/09/10/12 一律**请求全部规则**（rules=None），不再点名某条；
#      07 的判据改成「网关确实收到 /vl/judge 且确有 verdict 落 undetermined + 写 note」，
#      11 直接让包按自己的修复策略（rules=None）挑可修规则。
# 协议面（传输/方法/数据结构/错误码/取消/能力白名单/产物落盘）断言一条没松。
# --------------------------------------------------------------------------
REFERENCE_RULES = ["A2_min_short_side", "A4_subject_ratio",
                   "A5_overlay_text", "A6_category"]
_PACK_RULES = []


def probe_pack_rules(pack_dir, manifest):
    """规则 id 列表：manifest.rules → pack.info.rules → 参考实现的 A* 名。"""
    discovered = [r.get("id") for r in (manifest.get("rules") or [])
                  if isinstance(r, dict) and r.get("id")]
    if not discovered:
        try:
            with Session(pack_dir) as sess:
                resp = sess.request("pack.info", timeout=5.0)
                if resp.error is None:
                    discovered = [r.get("id")
                                  for r in ((resp.result or {}).get("rules") or [])
                                  if isinstance(r, dict) and r.get("id")]
        except Exception:
            discovered = []
    return list(discovered) or list(REFERENCE_RULES)


def pack_rules():
    return list(_PACK_RULES) or list(REFERENCE_RULES)



def check(cid, title):
    def deco(fn):
        CHECKS.append((cid, title, fn))
        return fn
    return deco


def _fail(*parts):
    return False, "；".join(str(p) for p in parts if p)


def _judge_skip(ctx):
    """处理类包（`kinds` 不含 `judge`）没有判定应答面：判定类断言**如实跳过**，不冒充通过。

    `kinds` 缺省 = 判定类（历史包全是判定包），所以老包行为不变。
    """
    kinds = tuple(ctx.manifest.get("kinds") or ("judge",))
    if "judge" in kinds:
        return None
    return True, "跳过：本包 kinds=%s（处理类无判定面）" % (list(kinds),)



@check("01", "manifest 合法")
def check_manifest(ctx):
    path = os.path.join(ctx.pack_dir, "manifest.json")
    if not os.path.isfile(path):
        return _fail("缺 manifest.json")
    try:
        manifest = json.load(open(path, encoding="utf-8"))
    except Exception as exc:
        return _fail("manifest.json 不是合法 JSON：%r" % (exc,))
    problems = proto.validate_manifest(manifest)
    if problems:
        return _fail(*problems)
    entry = manifest["entrypoint"]
    missing = [a for a in entry.get("args") or []
               if not os.path.isabs(a) and not os.path.exists(os.path.join(ctx.pack_dir, a))]
    if missing:
        return _fail("entrypoint.args 指向不存在的文件 %s" % missing)
    return True, "id=%s version=%s api=%s caps=%s" % (
        manifest["id"], manifest["version"], manifest["api_version"],
        ",".join(manifest["capabilities"]))


def session_timeout_for_judge(pack_dir):
    """check 02/10 之外的 judge 等待上限：取「包自己声明的 timeout_secs」与默认值的较大者。

    check 02 判的是 **stdout 纯净性**（协议行之外有没有日志），不是延迟 —— 用 10s 硬等
    当判据会在真网关模式下假红：真网关 judge 要跑真 u2net/OCR（fixture 声明
    `timeout_secs: 60`），宿主并行跑多个 e2e 用例时实测 **>10s**（K07-FIX-G-B：
    `judge` 未在 10.0s 内应答 ×2，同用例单独跑 3/3 绿）。等待上限对齐声明预算即可，
    判据本身不变；上限仍夹在 120s 内，避免挂着不动的插件拖死门禁。
    """
    declared = 0.0
    try:
        with open(os.path.join(pack_dir, "manifest.json"), encoding="utf-8") as fh:
            declared = float(json.load(fh).get("timeout_secs") or 0)
    except (OSError, ValueError, TypeError):
        declared = 0.0
    return max(DEFAULT_HOST_TIMEOUT, min(declared or DEFAULT_HOST_TIMEOUT, 120.0))


@check("02", "stdout 只走协议行")
def check_stdout_purity(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    try:
        with Session(ctx.pack_dir, host_timeout=session_timeout_for_judge(ctx.pack_dir)) as sess:
            sess.request("pack.info")
            sess.request("judge", {"job_id": "j-stdout",
                                   "asset": sess.asset("main_fg0.05_1600x1600.png"),
                                   "rules": None})
            time.sleep(0.05)
            stray = list(sess.stray_lines)
            total = len(sess.stdout_lines)
    except RequestTimeout as exc:
        return _fail("会话未完成，无法判定 stdout 纯净性：%s" % exc)
    if stray:
        return _fail("stdout 有 %d/%d 行非协议输出（日志必须走 stderr）：%r"
                     % (len(stray), total, stray[:3]))
    return True, "%d 行输出全部是合法 JSON-RPC 帧" % total


@check("03", "pack.info 与 manifest 一致")
def check_pack_info(ctx):
    with Session(ctx.pack_dir) as sess:
        resp = sess.request("pack.info")
        if resp.error:
            return _fail("pack.info 返回错误 %s：%s" % (resp.code, resp.message),
                         "stderr 尾部：%r" % (sess.stderr_tail(),))
        info = resp.result
        manifest = sess.manifest
    if not isinstance(info, dict):
        return _fail("result 不是对象：%r" % (info,))
    missing = [f for f in ("id", "name", "version", "api_version", "rules",
                           "capabilities", "deterministic", "timeout_secs")
               if f not in info]
    if missing:
        return _fail("PackInfo 缺字段 %s" % missing)
    problems = []
    for field in ("id", "version", "api_version"):
        if info[field] != manifest[field]:
            problems.append("%s 不一致：info=%r manifest=%r"
                            % (field, info[field], manifest[field]))
    if set(info["capabilities"]) != set(manifest["capabilities"]):
        problems.append("capabilities 不一致：info=%s manifest=%s"
                        % (info["capabilities"], manifest["capabilities"]))
    if info["deterministic"] != manifest["deterministic"]:
        problems.append("deterministic 不一致")
    if info["timeout_secs"] != manifest["timeout_secs"]:
        problems.append("timeout_secs 不一致")
    rules = info["rules"]
    if not isinstance(rules, list):
        problems.append("rules 必须是数组（判定类非空；处理类可为空）")
    elif not rules and "judge" in tuple(manifest.get("kinds") or ("judge",)):
        # N5a：`kinds:["process"]` 的包**没有规则面**（唯一动作是处理），
        # 旧断言「rules 必须非空」会把合法的处理类包判红；判定类包仍严格要求非空。
        problems.append("rules 必须是非空数组")
    else:
        for rule in rules:
            if not isinstance(rule, dict) or not str(rule.get("id") or "").strip():
                problems.append("rules 每项必须有 id：%r" % (rule,))
                break
    if problems:
        return _fail(*problems)
    return True, "id=%s rules=%d caps=%d" % (
        info["id"], len(rules), len(info["capabilities"]))


@check("04", "health 握手 ≤2s")
def check_health(ctx):
    with Session(ctx.pack_dir) as sess:
        try:
            resp = sess.request("health", timeout=HEALTH_TIMEOUT)
        except RequestTimeout as exc:
            return _fail("health 未在 %.1fs 内应答（宿主必须能完成懒启动握手）"
                         % HEALTH_TIMEOUT,
                         "stderr 尾部：%r" % (sess.stderr_tail(),))
        if resp.error:
            return _fail("health 返回错误 %s：%s" % (resp.code, resp.message))
        result = resp.result or {}
    if result.get("status") != "ok":
        return _fail("health.result.status=%r（应为 'ok'）" % (result.get("status"),))
    return True, "status=ok pid=%s" % (result.get("pid"),)


@check("05", "六个方法齐全（report/cancel 不得缺）")
def check_methods_complete(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    problems = []
    with Session(ctx.pack_dir) as sess:
        rep = sess.request("report", {"job_id": "j-methods"})
        cxl = sess.request("cancel", {"job_id": "j-methods"})
    if rep.error:
        problems.append("report → %s %s%s" % (
            rep.code, rep.message,
            "（缺方法，未实现）" if rep.code == proto.ERR_METHOD_NOT_FOUND else ""))
    elif not isinstance(rep.result, dict):
        problems.append("report.result 不是对象")
    if cxl.error:
        problems.append("cancel → %s %s%s" % (
            cxl.code, cxl.message,
            "（缺方法，未实现）" if cxl.code == proto.ERR_METHOD_NOT_FOUND else ""))
    elif cxl.result.get("canceled") is not True:
        problems.append("cancel.result.canceled=%r（应为 true）" % (cxl.result.get("canceled"),))
    if problems:
        return _fail(*problems)
    return True, "report/cancel 均可用"


@check("06", "Verdict 七字段 + status 闭集")
def check_verdict_shape(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    with Session(ctx.pack_dir) as sess:
        rules = [r["id"] for r in sess.manifest.get("rules", [])] or None
        rules = rules or pack_rules()
        resp = sess.request("judge", {"job_id": "j-shape",
                                      "asset": sess.asset("small_600x600_fg0.30.png"),
                                      "rules": rules})
        if resp.error:
            return _fail("judge 返回错误 %s：%s" % (resp.code, resp.message))
        verdicts = (resp.result or {}).get("verdicts")
        pack_reply = dict(resp.result or {})
    kinds = tuple(ctx.manifest.get("kinds") or ("judge",))
    if not isinstance(verdicts, list) or not verdicts:
        return _fail("judge.result.verdicts 必须是非空数组：%r" % (verdicts,))
    problems = []
    for verdict in verdicts:
        for problem in proto.validate_verdict(verdict):
            problems.append("%s: %s" % (verdict.get("ruleId", "?"), problem))
    got = {v.get("ruleId") for v in verdicts if isinstance(v, dict)}
    if got != set(rules):
        problems.append("返回的 ruleId 与请求不一致：请求 %s 实得 %s"
                        % (sorted(rules), sorted(got)))
    # N5a：判定应答必须能归一化成合法结果行（`result=ok` + `verdict` 三态）
    _, row_problems = proto.rows_from_reply(pack_reply, kinds)
    if row_problems:
        problems.append("结果行契约（协议 §4.4）：%s" % row_problems[:4])
    if problems:
        return _fail(*problems[:6])
    return True, "%d 条 verdict 全部合法（statuses=%s），结果行归一无违例" % (
        len(verdicts), [v["status"] for v in verdicts])


@check("07", "VL 不可判 → undetermined（禁止冒充 pass/fail）")
def check_undetermined_not_promoted(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    caps = set(ctx.manifest.get("capabilities") or [])
    if "vl.judge" not in caps:
        return True, "跳过：本包未声明 vl.judge"
    with Session(ctx.pack_dir) as sess:
        resp = sess.request("judge", {"job_id": "j-undet",
                                      "asset": sess.asset("vlundet_fg0.05_1600x1600.png"),
                                      "rules": None})
        if resp.error:
            return _fail("judge 返回错误 %s：%s" % (resp.code, resp.message))
        verdicts = (resp.result or {}).get("verdicts") or []
        # 真网关模式（--gateway-url）下插件直连外部网关，套件没有内省通道
        # （RemoteGateway.hits() 恒空），所以「有没有真的调 /vl/judge」这条在这里
        # 不可观测——不观测不等于可以少判：undetermined/note 两条照旧断言，
        # 命中计数由 Rust 侧 e2e（注入桩 VL 的真网关）负责，见
        # crates/pack-host/tests/gateway_pack_e2e.rs。
        observable = not getattr(sess, "real_gateway", False)
        vl_hits = [h for h in sess.gateway.hits("/vl/judge") if h.get("status") == 200]
    if not verdicts:
        return _fail("未返回 verdict")
    if observable and not vl_hits:
        return _fail("网关没收到任何 /vl/judge 调用：要么插件没用 VL，"
                     "要么它伪造了结论（本次请求的是全部规则）")
    undetermined = [v for v in verdicts if v.get("status") == "undetermined"]
    if not undetermined:
        return _fail("VL 回 undetermined 后没有任何 verdict 落 undetermined"
                     "（必须落灰色待人工，不得冒充 pass/fail）：statuses=%s"
                     % [(v.get("ruleId"), v.get("status")) for v in verdicts])
    silent = [v.get("ruleId") for v in undetermined
              if not str(v.get("note") or "").strip()]
    if silent:
        return _fail("undetermined 必须写 note 说明原因（宿主据此分流人工）：%s" % silent)
    if not observable:
        return True, "undetermined=%d（note 齐全）；真网关模式：VL 命中不可内省" % len(undetermined)
    return True, "vl_hits=%d undetermined=%d（note 齐全）" % (
        len(vl_hits), len(undetermined))


@check("08", "能力白名单：未声明能力必须 -32001")
def check_capability_whitelist(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    caps = list(ctx.manifest.get("capabilities") or [])
    if "vl.judge" not in caps:
        return True, "跳过：本包未声明 vl.judge"
    narrowed = [c for c in caps if c != "vl.judge"]
    with Session(ctx.pack_dir, manifest_override={"capabilities": narrowed}) as sess:
        resp = sess.request("judge", {"job_id": "j-caps",
                                      "asset": sess.asset("main_fg0.05_1600x1600.png"),
                                      "rules": None})
        denied = list(sess.gateway.denied)
        sneaked = [r["path"] for r in sess.gateway.hits()
                   if not r["allowed"] and r["status"] == 200]
    if sneaked:
        return _fail("插件绕过了能力白名单：%s 返回 200" % sneaked)
    if not denied:
        return _fail("网关未记录到 403（插件没走 /vl/judge，或伪造了结果）")
    if resp.code != proto.ERR_CAPABILITY_MISSING:
        return _fail("能力缺失应回 %d，实得 %s %s（网关已 403 %s）"
                     % (proto.ERR_CAPABILITY_MISSING, resp.code, resp.message, denied))
    return True, "403 → %d，denied=%s" % (proto.ERR_CAPABILITY_MISSING, denied)


@check("09", "错误码用法")
def check_error_codes(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    problems = []
    with Session(ctx.pack_dir) as sess:
        r1 = sess.request("judge", {"job_id": "j-e1", "asset": {}, "rules": None})
        if r1.code != proto.ERR_INVALID_PARAMS:
            problems.append("缺 asset.path 应回 %d（参数非法），实得 %s"
                            % (proto.ERR_INVALID_PARAMS, r1.code))
        r2 = sess.request("judge", {"job_id": "j-e2",
                                    "asset": {"id": "gone", "path": "/nonexistent/never.png"},
                                    "rules": None})
        if r2.code != proto.ERR_ASSET_UNREADABLE:
            problems.append("资产不可读应回 %d，实得 %s %s"
                            % (proto.ERR_ASSET_UNREADABLE, r2.code, r2.message))
        r3 = sess.request("no.such.method", {})
        if r3.code != proto.ERR_METHOD_NOT_FOUND:
            problems.append("未知方法应回 %d，实得 %s"
                            % (proto.ERR_METHOD_NOT_FOUND, r3.code))
        seen = {r.code for r in (r1, r2, r3) if r.code is not None}
        illegal = sorted(c for c in seen if c not in proto.PLUGIN_ERROR_CODES)
    if illegal:
        problems.append("出现协议外自造错误码 %s" % illegal)
    if problems:
        return _fail(*problems)
    return True, "参数=%d 资产=%d 未知方法=%d" % (
        r1.code, r2.code, r3.code)


@check("10", "cancel：judge 期间可取消且以 -32004 结束")
def check_cancel_semantics(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    with Session(ctx.pack_dir, host_timeout=20.0) as sess:
        job = "j-cancel"
        asset = sess.asset("slow_1600x1600.png")
        box = {}

        def run():
            try:
                box["resp"] = sess.request("judge", {"job_id": job, "asset": asset,
                                                     "rules": None})
            except RequestTimeout as exc:
                box["timeout"] = str(exc)

        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        time.sleep(0.4)
        cxl = sess.request("cancel", {"job_id": job}, timeout=5.0)
        worker.join(timeout=8.0)
        emits = sess.gateway.emits_for(job)

    if "timeout" in box:
        return _fail("cancel 后 judge 仍未结束：%s" % box["timeout"])
    if "resp" not in box:
        return _fail("judge 未返回（8s 内）")
    if cxl.code is not None:
        return _fail("cancel 本身出错：%s %s" % (cxl.code, cxl.message))
    resp = box["resp"]
    if resp.code != proto.ERR_CANCELED:
        return _fail("取消后 judge 应回 %d，实得 %s %s（status=%r）"
                     % (proto.ERR_CANCELED, resp.code, resp.message,
                        (resp.result or {}).get("verdicts")))
    if emits:
        return _fail("取消后仍向 /report/emit 上报了 %d 条" % len(emits))
    return True, "judge → %d，取消后 0 条 emit" % proto.ERR_CANCELED


@check("11", "repair 产物落盘且与 verdict 对账")
def check_repair_artifacts(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    caps = set(ctx.manifest.get("capabilities") or [])
    if "op.geometric" not in caps:
        return True, "跳过：本包未声明 op.geometric"
    with Session(ctx.pack_dir) as sess:
        workdir = os.path.join(sess.tmpdir, "repair")
        resp = sess.request("repair", {"job_id": "j-repair",
                                       "asset": sess.asset("small_600x600_fg0.30.png"),
                                       "rules": None,
                                       "options": {"workdir": workdir}})
        if resp.error:
            return _fail("repair 返回错误 %s：%s" % (resp.code, resp.message))
        result = resp.result or {}
        artifacts = result.get("artifacts") or []
        verdicts = result.get("verdicts") or []
        problems = []
        if not artifacts:
            problems.append("artifacts 为空（修复必须给出产物路径）")
        for art in artifacts:
            path = art.get("path")
            if not path or not os.path.isfile(path):
                problems.append("产物不存在：%r" % (path,))
                continue
            if os.path.getsize(path) <= 0:
                problems.append("产物为空文件：%s" % path)
            for field in ("rule_id", "width", "height", "kind"):
                if field not in art:
                    problems.append("artifact 缺字段 %s：%r" % (field, art))
                    break
        fixed_rules = {v.get("ruleId") for v in verdicts if v.get("fixed") is True}
        missing = [a.get("rule_id") for a in artifacts
                   if a.get("rule_id") not in fixed_rules]
        if missing:
            problems.append("产物对应的 verdict 未标 fixed=true：%s" % missing)
    if problems:
        return _fail(*problems[:6])
    return True, "artifacts=%d verdicts=%d 全部落盘" % (len(artifacts), len(verdicts))


@check("12", "job 归账隔离")
def check_job_isolation(ctx):
    skip = _judge_skip(ctx)
    if skip:
        return skip
    with Session(ctx.pack_dir) as sess:
        a = sess.asset("main_fg0.05_1600x1600.png")
        b = sess.asset("small_600x600_fg0.30.png")
        sess.request("judge", {"job_id": "job-A", "asset": a, "rules": None})
        sess.request("judge", {"job_id": "job-B", "asset": b, "rules": None})
        rep_a = sess.request("report", {"job_id": "job-A"})
        rep_b = sess.request("report", {"job_id": "job-B"})
        emits_a = [e for e in sess.gateway.emits_for("job-A")]
        emits_b = [e for e in sess.gateway.emits_for("job-B")]
    problems = []
    for name, resp, job in (("report(job-A)", rep_a, "job-A"),
                            ("report(job-B)", rep_b, "job-B")):
        if resp.error:
            problems.append("%s 返回错误 %s" % (name, resp.code))
            continue
        assets = (resp.result or {}).get("assets")
        if not isinstance(assets, list):
            problems.append("%s 缺 assets 字段" % name)
            continue
        strangers = [x for x in assets if x and not str(x).startswith(job.split("-")[1].lower())]
        if strangers and set(assets) == {"main_1600x1600.png", "small_600x600_fg0.30.png"}:
            problems.append("%s 串账：assets=%s" % (name, assets))
    if emits_a and emits_b:
        a_entry = emits_a[0][1] or {}
        b_entry = emits_b[0][1] or {}
        if (a_entry.get("asset_id") == b_entry.get("asset_id")):
            problems.append("两个 job 的 emit 指向同一资产：%r" % (a_entry.get("asset_id"),))
    if not emits_a or not emits_b:
        problems.append("emit 未按 job_id 分开上报：A=%d B=%d" % (len(emits_a), len(emits_b)))
    if problems:
        return _fail(*problems)
    return True, "emit A=%d B=%d 未串账" % (len(emits_a), len(emits_b))


@check("13", "i18n 字段：字符串/对象两形态可解析")
def check_i18n_shapes(ctx):
    """i18n-F（可选多语言字段，不升 api_version）：

    manifest `name`/`short_name`/`summary`（及可选 `rules[].name`）既可以是字符串
    （全语言通用，唯一向后兼容形态），也可以是 `{"locale": 文案}` 对象。
    这里校验**被测包自己写的形态**都合法；再用一份对象形态的合成 manifest 过一遍
    `validate_manifest`，证明协议规格接受新形态（老包字符串形态在 01 已覆盖）。
    """
    manifest = ctx.manifest
    problems = []
    seen = []
    for field in ("name", "short_name", "summary"):
        if field in manifest:
            seen.append(field)
            proto._check_i18n_value(field, manifest[field], problems)
    for r in (manifest.get("rules") or []):
        if isinstance(r, dict) and "name" in r:
            seen.append("rules[%s].name" % r.get("id", "?"))
            proto._check_i18n_value("rules[%s].name" % r.get("id", "?"), r["name"], problems)
    if problems:
        return _fail(*problems)
    # 对象形态端到端：合成 manifest 必须通过整份 manifest 校验（宿主拒装口径同源）
    synthetic = json.loads(json.dumps(manifest))
    synthetic["name"] = {"zh-Hans": "合成参考包", "en-US": "Synthetic Reference Pack"}
    synthetic["short_name"] = {"zh-Hans": "合成", "en-US": "Synthetic"}
    synthetic["summary"] = {"zh-Hans": "一句话描述。", "en-US": "One-line summary."}
    problems = proto.validate_manifest(synthetic)
    if problems:
        return _fail("对象形态 manifest 未通过 validate_manifest：%s" % problems)
    return True, "字段 %s 形态合法；对象形态 manifest 可解析" % (seen or ["name（字符串形态）"])


@check("14", "i18n 取值回退：locale → zh-Hans → 任意 → 空串")
def check_i18n_fallback(ctx):
    """宿主取值口径（`pick_i18n`，与 Rust `LocalizedText::pick` 同链）的契约断言。

    四条必须同时成立：命中当前语言 / 缺当前语言回退 zh-Hans / 缺 zh-Hans 回退任意一条 /
    空对象回空串（不崩）。字符串形态原样返回（老包零影响，回归保护）。
    """
    cases = [
        ({"zh-Hans": "白底图纯白", "en-US": "Pure white"}, "en-US", "Pure white", "命中当前语言"),
        ({"zh-Hans": "白底图纯白"}, "en-US", "白底图纯白", "缺当前语言回退 zh-Hans"),
        ({"en-US": "Only English"}, "zh-Hans", "Only English", "缺 zh-Hans 回退任意一条"),
        ({}, "en-US", "", "空对象回空串"),
        ("纯文本兜底", "en-US", "纯文本兜底", "字符串形态全语言通用"),
    ]
    for value, locale, want, why in cases:
        got = proto.pick_i18n(value, locale)
        if got != want:
            return _fail("回退链断在「%s」：value=%r locale=%s 期望 %r 实得 %r"
                         % (why, value, locale, want, got))
    return True, "5 条回退链用例全过（含字符串形态回归）"


# --------------------------------------------------------------------------
def main(argv=None):
    global _REAL_GATEWAY, _FIXED_WORK_DIR, _PACK_RULES
    parser = argparse.ArgumentParser(description="DAMDesk 行业包 conformance 套件")
    parser.add_argument("--plugin", required=True, help="包目录（含 manifest.json）")
    parser.add_argument("--json-out", help="把结果写成 JSON 落盘")
    parser.add_argument("--only", help="只跑指定编号，逗号分隔（如 02,07）")
    parser.add_argument("--gateway-url", help="改用宿主真网关（http://127.0.0.1:<port>）而不是假网关")
    parser.add_argument("--gateway-token", help="真网关的一次性 Bearer 令牌")
    parser.add_argument("--work-dir", help="固定工作目录（真网关模式下必须与网关写根一致）")
    args = parser.parse_args(argv)

    if args.gateway_url:
        if not args.gateway_token:
            print("ERROR: --gateway-url 必须配 --gateway-token")
            return 2
        if not args.work_dir:
            print("ERROR: 真网关模式必须给 --work-dir（= 网关写根，产物要在它之下）")
            return 2
        _REAL_GATEWAY = RemoteGateway(args.gateway_url, args.gateway_token)
        _FIXED_WORK_DIR = os.path.abspath(args.work_dir)
        os.makedirs(_FIXED_WORK_DIR, exist_ok=True)

    pack_dir = os.path.abspath(args.plugin)
    if not os.path.isfile(os.path.join(pack_dir, "manifest.json")):
        print("ERROR: %s 下没有 manifest.json" % pack_dir)
        return 2

    try:
        manifest = json.load(open(os.path.join(pack_dir, "manifest.json"), encoding="utf-8"))
    except Exception as exc:
        print("ERROR: manifest.json 解析失败：%r" % (exc,))
        return 2

    # 规则集探测（泛化，见「规则集解析」节）：探测失败不影响断言，回落参考实现名
    _PACK_RULES[:] = probe_pack_rules(pack_dir, manifest)

    ctx = type("Ctx", (), {"pack_dir": pack_dir, "manifest": manifest,
                           "rules": list(_PACK_RULES)})()
    selected = [c for c in CHECKS
                if not args.only or c[0] in {x.strip() for x in args.only.split(",")}]

    print("== DAMDesk pack conformance ==  pack=%s id=%s version=%s"
          % (pack_dir, manifest.get("id"), manifest.get("version")))
    results = []
    started = time.time()
    for cid, title, fn in selected:
        t0 = time.time()
        try:
            ok, detail = fn(ctx)
        except Exception as exc:  # 断言自身异常也算失败，绝不吞
            ok, detail = False, "断言自身异常：%r" % (exc,)
        elapsed = time.time() - t0
        print("[%s] %s %-28s %5.2fs  %s" % ("PASS" if ok else "FAIL", cid, title,
                                            elapsed, detail))
        results.append({"check": cid, "title": title, "ok": bool(ok),
                        "detail": detail, "elapsed_s": round(elapsed, 3)})

    passed = sum(1 for r in results if r["ok"])
    total = len(results)
    print("\n%d/%d PASS  (%.2fs)" % (passed, total, time.time() - started))
    if passed != total:
        print("FAILED: " + ", ".join(r["check"] for r in results if not r["ok"]))

    if args.json_out:
        payload = {"pack": pack_dir, "manifest_id": manifest.get("id"),
                   "manifest_version": manifest.get("version"),
                   "passed": passed, "total": total, "results": results}
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=2)
        print("json → %s" % args.json_out)

    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
