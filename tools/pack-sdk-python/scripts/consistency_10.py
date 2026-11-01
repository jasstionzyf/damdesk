#!/usr/bin/env python3
"""10 张一致性对账：SDK 版电商包 vs 旧引擎（Rust）K00 基线。

做什么
    取 `reports/t_7aba77f3-baseline.json`（K00 基线：10 资产 × 6 规则 = 60 行，
    `vl:deepseek` 通道、首判口径），把**同一批测量值**回放给 SDK 版样例包
    （`tools/pack-sdk-python/examples/ecommerce_py`）走完整的判链路，
    再逐图逐规则比对 `ruleId + status + score`。

    · 被测对象是**真的包子进程**（stdio JSON-RPC），不是 import 规则函数——
      协议壳、SDK 的 verdict 构造、短路语义都在被测范围内；
    · 网关输入是**回放**的（`/io/read` 的尺寸由 R1 首判分数反推、`/detect/u2net`
      的 fg_ratio 取 R4 首判分数、R2 走 classic 的 white_dev、VL 结论取基线 status）。

口径（别误读结果）
    1. 确定性规则 R1/R2/R4：**度量值**必须逐值一致（容差 1e-6），status 必须一致；
       `score` 是派生值，按包内定义投影后比较（R1：短边 px → short/门槛，见 `project_engine_score`）。
    2. VL 规则 R0/R3：`status` 单列比对，且**来源是基线回放**（不是本次实测）；
       `--vl-live` 时改成真调 deepseek-flash（此时可能翻车，属已知非确定性，不作门禁）。
    3. R5_attribute：旧引擎 `on_fail: pass` 把它算 pass，插件按裁决只出词表（skipped），
       是**口径差不是缺陷**，单列说明、不参与等式。
    4. 端到端（真 u2net/VL 网关 + 真语料全链路）**未在此验证** —— 那是 K07 等价门禁的活。

用法
    python3 tools/pack-sdk-python/scripts/consistency_10.py \
        --baseline reports/t_7aba77f3-baseline.json \
        --corpus <语料根> \
        --json-out /tmp/consistency.json
    # 追加真 VL（deepseek-flash，key 只从 env 读）：
    DEEPSEEK_API_KEY=sk-... python3 .../consistency_10.py --vl-live

退出码：0 确定性规则全一致；1 有不一致；2 基线/包缺失；3 --vl-live 但 VL 不可达。
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import shutil
import struct
import sys
import tempfile
import threading
import time
import traceback
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
SDK_DIR = os.path.dirname(HERE)
REPO_ROOT = os.path.abspath(os.path.join(SDK_DIR, "..", ".."))
EXAMPLE_PACK = os.path.join(SDK_DIR, "examples", "ecommerce_py")
sys.path.insert(0, os.path.join(SDK_DIR, "tests"))     # 复用 _support 的子进程客户端

DETERMINISTIC = ("R1_size", "R2_white_bg", "R4_subject_ratio")
VL_RULES = ("R0_product_main", "R3_no_watermark")
NOT_JUDGED = ("R5_attribute",)
TOLERANCE = 1e-6
VL_BASE = "https://api.deepseek.com/v1"
VL_MODEL = "deepseek-flash"
VL_MAX_TOKENS = 4096


# ---------------------------------------------------------------------------
# 回放网关：把 K00 基线的测量值/结论按资产回给插件
# ---------------------------------------------------------------------------
class ReplayGateway:
    """`/io/read` + `/detect/u2net` + `/detect/ocr` + `/vl/judge` + `/report/emit`。

    `rows[asset_key]` = {ruleId: row}；网关按 prompt 文本识别插件问的是哪条规则
    （prompt 常量从样例包的 rules 模块直接 import，避免两处字符串漂移）。
    """

    def __init__(self, rows, rules_module, workroot, vl_live=None, corpus=None):
        self.rows = rows
        self.rules = rules_module
        self.workroot = os.path.abspath(workroot)
        self.vl_live = vl_live          # None = 回放；dict = 真调 VL
        self.corpus = corpus
        self.token = "replay-token"
        self.calls = []
        self._httpd = None
        self.prompt_to_rule = {
            rules_module.PROMPT_DOMAIN: "R0_product_main",
            rules_module.PROMPT_WHITE_BG: "R2_white_bg",
            rules_module.PROMPT_NO_OVERLAY: "R3_no_watermark",
            rules_module.PROMPT_ATTRIBUTE: "R5_attribute",
        }

    # --- 生命周期 -----------------------------------------------------------
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
                with gw_lock:
                    gw.calls.append({"path": self.path, "payload": payload})
                if token != gw.token:
                    return self._send(401, {"error": "unauthorized"})
                try:
                    status, result = gw.dispatch(self.path, payload)
                except _ReplayError as exc:
                    return self._send(exc.status, {"error": exc.reason})
                return self._send(status, result)

        gw_lock = threading.Lock()
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

    # --- 端点 ---------------------------------------------------------------
    def dispatch(self, path, payload):
        if path == "/io/read":
            return self.io_read(payload)
        if path == "/detect/u2net":
            return self.u2net(payload)
        if path == "/detect/ocr":
            return self.ocr(payload)
        if path == "/vl/judge":
            return self.vl(payload)
        if path == "/report/emit":
            return 200, {"ok": True, "job_id": payload.get("job_id")}
        if path in ("/op/crop", "/op/letterbox", "/op/upscale", "/op/replace_bg",
                    "/op/inpaint"):
            return self.op(path, payload)
        raise _ReplayError(404, "no_such_endpoint")

    def _rows_for(self, path):
        key = os.path.basename(str(path or ""))
        rows = self.rows.get(key)
        if rows is None:
            raise _ReplayError(404, "asset_unreadable")
        return rows

    def io_read(self, payload):
        path = str(payload.get("path") or "")
        rows = self._rows_for(path)
        size_row = rows.get("R1_size") or {}
        short = size_row.get("score")
        short = int(round(float(short))) if isinstance(short, (int, float)) else 1600
        exists = os.path.isfile(path)
        return 200, {
            "path": path,
            "size": os.path.getsize(path) if exists else 0,
            "sha256": "",
            "mime": "image/jpeg",
            "width": short,
            "height": short,
            "replayed": True,
            "dims_source": "R1_size.score(短边 px) 反推；另一维取同值（短边判定不受影响）",
        }

    def u2net(self, payload):
        path = str(payload.get("path") or "")
        rows = self._rows_for(path)
        ratio = (rows.get("R4_subject_ratio") or {}).get("score")
        if ratio is None:
            raise _ReplayError(404, "asset_unreadable")
        mask = os.path.join(self.workroot, "mask-%s.png" % os.path.basename(path))
        _write_stub_png(mask, 64, 64)
        return 200, {"path": path, "fg_ratio": float(ratio), "mask_path": mask,
                     "width": 64, "height": 64}

    def ocr(self, payload):
        # 基线没有留 OCR 原始行；R3 的结论仍由 VL 通道回放给出（见 vl()）
        return 200, {"path": payload.get("path"), "lines": [], "engine": "replay-noop"}

    def vl(self, payload):
        path = str(payload.get("path") or "")
        prompt = str(payload.get("prompt") or "")
        rule_id = self.prompt_to_rule.get(prompt)
        rows = self._rows_for(path)
        if self.vl_live:
            return self.vl_live(rule_id, path, prompt, payload.get("max_tokens"))
        row = rows.get(rule_id) if rule_id else None
        if not row:
            return 200, {"path": path, "undetermined": True, "raw": "",
                         "reason": "replay_missing_row"}
        status = row.get("status")
        if status == "skipped":       # 短路派生行：包自己也会短路，不该走到这里
            return 200, {"path": path, "undetermined": True, "raw": "",
                         "reason": "replay_skipped_row"}
        return 200, {"path": path, "undetermined": False, "raw": "",
                     "replayed": True,
                     "rules": [{"status": status, "score": row.get("score"),
                                "label": "replay:%s" % rule_id}]}

    def op(self, path, payload):
        out = payload.get("out")
        if not out:
            raise _ReplayError(400, "invalid_params")
        os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
        _write_stub_png(out, 64, 64)
        return 200, {"path": os.path.abspath(out), "width": 1600, "height": 1600,
                     "op": path.rsplit("/", 1)[-1]}


class _ReplayError(Exception):
    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status
        self.reason = reason


def _write_stub_png(path, width, height):
    row = b"\x00" + bytes([220, 220, 220]) * width

    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    blob = b"\x89PNG\r\n\x1a\n"
    blob += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    blob += chunk(b"IDAT", zlib.compress(row * height, 6))
    blob += chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(blob)


# ---------------------------------------------------------------------------
# 真 VL（--vl-live）：deepseek-flash，只取 content（思考链在 reasoning_content）
# ---------------------------------------------------------------------------
def make_live_vl(api_key, base_url=VL_BASE, model=VL_MODEL, timeout=90, workdir=None):
    """真 VL 通道。**必须复刻宿主网关的入参归一**（`vl_image::load_for_vl`：
    短边 512 / 长边 ≤4096 / 重编码 JPEG），否则 6.5MB 原图会被服务端 400
    （实测：`unsupported image`，不是模型能力问题）。宿主真网关自己做这步，
    本脚本因为替身了网关，得自己补。ffmpeg 缺失时如实落 undetermined。
    """
    import shutil
    import subprocess
    import urllib.error
    import urllib.request

    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    norm_dir = os.path.join(workdir or tempfile.mkdtemp(prefix="vl-norm-"), "vl")
    os.makedirs(norm_dir, exist_ok=True)

    def normalize(path):
        """按 load_for_vl 口径归一，返回可送的 JPEG 路径（失败则原样返回）。"""
        if not (ffmpeg and ffprobe):
            return path
        try:
            out = subprocess.run(
                [ffprobe, "-v", "error", "-select_streams", "v:0", "-show_entries",
                 "stream=width,height", "-of", "csv=p=0", path],
                capture_output=True, text=True, timeout=30).stdout.strip()
            width, height = [int(v) for v in out.split(",")[:2]]
        except (ValueError, OSError, subprocess.SubprocessError):
            return path
        scale = 1.0
        short = min(width, height)
        if short < 512:
            scale = 512.0 / short
        long_side = max(width, height) * scale
        if long_side > 4096:
            scale *= 4096.0 / long_side
        tw, th = max(2, int(round(width * scale))), max(2, int(round(height * scale)))
        if (tw, th) == (width, height) and os.path.getsize(path) <= 1_500_000:
            return path
        target = os.path.join(norm_dir, os.path.basename(path) + ".norm.jpg")
        try:
            subprocess.run([ffmpeg, "-y", "-loglevel", "error", "-i", path,
                            "-vf", "scale=%d:%d" % (tw, th), "-q:v", "3", target],
                           capture_output=True, timeout=120, check=True)
        except (OSError, subprocess.SubprocessError):
            return path
        return target

    def call(rule_id, path, prompt, max_tokens):
        path = normalize(path)
        with open(path, "rb") as fh:
            b64 = base64.b64encode(fh.read()).decode("ascii")
        body = json.dumps({
            "model": model,
            "max_tokens": int(max_tokens or VL_MAX_TOKENS),
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": prompt},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64," + b64}},
            ]}],
        }, ensure_ascii=False).encode("utf-8")
        request = urllib.request.Request(
            base_url.rstrip("/") + "/chat/completions", data=body, method="POST",
            headers={"content-type": "application/json",
                     "authorization": "Bearer " + api_key})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as resp:
                payload = json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as exc:
            detail = ""
            try:
                detail = str(json.loads(exc.read().decode("utf-8")).get("error", {})
                             .get("message") or "")[:120]
            except (ValueError, AttributeError):
                pass
            return 200, {"path": path, "undetermined": True, "raw": detail,
                         "reason": "vl_http_%d" % exc.code}
        except (urllib.error.URLError, TimeoutError, OSError):
            return 200, {"path": path, "undetermined": True, "raw": "",
                         "reason": "vl_unreachable"}
        content = ((payload.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
        status = _status_from_answer(content, expect_yes=(rule_id != "R3_no_watermark"))
        return 200, {"path": path, "undetermined": status is None, "raw": content.strip(),
                     "reason": None if status else "vl_answer_unparsed",
                     "rules": [{"status": status, "score": None,
                                "label": "live:%s" % model}] if status else []}

    return call


def _status_from_answer(text, expect_yes):
    """与样例包 rules.status_from_text 同口径（本文件独立实现，防「一起抄错」未被发现）。"""
    clean = (text or "").strip()[:32].lower().replace("是否", "")
    if not clean:
        return None
    if any(tok in clean for tok in ("不是", "不符合", "不满足", "no", "false", "否")):
        hit = False
    elif any(tok in clean for tok in ("符合", "满足", "yes", "true", "是")):
        hit = True
    else:
        return None
    return "pass" if hit == bool(expect_yes) else "fail"


# ---------------------------------------------------------------------------
# 比对
# ---------------------------------------------------------------------------
def project_engine_score(rule_id, score, thresholds):
    """旧引擎 score → 包 score 口径（唯一一处单位换算，写死并注释）。"""
    if score is None:
        return None
    value = float(score)
    if rule_id == "R1_size":
        short_min = float((thresholds.get("R1_size") or {}).get("short_side_min", 1600))
        return 1.0 if value >= short_min else value / short_min
    return value          # R2 white_dev / R4 fg_ratio：同单位、同精度，直接比


def compare_asset(key, engine_rows, sdk_verdicts, thresholds, gate_mismatch=False):
    """逐规则比对，返回行记录列表。

    `gate_mismatch`：本资产的 R0 结论与基线不同（VL 抖动）→ 下游 skipped 是
    **gate 短路派生**，不是规则判定差异，单列不参与等式（同 K00 §4 的口径）。
    """
    by_rule = {v.get("ruleId"): v for v in sdk_verdicts or []}
    results = []
    for rule_id in ("R0_product_main", "R1_size", "R2_white_bg", "R4_subject_ratio",
                    "R3_no_watermark", "R5_attribute"):
        engine = engine_rows.get(rule_id) or {}
        got = by_rule.get(rule_id) or {}
        row = {
            "asset": key,
            "ruleId": rule_id,
            "engineStatus": engine.get("status"),
            "engineScore": engine.get("score"),
            "sdkStatus": got.get("status"),
            "sdkScore": got.get("score"),
            "sdkNote": got.get("note"),
        }
        if rule_id in NOT_JUDGED:
            row.update(category="口径差(不参与等式)", statusMatch=None, scoreMatch=None,
                       verdict="单列",
                       reason="旧引擎 on_fail:pass 记 pass；插件按裁决只出词表（skipped）")
            results.append(row)
            continue

        if (gate_mismatch and got.get("status") == "skipped"
                and engine.get("status") != "skipped"):
            row.update(category="派生(gate 抖动)", statusMatch=None, scoreMatch=None,
                       verdict="派生",
                       reason="R0 结论与基线不同 → 本条被短路，不是规则判定差异")
            results.append(row)
            continue

        row["statusMatch"] = row["engineStatus"] == row["sdkStatus"]
        if rule_id in VL_RULES:
            row.update(category="VL(回放)", scoreMatch=None,
                       verdict="PASS" if row["statusMatch"] else "FAIL")
            results.append(row)
            continue

        # 确定性规则：度量值 + 投影后 score 双向比对
        measurement_engine = row["engineScore"]
        measurement_sdk = {
            "R1_size": ((got.get("evidence") or {}).get("short_side")),
            "R2_white_bg": ((got.get("evidence") or {}).get("white_dev")),
            "R4_subject_ratio": got.get("score"),
        }[rule_id]
        projected = project_engine_score(rule_id, measurement_engine, thresholds)
        row["engineScoreProjected"] = projected
        row["sdkMeasurement"] = measurement_sdk
        row["measurementMatch"] = _close(measurement_engine, measurement_sdk)
        row["scoreMatch"] = _close(projected, row["sdkScore"])
        ok = bool(row["statusMatch"] and row["measurementMatch"] and row["scoreMatch"])
        row.update(category="确定性", verdict="PASS" if ok else "FAIL")
        results.append(row)
    return results


def _close(left, right, tol=TOLERANCE):
    if left is None or right is None:
        return left is None and right is None
    try:
        return abs(float(left) - float(right)) <= tol
    except (TypeError, ValueError):
        return False


# ---------------------------------------------------------------------------
ENGINE_R4_MIN = 0.65     # main 的 packs/ecommerce-taobao/rules.yaml：min 0.65（K00 基线跑的就是这个）
PRODUCT_R4_MIN = 0.55    # 用户裁决的产品口径（fix-policy-and-a4.md）


def judge_all(proc, keys, grouped, corpus, tmpdir, thresholds, r4_min, job_id="j"):
    """对每个资产调一次 judge（网关回放测量值），返回 (比对行, 原始 verdicts)。"""
    rows = []
    per_asset = {}
    for key in keys:
        engine_rows = grouped[key]
        asset_path = resolve_corpus_path(corpus, rows_of(engine_rows)) or key
        meas = {}
        white = (engine_rows.get("R2_white_bg") or {}).get("score")
        if isinstance(white, (int, float)):
            meas["white_dev"] = float(white)     # 旧引擎 R2 走 classic：把像素统计喂回去
        options = {"measurements": meas, "workdir": tmpdir,
                   "thresholds": {"R4_subject_ratio": {"min": float(r4_min)}}}
        resp = proc.send("judge", {
            "job_id": job_id,
            "asset": {"id": key, "path": asset_path},
            "rules": None,
            "options": options,
        }, timeout=180)
        if resp.error:
            per_asset[key] = {"error": resp.error}
            rows.append({"asset": key, "ruleId": "*", "category": "调用失败",
                         "engineStatus": None, "sdkStatus": None, "verdict": "FAIL",
                         "reason": "judge 返回错误 %s：%s" % (resp.code, resp.message)})
            continue
        verdicts = (resp.result or {}).get("verdicts") or []
        per_asset[key] = {"verdicts": verdicts}
        sdk_gate = next((v.get("status") for v in verdicts
                         if v.get("ruleId") == "R0_product_main"), None)
        gate_mismatch = sdk_gate != (engine_rows.get("R0_product_main") or {}).get("status")
        rows.extend(compare_asset(key, engine_rows, verdicts, thresholds,
                                  gate_mismatch=gate_mismatch))
    return rows, per_asset


def load_baseline(path):
    with open(path, encoding="utf-8") as fh:
        rows = json.load(fh)
    if not isinstance(rows, list) or not rows:
        raise SystemExit("基线不是非空数组：%s" % path)
    grouped = {}
    for row in rows:
        key = os.path.basename(str(row.get("fileName") or ""))
        grouped.setdefault(key, {})[row.get("ruleId")] = row
    return grouped


def resolve_corpus_path(corpus, row):
    rel = str(row.get("corpusFile") or "")
    if corpus and rel:
        candidate = os.path.join(corpus, rel)
        if os.path.isfile(candidate):
            return candidate
    return os.path.join(corpus or "", rel) if corpus else ""


def measure_startup(pack_dir, gateway_url, gateway_token, tmpdir):
    """从 spawn 到 health 应答的耗时（文档用「启动时间」）。"""
    from _support import PackProcess
    started = time.time()
    proc = PackProcess(pack_dir, env={
        "DAMPACK_GATEWAY_URL": gateway_url,
        "DAMPACK_GATEWAY_TOKEN": gateway_token,
        "DAMPACK_WORK_DIR": tmpdir,
    })
    try:
        resp = proc.send("health", {}, timeout=30)
        elapsed_ms = (time.time() - started) * 1000.0
        return elapsed_ms if resp.error is None else None
    finally:
        proc.close()


def dir_profile(path):
    total = files = lines = 0
    for root, _dirs, names in os.walk(path):
        for name in names:
            full = os.path.join(root, name)
            if "__pycache__" in full or full.endswith((".pyc", ".pyo")):
                continue
            files += 1
            total += os.path.getsize(full)
            if name.endswith((".py", ".md", ".toml", ".json", ".txt", ".sh")):
                try:
                    with open(full, encoding="utf-8", errors="replace") as fh:
                        lines += sum(1 for _ in fh)
                except OSError:
                    pass
    return {"files": files, "bytes": total, "kib": round(total / 1024.0, 1), "lines": lines}


def render_markdown(payload):
    out = []
    add = out.append
    add("# IP-K11 · Python SDK 与旧引擎 10 张一致性对账")
    add("")
    add("基线：`%s`（K00，10 资产 × 6 规则 = 60 行，`vl:deepseek` 首判口径）  " %
        payload["baseline"])
    add("被测：`%s`（真子进程，stdio JSON-RPC；网关输入为基线回放）  " % payload["pack"])
    add("通道：%s" % payload["channel"])
    add("")
    add("## 0. 汇总")
    add("")
    summary = payload["summary"]
    add("| 类别 | 行数 | status 一致 | 度量一致 | score 一致 | 结论 |")
    add("|---|---|---|---|---|---|")
    add("| 确定性（R1/R2/R4） | %d | %d | %d | %d | %s |"
        % (summary["deterministic_rows"], summary["deterministic_status"],
           summary["deterministic_measurement"], summary["deterministic_score"],
           "PASS" if summary["deterministic_fail"] == 0 else "FAIL(%d)" %
           summary["deterministic_fail"]))
    add("| VL（R0/R3，单列） | %d | %d | — | — | %s |"
        % (summary["vl_rows"], summary["vl_status"], "参考（不作门禁）"))
    add("| R5_attribute（口径差） | %d | — | — | — | 单列说明 |"
        % summary["not_judged_rows"])
    if summary.get("cascade_rows"):
        add("| 派生（R0 gate 抖动 → 下游短路） | %d | — | — | — | 单列（不计等式） |"
            % summary["cascade_rows"])
        add("")
        add("R0 与基线不一致的资产：%s —— 这是 VL 非确定性（K00 §4 已实测过翻转），"
            "该资产的下游 skipped 由短路派生，不参与等式。"
            % ", ".join(summary["gate_mismatch_assets"]))
    add("")
    add("启动时间（spawn → health）：%s ms；SDK 体积：%s KiB / %d 文件 / %d 行"
        % (payload["startup_ms"], payload["sdk_size"]["kib"],
           payload["sdk_size"]["files"], payload["sdk_size"]["lines"]))
    add("")
    add("## 1. 逐图逐规则对比表")
    add("")
    add("| 资产 | 规则 | 类别 | 旧引擎 status/score | SDK 包 status/score | 度量 | 判定 |")
    add("|---|---|---|---|---|---|---|")
    for row in payload["rows"]:
        measure = ""
        if row.get("sdkMeasurement") is not None:
            measure = "%s↔%s %s" % (row.get("engineScore"), row.get("sdkMeasurement"),
                                    "✓" if row.get("measurementMatch") else "✗")
        add("| %s | %s | %s | %s / %s | %s / %s | %s | %s |"
            % (row["asset"], row["ruleId"], row.get("category", ""),
               row.get("engineStatus"), _fmt(row.get("engineScore")),
               row.get("sdkStatus"), _fmt(row.get("sdkScore")), measure,
               row.get("verdict")))
    add("")
    bad = [r for r in payload["rows"] if r.get("verdict") == "FAIL"]
    add("## 2. 差异清单")
    add("")
    if not bad:
        add("无（确定性规则逐值一致；VL 行 status 亦与回放一致）。")
    else:
        for row in bad:
            add("- `%s` / `%s`：旧引擎 %s@%s → SDK 包 %s@%s（note=%s）"
                % (row["asset"], row["ruleId"], row.get("engineStatus"),
                   _fmt(row.get("engineScore")), row.get("sdkStatus"),
                   _fmt(row.get("sdkScore")), row.get("sdkNote")))
    add("")
    add("## 3. R4 阈值口径：对账用 0.65 / 产品裁决 0.55")
    add("")
    add("旧引擎基线跑的是 main 的 `packs/ecommerce-taobao/rules.yaml`（`R4 min: 0.65`），"
        "产品裁决值是 `0.55`（`fix-policy-and-a4.md`）。**等式门禁按 0.65 对账**"
        "（与基线同口径），下表是切到 0.55 后**有意变化**的行：")
    add("")
    add("| 资产 | fg_ratio | 旧引擎@0.65 | SDK 包@0.55 | 语义 |")
    add("|---|---|---|---|---|")
    for row in payload["product_diffs"]:
        add("| %s | %s | %s | %s | %s |"
            % (row["asset"], _fmt(row["score"]), row["engineStatus"], row["sdkStatus"],
               row["meaning"]))
    if not payload["product_diffs"]:
        add("| — | — | — | — | 无差异 |")
    add("")
    add("## 4. 口径与边界（诚实门）")
    add("")
    for line in payload["caveats"]:
        add("- " + line)
    add("")
    return "\n".join(out)


def _fmt(value):
    if value is None:
        return "—"
    if isinstance(value, float):
        return ("%.6f" % value).rstrip("0").rstrip(".") or "0"
    return str(value)


def main(argv=None):
    parser = argparse.ArgumentParser(description="10 张一致性对账（SDK 包 vs 旧引擎基线）")
    parser.add_argument("--baseline", default=os.path.join(
        REPO_ROOT, "reports", "t_7aba77f3-baseline.json"))
    parser.add_argument("--pack", default=EXAMPLE_PACK)
    parser.add_argument("--corpus", default=os.environ.get("DAM_CORPUS", ""),
                        help="语料根（env DAM_CORPUS 可给默认；仅 --vl-live 需要真图；回放模式可不存在）")
    parser.add_argument("--limit", type=int, default=10, help="上限 10 张（分母不得放大）")
    parser.add_argument("--vl-live", action="store_true",
                        help="VL 规则改真调 deepseek-flash（key 从 env DEEPSEEK_API_KEY 读）")
    parser.add_argument("--json-out", help="结果 JSON 落盘路径")
    parser.add_argument("--md-out", help="报告 Markdown 落盘路径")
    parser.add_argument("--exit-zero", action="store_true",
                        help="只出报告，不让不一致影响退出码（写报告用）")
    args = parser.parse_args(argv)

    if not os.path.isfile(args.baseline):
        print("ERROR: 基线不存在：%s" % args.baseline)
        return 2
    if not os.path.isfile(os.path.join(args.pack, "manifest.json")):
        print("ERROR: 包里没有 manifest.json：%s" % args.pack)
        return 2
    if args.limit > 10:
        print("ERROR: 语料上限 10 张（口径：分母不得放大）")
        return 2

    sys.path.insert(0, SDK_DIR)                          # 回放网关 import 样例包的 rules 需要 SDK
    sys.path.insert(0, os.path.join(args.pack, "plugin"))
    import rules as pack_rules            # noqa: E402 —— 回放网关按 prompt 常量识别规则
    from _support import PackProcess      # noqa: E402

    grouped = load_baseline(args.baseline)
    keys = sorted(grouped)[: args.limit]
    if len(keys) < args.limit:
        print("WARN: 基线只有 %d 个资产（要求 %d）" % (len(keys), args.limit))

    live = None
    if args.vl_live:
        key = os.environ.get("DEEPSEEK_API_KEY") or os.environ.get("PACK_BASELINE_VL_KEY") or ""
        if not key:
            print("ERROR: --vl-live 需要 env DEEPSEEK_API_KEY（key 不落文件/报告）")
            return 3
        live = make_live_vl(key, workdir=tempfile.mkdtemp(prefix="consistency-vl-"))
        missing = [k for k in keys if not os.path.isfile(resolve_corpus_path(args.corpus, rows_of(grouped[k])))]
        if missing:
            print("ERROR: --vl-live 需要真图，缺 %d 个：%s" % (len(missing), missing[:3]))
            return 3

    tmpdir = tempfile.mkdtemp(prefix="consistency-")
    gateway = ReplayGateway(grouped, pack_rules, tmpdir, vl_live=live, corpus=args.corpus)
    gateway.start()
    try:
        startup_ms = measure_startup(args.pack, gateway.base_url, gateway.token, tmpdir)
        proc = PackProcess(args.pack, env={
            "DAMPACK_GATEWAY_URL": gateway.base_url,
            "DAMPACK_GATEWAY_TOKEN": gateway.token,
            "DAMPACK_WORK_DIR": tmpdir,
        })
        rows = []
        per_asset = {}
        product_rows = []
        try:
            rows, per_asset = judge_all(proc, keys, grouped, args.corpus, tmpdir,
                                        pack_rules.THRESHOLDS, ENGINE_R4_MIN,
                                        job_id="consistency-engine")
            product_rows, _ = judge_all(proc, keys, grouped, args.corpus, tmpdir,
                                        pack_rules.THRESHOLDS, PRODUCT_R4_MIN,
                                        job_id="consistency-product")
        finally:
            proc.close()
    finally:
        gateway.stop()
        shutil.rmtree(tmpdir, ignore_errors=True)

    deterministic = [r for r in rows if r.get("category") == "确定性"]
    vl_rows = [r for r in rows if r.get("category") == "VL(回放)"]
    not_judged = [r for r in rows if r.get("category") == "口径差(不参与等式)"]
    summary = {
        "assets": len(keys),
        "rules_per_asset": 6,
        "deterministic_rows": len(deterministic),
        "deterministic_status": sum(1 for r in deterministic if r.get("statusMatch")),
        "deterministic_measurement": sum(1 for r in deterministic if r.get("measurementMatch")),
        "deterministic_score": sum(1 for r in deterministic if r.get("scoreMatch")),
        "deterministic_fail": sum(1 for r in deterministic if r.get("verdict") == "FAIL"),
        "vl_rows": len(vl_rows),
        "vl_status": sum(1 for r in vl_rows if r.get("statusMatch")),
        "not_judged_rows": len(not_judged),
        "cascade_rows": sum(1 for r in rows if r.get("category") == "派生(gate 抖动)"),
        "gate_mismatch_assets": sorted({r["asset"] for r in rows
                                        if r.get("category") == "派生(gate 抖动)"}),
    }
    channel = ("VL=deepseek-flash 真调（非门禁）" if live
               else "VL=基线回放（非实测，K07 门禁负责端到端）")
    engine_r4 = {r["asset"]: r for r in rows if r.get("ruleId") == "R4_subject_ratio"}
    product_diffs = []
    for row in product_rows:
        if row.get("ruleId") != "R4_subject_ratio":
            continue
        engine_row = engine_r4.get(row["asset"]) or {}
        if engine_row.get("sdkStatus") == row.get("sdkStatus"):
            continue
        product_diffs.append({
            "asset": row["asset"],
            "score": row.get("engineScore"),
            "engineStatus": engine_row.get("engineStatus"),
            "sdkStatus": row.get("sdkStatus"),
            "meaning": ("旧引擎 0.65 门槛判 fail，产品口径 0.55 判 pass："
                        "这是 t_7b06d277 重标语料后**故意放宽**的那一档"),
        })

    payload = {
        "schema": "damdesk-pack-consistency/1",
        "generated_at": time.strftime("%Y-%m-%d %H:%M:%S"),
        "baseline": args.baseline,
        "pack": args.pack,
        "channel": channel,
        "tolerance": TOLERANCE,
        "startup_ms": None if startup_ms is None else round(startup_ms, 1),
        "sdk_size": dir_profile(SDK_DIR),
        "summary": summary,
        "r4_thresholds": {"equality": ENGINE_R4_MIN, "product": PRODUCT_R4_MIN},
        "product_diffs": product_diffs,
        "rows": rows,
        "per_asset": per_asset,
        "caveats": [
            "网关输入是**回放**：`/io/read` 尺寸由旧引擎 R1 首判分数（短边 px）反推、另一维取同值；"
            "`/detect/u2net` 的 fg_ratio 取 R4 首判分数；R2 走 classic（white_dev 取旧引擎分数）；"
            "VL 结论取基线 status。被测的是插件的判定链路与协议壳，不是模型/像素处理。",
            "score 是派生值：旧引擎 R1 记短边 px（如 600），插件记 short/门槛（0.375）。"
            "比较用 `project_engine_score()` 这一处换算，度量值（短边/white_dev/fg_ratio）另行逐值比对。",
            "R5_attribute 旧引擎 `on_fail: pass` 记 pass，插件按裁决只出词表（skipped）——口径差，单列不判。",
            "端到端（真 u2net / 真 OCR / 真 VL + 真语料 + 宿主网关）**未在此验证**，"
            "由 K07 等价门禁负责；本表只能证明「同一批测量值进去，规则结论与派生值一致」。",
            "R4 阈值口径：基线（K00）跑的是 main 的 `min 0.65`，产品裁决值是 0.55；"
            "等式门禁按 0.65 对账（与基线同口径），0.55 下的差异单列在 §3，属**有意变化**不是缺陷。",
            "度量精度：SDK 样例包 score/evidence **不截断**（官方包 `round(...,3)` 是展示口径），"
            "否则 white_dev=0.000146 这类值会被压成 0，逐值 1e-6 对账直接假红。",
            "`--vl-live` 的 VL 替身会先按宿主网关口径归一图片（短边 512 / 长边 ≤4096 / 重编码 JPEG，"
            "ffmpeg 实现）；宿主真网关自己做这一步，本脚本替身网关所以才补。",
            "VL 规则本身非确定性（K00 §4：同一提示词两次跑 f001 R0 pass→fail），"
            "所以 VL 列即使 status 一致也只作参考，不作门禁。",
        ],
    }

    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=2)
    markdown = render_markdown(payload)
    if args.md_out:
        with open(args.md_out, "w", encoding="utf-8") as fh:
            fh.write(markdown + "\n")

    print(markdown)
    print("\n一致性：确定性 %d/%d status、%d/%d 度量、%d/%d score 一致；VL 单列 %d/%d"
          % (summary["deterministic_status"], summary["deterministic_rows"],
             summary["deterministic_measurement"], summary["deterministic_rows"],
             summary["deterministic_score"], summary["deterministic_rows"],
             summary["vl_status"], summary["vl_rows"]))
    if args.exit_zero:
        return 0
    return 0 if summary["deterministic_fail"] == 0 else 1


def rows_of(engine_rows):
    """把 {ruleId: row} 还原成一行（只为复用 resolve_corpus_path 取 corpusFile）。"""
    return next(iter(engine_rows.values())) if engine_rows else {}


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception:  # noqa: BLE001 —— 脚本自身炸了也要把栈打出来（诚实门）
        traceback.print_exc()
        sys.exit(4)
