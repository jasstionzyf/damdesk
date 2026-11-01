#!/usr/bin/env python3
"""电商行业包（官方包）—— 插件入口（`docs/industry-pack-protocol.md` v1.0）。

启动形态：宿主 `pack-host` 以 stdio 行式 JSON-RPC 拉起本进程
（`python3 plugin/__init__.py`，cwd=包目录，env 带 `DAMPACK_GATEWAY_URL/TOKEN`）。

本文件只做三件事：
1. **协议壳**：收行 → 校验 → 派发给 `HANDLERS` → 回帧；stdout 只走协议行，日志一律 stderr。
2. **编排**：`judge`（六条规则，R0 领域 gate 短路）+ `repair`（几何闭式解 + 全量复判，
   轮次硬上限 3）。规则语义在 `ruleset.py`、几何求解在 `geometry.py`。
3. **能力调用**：一切重活都过网关（`gw()` 是唯一出口），插件自己不解码大图、不出网。

宿主能力缺口（协议 1.0 网关只有 10 端点）导致的两处口径改写、以及 1.1 补齐后的默认路径，
见包内 README「能力缺口」；判定阈值与修复策略的用户裁决见
`damdesk-repo/references/fix-policy-and-a4.md`。
"""
from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shutil
import sys
import threading
import time
import urllib.error
import urllib.request

API_VERSION = "2.0"
PACK_VERSION = "2.0.0"
PACK_ID = "com.soujpg.pack.ecommerce"

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import geometry          # noqa: E402
import maskpng           # noqa: E402
import ruleset           # noqa: E402

RpcError = ruleset.RpcError
VL_MAX_TOKENS = 4096     # 协议 §6 上限；思考型模型低于此会 content 空
OCR_LANG = "chi_sim+eng"

PACK_INFO = {
    "id": PACK_ID,
    "name": "电商主图合规（白底商品图 / 淘宝·Amazon 通用）",
    "version": PACK_VERSION,
    "api_version": API_VERSION,
    "rules": [dict(rule) for rule in ruleset.RULES],
    "capabilities": [
        "io.read",
        "detect.u2net",
        "detect.ocr",
        "detect.white",
        "op.geometric",
        "op.generative",
        "vl.judge",
        "report.emit",
    ],
    "deterministic": False,
    # N9 实测：最大语料（6016×4000 多实例）单张判定 45–68s，8 并发下偶发 >90s →
    # 90s 的 kill 线会把「结论稳定的图」打成 undetermined（3 次复跑翻转 1 次的唯一来源）。
    # 与房产包对齐到 180s：超时只影响「等多久才 kill」，不改任何判定语义。
    "timeout_secs": 180,
    "resources": ["resources/rules.json", "resources/taxonomy.json"],
}

# --- 运行时状态（进程内；按 job 隔离） ---------------------------------------
_STATE_LOCK = threading.Lock()
_CANCEL = {}            # job_id -> threading.Event
_REPORTS = {}           # job_id -> [entry]
_SAFE = re.compile(r"[^0-9A-Za-z_.-]+")


def log(*parts):
    """日志只走 stderr：stdout 是协议通道，一行都不能脏。"""
    sys.stderr.write("[ecommerce] " + " ".join(str(p) for p in parts) + "\n")
    sys.stderr.flush()


def _safe(name, fallback="asset"):
    cleaned = _SAFE.sub("_", str(name or "")).strip("_")
    return (cleaned or fallback)[:64]


def _trace(workdir, **kv):
    """临时诊断（`DAMPACK_TRACE=1` 时把修复链每步落 jsonl）；不作为契约。"""
    if not os.environ.get("DAMPACK_TRACE"):
        return
    try:
        rec = dict(kv, ts=round(time.time(), 3))
        with open(os.path.join(workdir, "_trace.jsonl"), "a", encoding="utf-8") as fh:
            fh.write(json.dumps(rec, ensure_ascii=False) + "\n")
    except Exception:  # noqa: BLE001
        pass


def _asset_tag(asset, path):
    """修复链产物的资产前缀：`<资产 id 或文件名>-<源路径摘要>`。

    宿主给的 workdir 是**整个包共用**的（`{data}/compliance/fixes`），修复链的中间产物
    若按固定名（`r1-1crop.png` / `r1-mask.png`）落盘，不同资产会互相覆盖：K07 A4 实测
    f002/f003 的 crop 应答 2388×2372 而落盘文件是上一资产的 816×818，掩码也串成别的
    尺寸（`/op/inpaint` 直接 400）。产物名带资产前缀后，每个资产的链路只读写自己的文件。
    """
    raw = asset.get("id") if isinstance(asset, dict) else None
    stem = os.path.basename(str(path)).rsplit(".", 1)[0]
    digest = hashlib.sha256(str(path).encode("utf-8")).hexdigest()[:8]
    return "%s-%s" % (_safe(raw or stem, "asset"), digest)


def _same_dims(a, b):
    """两个尺寸对是否相等（形状/类型不规整时返回 False，不抛）。"""
    try:
        return tuple(int(v) for v in a) == tuple(int(v) for v in b)
    except (TypeError, ValueError):
        return False


def _dims_of(path, res=None):
    """产物宽高以**文件实际**为准。

    宿主应答的 `width/height` 与落盘文件可能不一致（K07 A4 实测：应答 2388×2372，
    `/io/read` 读到的却是 816×818），而后续放大倍数、掩码尺寸全都依赖它 —— 信应答就会
    一路错到底（短边不达标 / 掩码与原图不同宽高）。`/io/read` 是唯一可信来源。
    """
    try:
        meta = gw("/io/read", {"path": path})
        width, height = int(meta.get("width") or 0), int(meta.get("height") or 0)
        if width > 0 and height > 0:
            return (width, height)
    except RpcError:
        pass
    if res:
        return (int(res.get("width") or 0) or None, int(res.get("height") or 0) or None)
    return (None, None)


# --- 网关客户端（唯一能力出口） ----------------------------------------------
_GATEWAY_URL = (os.environ.get("DAMPACK_GATEWAY_URL") or "").rstrip("/")
_GATEWAY_TOKEN = os.environ.get("DAMPACK_GATEWAY_TOKEN") or ""


def _http_timeout():
    raw = os.environ.get("DAMPACK_TIMEOUT_SECS")
    try:
        return max(5.0, float(raw)) if raw else 120.0
    except ValueError:
        return 120.0


def _error_detail(exc):
    """解析网关错误体 → (error, detail, op, params)。

    协议 §5 只规定了错误码映射，没说错误文案能丢信息；旧实现只取 `error`
    字段，于是 `-32003` 到宿主只剩一句「参数非法（400）invalid_params」，
    看不出是哪个 op、什么参数、超了哪个门槛（K07 §5.2 / D7）。
    """
    try:
        payload = json.loads(exc.read().decode("utf-8"))
    except Exception:  # noqa: BLE001 —— 错误体不是 JSON 时不该掩盖原始状态码
        return "", "", "", None
    if not isinstance(payload, dict):
        return "", "", "", None
    return (str(payload.get("error") or ""), str(payload.get("detail") or ""),
            str(payload.get("op") or ""), payload.get("params"))


def _context(detail, op, params, path):
    """把「哪个 op + 实际参数 + 为什么不行」拼成一句人读的错误上下文。"""
    op = op or path
    bits = []
    if params:
        try:
            shown = json.dumps(params, ensure_ascii=False, sort_keys=True)
        except Exception:  # noqa: BLE001
            shown = str(params)
        if len(shown) > 300:
            shown = shown[:300] + "…"
        bits.append("[%s] %s" % (op, shown))
    else:
        bits.append("[%s]" % (op,))
    if detail:
        bits.append(detail)
    return " ".join(bits)


def gw(path, payload):
    """调网关端点；错误映射严格按协议 §5（不吞 403，不重试）。

    错误文案必须带**端点 + 实际参数 + 网关 detail**（不许裸 `-32003`）：
    宿主把插件错误原样落进 `evidence.repair_error`，缺上下文就等于没证据。
    """
    if not _GATEWAY_URL:
        raise RpcError(-32000, "DAMPACK_GATEWAY_URL 未设置，插件无法调用宿主能力")
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        _GATEWAY_URL + path, data=body, method="POST",
        headers={"content-type": "application/json; charset=utf-8",
                 "authorization": "Bearer " + _GATEWAY_TOKEN},
    )
    try:
        with urllib.request.urlopen(request, timeout=_http_timeout()) as resp:
            raw = resp.read().decode("utf-8")
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as exc:
        err, detail, op, params = _error_detail(exc)
        detail = detail or err
        ctx = _context(detail, op, params, path)
        if exc.code == 401:
            raise RpcError(-32000, "网关鉴权失败（401）%s" % ctx)
        if exc.code == 403:
            raise RpcError(-32001, "能力未授权（403）%s" % ctx)
        if exc.code == 404:
            raise RpcError(-32002, "资产不可读（404）%s" % ctx)
        if exc.code == 400:
            raise RpcError(-32003, "参数非法（400）%s" % ctx)
        raise RpcError(-32000, "网关错误 %s %s" % (exc.code, ctx))
    except urllib.error.URLError as exc:
        raise RpcError(-32000, "网关不可达（%s）：%s" % (path, exc.reason))
    except (TimeoutError, OSError) as exc:
        raise RpcError(-32000, "网关调用失败（%s）：%r" % (path, exc))


# --- 取消语义 ----------------------------------------------------------------
def _cancel_flag(job_id):
    with _STATE_LOCK:
        flag = _CANCEL.get(job_id)
        if flag is None:
            flag = threading.Event()
            _CANCEL[job_id] = flag
        return flag


def _check_cancel(job_id, stage=""):
    flag = _CANCEL.get(job_id)
    if flag is not None and flag.is_set():
        raise RpcError(-32004, "任务已取消%s" % (("（" + stage + "）") if stage else ""))


def _drop_job(job_id):
    with _STATE_LOCK:
        _CANCEL.pop(job_id, None)
    return True


# --- 参数校验 ----------------------------------------------------------------
def _require_path(asset):
    if not isinstance(asset, dict) or not asset.get("path"):
        raise RpcError(-32003, "asset.path 必填")
    return str(asset["path"])


def _normalize_rules(rules):
    """请求的规则集：缺省 = 全部；顺序按包内 pipeline 声明序。"""
    if not rules:
        return list(ruleset.PIPELINE_ORDER)
    wanted = []
    for item in rules:
        rid = str(item)
        if rid not in wanted:
            wanted.append(rid)
    ordered = [rid for rid in ruleset.PIPELINE_ORDER if rid in wanted]
    ordered += [rid for rid in wanted if rid not in ruleset.PIPELINE_ORDER]
    return ordered


def _workdir(options):
    raw = (options or {}).get("workdir") or os.environ.get("DAMPACK_WORK_DIR")
    if not raw:
        raise RpcError(-32003, "缺少 workdir（options.workdir 或 DAMPACK_WORK_DIR）")
    os.makedirs(raw, exist_ok=True)
    return raw


# --- 单规则判定（judge / repair 复判共用同一实现，保证首判与复判同语义） -------
def _gw_lacks(err, endpoint):
    """宿主缺这个端点/能力：403 未授权（-32001）或 404 no_such_endpoint（旧协议网关）。"""
    if err.code == -32001:
        return True
    return err.code == -32002 and "no_such_endpoint" in str(err)


def _judge_domain(job_id, path, thresholds=None, subject=None):
    """R0 领域 gate：**确定性前置 + 三项硬否决 + VL 多数票**（K07-FIX-I / t_9267ed7a）。

    判据四档（数字全部来自 schema `rules[].params`，代码不写死）：

    0. **不适用（零 VL）**：面积 ≤2% → `skipped`（`ruleset.classify_domain`；
       画面里没有可判主体 = 本包对该图不适用，不是「不合格」——卡 t_9267ed7a）。
       这一档必须先于「贴边」判：掩码是噪声时 bbox 同样会贴着画面边。
    1. **确定性通过（零 VL）**：面积 ≥15% 且掩码成团、且主体框不贴边 → pass。
    2. **VL 档**：边界面积 / 掩码散碎 / 整幅掩码 / **主体框贴边** 都要 VL 复核 ——
       要求结构化 JSON 应答（主体在场 + 面积、others 清单、verdict），三项否决由
       `ruleset.eval_domain_answer` 在代码里执行（模型只给事实，判据不交给它自由裁量）。
       模型明确回 `subject.present=false`（画面里没有商品主体）→ `skipped`，不算 fail。
       3 份采样按**多数票**汇总（`ruleset.combine_samples`），票数与逐票结论落 evidence + 日志。

    为什么「贴边」不再直接判 fail（t_9267ed7a 真机实测）：`/detect/bbox` 的掩码在
    非商品图（截图 / 拼贴 / 整屏 UI）上同样会给出「贴着画面边、面积 60%+、填充度 0.97」
    的团块 —— 纯几何判据分不出「商品被裁掉」与「整张图是个界面」。实测 shot-a.png
    （真实截图）fg_ratio=0.630、bottom 边距 0.0 → 旧口径直接 fail + score 0.000，
    等于给非商品图挂假不合格。现在贴边只作为「必须问模型」的理由，模型说没主体即 skipped。

    `subject` 由调用方传入（`/detect/bbox` 一轮只测一次，R3/R4 复用同一份掩码测量）。
    """
    th = ruleset.thresholds_for("R0_product_main", thresholds)
    measurement = _subject_evidence(subject)
    decision = ruleset.classify_domain(
        (subject or {}).get("fg_ratio"),
        bbox_area_ratio=_bbox_area_ratio(subject),
        area_min=th.get("subject_area_min"),
        area_absent=th.get("subject_area_absent"),
        solidity_min=th.get("solidity_min"),
        full_frame_ratio=th.get("full_frame_ratio"),
    )
    if decision["action"] == "skip":
        verdict = ruleset.domain_gate_verdict(decision)
        verdict["evidence"] = dict(verdict["evidence"], subject=measurement,
                                   thresholds=th)
        log("R0_product_main", "decided_by=subject_absent",
            "fg_ratio=%s" % (decision["metrics"].get("fg_ratio"),))
        return verdict
    cut = ruleset.subject_cut(subject, margin_ratio=th.get("edge_margin_ratio"))
    if decision["action"] != "vl" and not cut.get("cut"):
        verdict = ruleset.domain_gate_verdict(decision)
        verdict["evidence"] = dict(verdict["evidence"], subject=measurement,
                                   thresholds=th)
        return verdict
    confirm_note = "subject_cut_edge" if cut.get("cut") else decision["note"]
    prompt = ruleset.domain_prompt(th)
    samples = int(ruleset.VL_SAMPLES)
    reply = gw("/vl/judge", {"path": path, "prompt": prompt,
                             "max_tokens": VL_MAX_TOKENS, "samples": samples})
    raws = (reply or {}).get("raws")
    if not isinstance(raws, list) or not raws:
        raws = [reply or {}]
    statuses, details = [], []
    for index, item in enumerate(raws):
        piece = item if isinstance(item, dict) else {"raw": str(item)}
        answer = ruleset.parse_domain_answer(piece.get("raw"))
        # 旧宿主/旧 prompt 可能回 `{"status":"pass"}` 或纯词表答案：都按单票词表结论处理
        structured = isinstance(answer, dict) and any(
            key in answer for key in ("verdict", "subject", "others"))
        if not structured:
            # 旧宿主/模型没按 JSON 回（协议 1.1 只有单份 raw）→ 词表降级，仍进多数票
            status = ruleset.eval_answer(piece, expect_yes=True,
                                         rule_id="R0_product_main",
                                         label="domain_gate")["status"]
            details.append({"sample": index, "status": status, "reasons": ["unparsed_answer"],
                            "raw": str(piece.get("raw") or "")[:160]})
            statuses.append(status)
            continue
        result = ruleset.eval_domain_answer(answer, subject=subject, thresholds=th)
        statuses.append(result["status"])
        details.append({"sample": index, "status": result["status"],
                        "reasons": result["reasons"],
                        "subject": result["metrics"]["subject"],
                        "others": result["metrics"]["others"],
                        "cut": result["metrics"]["cut"].get("note"),
                        "raw": str(piece.get("raw") or "")[:160]})
    evidence = ruleset.prompt_evidence(
        "R0_product_main", text=prompt,
        samples_answered=len(raws),
        decided_by="vl_veto",
        confirm_note=confirm_note,
        subject_cut=cut,
        subject=measurement,
        subject_measurement=decision["metrics"],
        thresholds=th,
        raw=(raws[0] or {}).get("raw") if isinstance(raws[0], dict) else None,
    )
    verdict = ruleset.combine_samples(statuses, rule_id="R0_product_main",
                                      label="domain_gate", evidence=evidence,
                                      details=details)
    # 多数派的逐票理由去重后落 evidence：这是「为什么判 fail」的唯一可读答案
    majority = verdict["evidence"].get("majority") or verdict["status"]
    reasons = []
    for detail in details:
        if detail.get("status") != majority:
            continue
        for reason in detail.get("reasons") or ():
            if reason not in reasons:
                reasons.append(reason)
    verdict["evidence"]["reasons"] = reasons
    verdict["evidence"]["veto"] = reasons[0] if reasons else None
    log("R0_product_main", "gate=%s" % decision["note"],
        "votes=%s" % (verdict["evidence"].get("votes"),),
        "sample_statuses=%s" % (statuses,),
        "reasons=%s" % (reasons,))
    return verdict


def _subject_evidence(subject):
    """主体测量的 evidence 摘要（不落整份 bbox 应答，只落判定用到的量）。"""
    if not isinstance(subject, dict):
        return None
    return {"fg_ratio": _maybe_float(subject.get("fg_ratio")),
            "bbox": subject.get("bbox"),
            "bbox_area_ratio": _bbox_area_ratio(subject),
            "width": subject.get("width"), "height": subject.get("height")}


def _maybe_float(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return value


def _bbox_area_ratio(subject):
    """主体框面积占整幅的比例（`/detect/bbox` 的 `bbox` 是 `[x1,y1,x2,y2]` 源图像素）。"""
    if not isinstance(subject, dict):
        return None
    box = subject.get("bbox")
    width, height = subject.get("width"), subject.get("height")
    try:
        x1, y1, x2, y2 = [float(v) for v in (box or [])[:4]]
        if not width or not height:
            return None
        area = max(0.0, x2 - x1) * max(0.0, y2 - y1)
        return area / (float(width) * float(height))
    except (TypeError, ValueError):
        return None


def _vl_judge(job_id, path, rule_id, prompt, *, expect_yes, label, samples=None):
    """VL 提问的唯一出口：prompt 版本化 + 固定解码参数 + N 次采样多数票。

    - `samples`（默认 `ruleset.VL_SAMPLES`=3）：宿主按这个数**重复提问**并回 `raws`；
      协议 1.1 及更早的宿主不认这个字段、只回单个 `raw`，插件按 1 份采样处理（不报错）。
    - 每份采样单独解析成 status，再由 `ruleset.combine_samples` 汇总：
      过半多数票采纳，平票 → `undetermined`（`vl_no_majority`），票数落 evidence。
    - evidence 里落 `prompt_id` / `prompt_sha256` / `temperature` / `max_tokens`，
      判定变了能立刻分清「prompt 改了」还是「模型抖了」。
    """
    samples = int(samples or ruleset.VL_SAMPLES)
    reply = gw("/vl/judge", {"path": path, "prompt": prompt,
                             "max_tokens": VL_MAX_TOKENS, "samples": samples})
    raws = (reply or {}).get("raws")
    if not isinstance(raws, list) or not raws:
        raws = [reply or {}]
    statuses = []
    for item in raws:
        piece = item if isinstance(item, dict) else {"raw": str(item)}
        statuses.append(ruleset.eval_answer(piece, expect_yes=expect_yes,
                                            rule_id=rule_id, label=label)["status"])
    evidence = ruleset.prompt_evidence(
        rule_id,
        text=prompt,
        samples_requested=samples,
        samples_answered=len(raws),
        raw=(raws[0] or {}).get("raw") if isinstance(raws[0], dict) else None,
    )
    return ruleset.combine_samples(statuses, rule_id=rule_id, label=label,
                                   evidence=evidence)


def _subject_frame(path, target_ratio=None):
    """主体几何：`/detect/bbox`（协议 1.1）优先，旧宿主退回 `/detect/u2net`。

    宿主对同一张图连调 u2net/bbox 只跑一次推理（按路径+mtime 缓存掩码），
    所以 R3（要主体框排除吊牌）与 R4（要占比/窗）各调一次不会重复付算力。
    """
    body = {"path": path}
    if target_ratio is not None:
        body["target_ratio"] = target_ratio
    try:
        return gw("/detect/bbox", body)
    except RpcError as err:
        if not _gw_lacks(err, "/detect/bbox"):
            raise
        return gw("/detect/u2net", {"path": path})


def _instance_boxes(subject):
    """`/detect/bbox` 的 `instances` → 裸 bbox 列表（多实例主体）。

    老宿主（没有 `instances`）→ 空列表，调用方退回单主体口径；
    宿主没探到前景时也给 `[]`，语义同「没有主体」。
    """
    out = []
    for item in (subject or {}).get("instances") or []:
        box = item.get("bbox") if isinstance(item, dict) else item
        if box:
            out.append(box)
    return out


def _judge_size(path, info, thresholds, rule_id="R1_size"):
    meta = info or gw("/io/read", {"path": path})
    th = ruleset.thresholds_for("R1_size", thresholds)
    return ruleset.eval_size(meta.get("width") or 0, meta.get("height") or 0,
                             short_side_min=th.get("short_side_min", 1600),
                             long_side_min=th.get("long_side_min", 0) or 0,
                             rule_id=rule_id)


def _judge_white(job_id, path, thresholds, options, info=None):
    """白底：宿主像素统计（`/detect/white_stats`，协议 1.1）优先，VL 只做兜底。

    口径与旧引擎 `white_edge` 检测器逐值同源（同一份 `engine-classic` 实现），
    所以 R2 的 score 与阈值语义和 classic 完全等价、且确定性可复现（协议 §7 铁律 2）。
    `options.measurements.white_dev` 仍保留：宿主若已算过统计，直接复用省一次解码。
    """
    th = ruleset.thresholds_for("R2_white_bg", thresholds)
    max_dev = th.get("max_dev", 8)
    measures = (options or {}).get("measurements") or {}
    dev = measures.get("white_dev")
    if dev is None and isinstance(info, dict):
        # K07-FIX-D：`/io/read` 带 `measure:["white"]` 时宿主已把边缘白度一并回填
        dev = info.get("white_dev")
    if dev is not None:
        # 同一次测量里的 band/worst_dev 一并落 evidence：结论要能按口径复现
        # （`/io/read` 的 measure 与 `/detect/white_stats` 回的是同一份统计，协议 1.1）
        return ruleset.eval_white_edge(dev, max_dev=max_dev,
                                      band=(info or {}).get("white_band"),
                                      worst_dev=(info or {}).get("white_worst_dev"))
    try:
        stats = gw("/detect/white_stats", {"path": path, "max_dev": max_dev})
    except RpcError as err:
        # 宿主没这项能力（部署方只授权子集 → -32001；协议 1.0 网关根本没这个端点 →
        # 404 no_such_endpoint）→ 退回 VL，并在 evidence 里标明降级来源，不假装是 classic
        if not _gw_lacks(err, "/detect/white_stats"):
            raise
        verdict = _vl_judge(job_id, path, "R2_white_bg", ruleset.PROMPT_WHITE_BG,
                            expect_yes=True, label="white_background")
        verdict["evidence"] = dict(verdict.get("evidence") or {},
                                   source="vl_fallback_no_white_stats")
        return verdict
    mean_dev = stats.get("mean_dev")
    if mean_dev is None:
        return ruleset.verdict("R2_white_bg", "undetermined", score=None,
                               label="white_edge_dev",
                               evidence={"source": "classic_white_edge"},
                               note="no_white_measurement")
    return ruleset.eval_white_edge(mean_dev, max_dev=max_dev,
                                   band=stats.get("band"),
                                   worst_dev=stats.get("worst_dev"))


def _measure_subject(job_id, path, thresholds, rule_id="R4_subject_ratio", subject=None):
    seg = subject if subject is not None else _subject_frame(path)
    ratio = seg.get("fg_ratio")
    th = ruleset.thresholds_for("R4_subject_ratio", thresholds)
    if ratio is None:
        return ruleset.verdict(rule_id, "undetermined", score=None,
                               label="subject_ratio",
                               evidence={"source": "u2net"},
                               note="no_subject_measurement")
    verdict = ruleset.eval_subject_ratio(float(ratio),
                                         minimum=th.get("min", 0.55),
                                         maximum=th.get("max", 0.85),
                                         rule_id=rule_id)
    # 主体框随占比一起落 evidence：修不修得动、按哪个锚点修都要靠它解释
    verdict["evidence"] = dict(verdict.get("evidence") or {},
                               dominant_bbox=seg.get("dominant_bbox"),
                               has_window=bool(seg.get("window")))
    return verdict


def _judge_overlay(job_id, path, *, subject=None, thresholds=None, info=None):
    """R3：OCR 只做候选定位，判据是「文字区性质 + 面积占比」（用户裁决 D1=A）。

    三档（`ruleset.classify_text_overlay`）：
    - 覆盖主体的水印 → fail（确定性）；
    - 主体外文字面积 ≥ 2% → fail（确定性，促销条/牛皮癣）；
    - 落在任一商品实例内的文字（吊牌/型号码/印花）→ pass（排除，商品自带文字不是叠加物）；
    - 主体外但面积 < 2%（型号码/角标/少量文字）→ 交 VL 裁量（促销文案 vs 商品标签）。
    无文字 → pass，不问 VL（省一次云调用）。

    「商品实例」= `/detect/bbox` 的 `instances`（掩码全部连通域，宿主按面积过滤）。
    多商品图（K07 咖啡胶囊 f002：主玻璃杯 + 一簇胶囊）只认主主体时，胶囊上的型号码
    会被算成「主体外文字」→ 误判促销条；按实例集合判就没有这个问题。
    """
    th = ruleset.thresholds_for("R3_no_watermark", thresholds)
    ocr = gw("/detect/ocr", {"path": path, "lang": OCR_LANG})
    lines = ocr.get("lines") or []
    meta = info or gw("/io/read", {"path": path})
    width, height = meta.get("width"), meta.get("height")
    if not width or not height:
        return ruleset.verdict("R3_no_watermark", "undetermined", score=None,
                               label="overlay_text",
                               evidence={"source": "ocr"}, note="no_image_size")
    bbox = (subject or {}).get("dominant_bbox") if subject is not None else None
    if subject is None:
        subject = _subject_frame(path)
        bbox = subject.get("dominant_bbox")
    subjects = _instance_boxes(subject)
    decision = ruleset.classify_text_overlay(
        lines,
        width=width, height=height, subject_bbox=bbox, subject_boxes=subjects,
        text_area_ratio_max=th.get("text_area_ratio_max", 0.02),
        subject_cover_ratio_max=th.get("subject_cover_ratio_max", 0.08),
        min_text_chars=th.get("min_text_chars", 2),
    )
    if decision["action"] != "vl":
        verdict = ruleset.text_overlay_verdict(decision)
        # 框是在**这张画布**上量的：repair 只在画布没变时才敢复用（见 `_inpaint_overlay` 调用点）
        if not isinstance(verdict.get("evidence"), dict):
            verdict["evidence"] = {}
        verdict["evidence"]["dims"] = [width, height]
        return verdict
    # 小面积主体外文字：VL 裁量「促销文案 vs 商品标签」，OCR 事实全部落 evidence
    verdict = _vl_judge(job_id, path, "R3_no_watermark", ruleset.PROMPT_NO_OVERLAY,
                        expect_yes=False, label="overlay_text")
    metrics = decision["metrics"]
    regions = decision["regions"]
    verdict["evidence"] = dict(verdict.get("evidence") or {},
                               source="ocr+vl",
                               lines=metrics["lines"],
                               noise_lines=metrics.get("noise_lines"),
                               outside_area_ratio=metrics["outside_area_ratio"],
                               text_area_ratio=metrics["text_area_ratio"],
                               subject_bbox=metrics.get("subject_bbox"),
                               subject_boxes=metrics.get("subject_boxes"),
                               subject_count=metrics.get("subject_count"),
                               excluded_label_boxes=len(regions["labels"]),
                               # 修复用框必须随 evidence 一起给出：repair 的 inpaint 步骤
                               # 直接复用这份本轮测量结果，否则它会为同一份文件**再跑一遍
                               # OCR**（实测一次 OCR ≈2.9 s，K07-FIX-G）。确定性档
                               # （`text_overlay_verdict`）早就带 boxes，VL 档漏了 → 补齐。
                               boxes=[item["box"] for item in
                                      regions["watermark"] + regions["outside"]],
                               dims=[width, height])
    if verdict.get("status") == "pass":
        verdict["note"] = "small_text_vl_pass"
    return verdict


def _judge_attribute(job_id, path):
    """R5 打标：不是合格判定（`on_fail: pass`），只填词表。"""
    reply = gw("/vl/judge", {"path": path, "prompt": ruleset.PROMPT_ATTRIBUTE,
                            "max_tokens": VL_MAX_TOKENS})
    if (reply or {}).get("undetermined"):
        return ruleset.skipped("R5_attribute", "attribute_undetermined",
                               label="attribute")
    tags = _parse_json_block((reply or {}).get("raw") or "")
    return ruleset.skipped("R5_attribute", "outputs_only:attribute_tagging",
                           label="attribute"), tags


def _parse_json_block(text):
    start = text.find("{")
    end = text.rfind("}")
    if start < 0 or end <= start:
        return {}
    try:
        payload = json.loads(text[start:end + 1])
        return payload if isinstance(payload, dict) else {}
    except Exception:
        return {}


def _has_white_measure(options):
    measures = (options or {}).get("measurements") or {}
    return measures.get("white_dev") is not None


# K07-I：宿主按 `resources/params.schema.json` 发来的是**平铺**的生效值
# （schema v1 没有嵌套对象类型），这里把它折成 ruleset 认的 `thresholds` 结构。
# 参数语义的解释权在插件：宿主只负责「按声明合并 + 校验」，不许懂业务映射。
_FLAT_THRESHOLD_PARAMS = {
    "shortSideMin": ("R1_size", "short_side_min"),
    "longSideMin": ("R1_size", "long_side_min"),
    "whiteMaxDev": ("R2_white_bg", "max_dev"),
    "subjectRatioMin": ("R4_subject_ratio", "min"),
    "subjectRatioMax": ("R4_subject_ratio", "max"),
}


def _merge_flat_thresholds(thresholds, options):
    """平铺参数 → thresholds；显式 `options.thresholds` 作为兜底（老调用方仍可用）。

    优先级：schema 平铺值 > 显式 thresholds > 包默认（`ruleset.THRESHOLDS`）。
    只认白名单键：别的键留给别的消费方（`attribute` / `allow_inpaint` / `measurements`），
    不往阈值里塞。
    """
    merged = {rid: dict(v or {}) for rid, v in (thresholds or {}).items()}
    options = options or {}
    for key, (rule_id, th_key) in _FLAT_THRESHOLD_PARAMS.items():
        if key in options and options[key] is not None:
            merged.setdefault(rule_id, {})[th_key] = options[key]
    return merged or None


def _check_params(options):
    """插件侧二次校验（协议 §7 铁律 1 的对偶：宿主校过的参数，插件也要自己校）。

    宿主只会按单键类型/范围校；**跨键约束只有包自己知道**，所以这里补一条：
    主体占比下限不得大于上限。返回 `-32003`（参数错），不静默改成某个默认值。
    """
    options = options or {}
    lo = options.get("subjectRatioMin")
    hi = options.get("subjectRatioMax")
    if lo is not None and hi is not None and float(lo) > float(hi):
        raise RpcError(-32003,
                       "参数越界：subjectRatioMin=%s > subjectRatioMax=%s" % (lo, hi))
    for key in ("shortSideMin", "longSideMin", "whiteMaxDev"):
        value = options.get(key)
        if value is not None and (not isinstance(value, (int, float)) or isinstance(value, bool)):
            raise RpcError(-32003, "参数类型错：%s 必须是数字（收到 %r）" % (key, value))


def measure(job_id, path, rule_ids, options=None, info=None, respect_deps=True):
    """按 pipeline 声明序判一组规则，返回 {rule_id: verdict}（含 gate 短路）。

    两档：
    - `judge` 用 `respect_deps=True`：R0 与 R1 的短路语义照 pipeline 声明（省 VL/算力）；
    - `repair` 用 `respect_deps=False`：每条规则都要有自己的实测值（否则「R1 修好后
      R4 才被判」会让 R4 永远拿不到首判，产物与 fixed 对不上账）。

    复判范围由调用方给（`rule_ids`）—— repair 的增量复判靠这一点省掉整轮全量
    （K07-FIX-D：只有被改动影响的规则才进 `rule_ids`）。
    """
    options = options or {}
    _check_params(options)
    thresholds = _merge_flat_thresholds(options.get("thresholds"), options)
    verdicts = {}
    domain_status = "pass" if "R0_product_main" not in rule_ids else None
    tags = None
    # 一次 `/io/read` 同时拿宽高与（可选）边缘白度：R2 走 classic 的前提就是这个字段。
    if info is None and ("R1_size" in rule_ids or "R2_white_bg" in rule_ids):
        body = {"path": path}
        if "R2_white_bg" in rule_ids and not _has_white_measure(options):
            body["measure"] = ["white"]
        info = gw("/io/read", body)
    subject = None          # 主体测量（fg_ratio + dominant_bbox + window）一轮只取一次
    subject_loaded = False

    def subject_once():
        """`/detect/bbox` 一轮只测一次：R0（确定性前置）/ R3（排除吊牌）/ R4（占比窗）共用。

        宿主没有主体分割能力（旧网关 404 / 部署方未授权）→ 回 `{}`：
        R0 降级为「纯 VL 确认」，R4 落 undetermined，都不许假装测到了。
        """
        nonlocal subject, subject_loaded
        if subject_loaded:
            return subject
        try:
            subject = _subject_frame(path, ruleset.THRESHOLDS["R4_subject_ratio"]
                                     .get("target"))
        except RpcError as err:
            if not _gw_lacks(err, "/detect/bbox"):
                raise                  # 真错误（读图失败等）照常冒泡，不吞
            subject = {}
        subject_loaded = True
        return subject

    for rid in rule_ids:
        _check_cancel(job_id, "rule:" + rid)
        if rid == "R0_product_main":
            verdict = _judge_domain(job_id, path, thresholds, subject=subject_once())
            domain_status = verdict["status"]
            verdicts[rid] = verdict
            continue
        if domain_status is not None and ruleset.domain_gate_blocks(domain_status):
            verdicts[rid] = ruleset.skipped(
                rid, "domain_gate_%s" % domain_status, label="short_circuit")
            continue
        if rid == "R1_size":
            verdicts[rid] = _judge_size(path, info, thresholds)
        elif rid == "R2_white_bg":
            verdicts[rid] = _judge_white(job_id, path, thresholds, options, info=info)
        elif rid == "R4_subject_ratio":
            # pipeline 的 depends_on：尺寸没达标先修尺寸，占比不判（旧引擎同口径）
            size = verdicts.get("R1_size")
            if respect_deps and size is not None and size.get("status") != "pass":
                verdicts[rid] = ruleset.skipped(rid, "depends_on:R1_size",
                                                label="short_circuit")
            else:
                verdicts[rid] = _measure_subject(job_id, path, thresholds,
                                                 subject=subject_once())
        elif rid == "R3_no_watermark":
            verdicts[rid] = _judge_overlay(job_id, path, subject=subject_once(),
                                           thresholds=thresholds, info=info)
        elif rid == "R5_attribute":
            if options.get("attribute"):
                verdict, tags = _judge_attribute(job_id, path)
                verdict["evidence"] = {"tags": tags or {}}
            else:
                verdict = ruleset.skipped(
                    rid, "outputs_only:attribute_tagging", label="attribute")
            verdicts[rid] = verdict
        else:
            verdicts[rid] = ruleset.skipped(rid, "unknown_rule", label="unknown")
    return verdicts


# --- 修复：几何闭式解 + 换底 + OCR 局部 inpaint -------------------------------
def _op(path, endpoint, payload):
    body = {"path": path}
    body.update(payload)
    out = gw(endpoint, body)
    return str(out.get("path") or ""), out


def _apply_geometry(job_id, path, plan, dims, workdir, stage, tag, short_side_min=1600):
    """执行 crop → letterbox → upscale；产物名 `<资产前缀>-<stage>-…`（见 `_asset_tag`）。

    两处 K07 A4 实测出来的硬约束：
    1. **尺寸一律以文件为准**（`_dims_of`）：宿主应答里的 `width/height` 可能是错的
       （f002/f003 实测应答 2388×2372、落盘 816×818），拿错一次就会按错短边算放大倍数；
    2. **放大倍数按实际画布重算**，不复用求解阶段的预测短边：裁剪/补白的取整会把
       预测值吃掉（f003 预测 1601、实际 1113），短边门槛就永远差一截。
    """
    cur, cur_dims = path, dims
    trail = []
    if plan.get("crop_box"):
        out = os.path.join(workdir, "%s-%s-1crop.png" % (tag, stage))
        cur, res = _op(cur, "/op/crop", {"box": plan["crop_box"], "out": out})
        cur_dims = _dims_of(cur, res)
        trail.append({"op": "crop", "box": plan["crop_box"], "out": cur,
                      "dims": list(cur_dims)})
    width, height = cur_dims
    if plan.get("letterbox") and geometry.needs_letterbox(plan.get("crop_box"),
                                                          width or 0, height or 0):
        out = os.path.join(workdir, "%s-%s-2letterbox.png" % (tag, stage))
        cur, res = _op(cur, "/op/letterbox",
                       {"target_ratio": plan["target_ratio"], "background": "#ffffff",
                        "out": out})
        cur_dims = _dims_of(cur, res)
        trail.append({"op": "letterbox", "target_ratio": plan["target_ratio"],
                      "out": cur, "dims": list(cur_dims)})
    elif plan.get("letterbox_skipped"):
        # 不补白是有意的：1:1 会把占比压破下限（宽主体图），产物保持窗口比例
        trail.append({"op": "letterbox_skipped",
                      "predicted_square_ratio": plan["letterbox_skipped"],
                      "note": "letterbox_would_break_ratio_min"})
    scale = float(plan.get("scale") or 1.0)
    short_now = min(int(cur_dims[0] or 0), int(cur_dims[1] or 0))
    # `plan.scale` 是按**预测**短边算的（求解时还没裁/补），对不上实际画布：
    # 宿主补白把画布钳小（f003 预测 1601 → 实际 1113）就永远差一截，钳大又会白放大。
    # 放大唯一的目的就是「短边够门槛」→ 一律按实际画布重算，plan 的倍数只留作证据。
    if short_side_min and short_now and short_now < int(short_side_min):
        scale = _scale_for(int(short_side_min), short_now)
    else:
        scale = 1.0
    if scale > 1.0:
        cur, res, cur_dims = _upscale(cur, scale, workdir, "%s-%s-3upscale" % (tag, stage))
        trail.append({"op": "upscale", "scale": round(scale, 6), "out": cur,
                      "dims": list(cur_dims),
                      "plan_scale": round(float(plan.get("scale") or 1.0), 6),
                      # 网关自报的口径（`upscale.staged` = 宿主内部拆了多趟；`passes`/`scale_max`
                      # 是人类核对「为什么这么大倍数还能过」的证据，K07 D7）
                      "gateway_op": res.get("op"), "passes": res.get("passes")})
        short_after = min(int(cur_dims[0] or 0), int(cur_dims[1] or 0))
        if short_side_min and short_after and short_after < int(short_side_min):
            # 兜底：宿主取整/分趟后仍差几个像素 → 按差额补一次（只补一次，不循环）
            bump = _scale_for(int(short_side_min), short_after)
            cur, res, cur_dims = _upscale(cur, bump, workdir,
                                          "%s-%s-3upscale-fix" % (tag, stage))
            trail.append({"op": "upscale", "scale": bump, "out": cur,
                          "dims": list(cur_dims), "reason": "short_side_below_min",
                          "gateway_op": res.get("op"), "passes": res.get("passes")})
    return cur, cur_dims, trail


def _scale_for(target_short, short_now):
    """达到目标短边所需的最小放大倍数（向上取到 1e-3，别把该补的像素舍掉）。"""
    return math.ceil(float(target_short) / float(short_now) * 1000.0) / 1000.0


def _upscale(path, scale, workdir, stem):
    out = os.path.join(workdir, "%s.png" % stem)
    cur, res = _op(path, "/op/upscale", {"scale": scale, "out": out})
    return cur, res, _dims_of(cur, res)


def _replace_bg(job_id, path, workdir, stage, tag):
    out = os.path.join(workdir, "%s-%s-bg.png" % (tag, stage))
    cur, res = _op(path, "/op/replace_bg",
                   {"color": "#ffffff", "background": "#ffffff", "out": out})
    dims = _dims_of(cur, res)
    return cur, dims, [{"op": "replace_bg", "out": cur, "dims": list(dims)}]


def _inpaint_overlay(job_id, path, dims, workdir, stage, tag, boxes=None, subject=None):
    """文字区 → 纯 stdlib 掩码 PNG → 局部 inpaint（只改叠加文字，不重绘商品本体）。

    **只 inpaint 该擦的框**（`ruleset.overlay_boxes` / R3 evidence 的 `boxes`：覆盖主体的水印 +
    主体外促销条）：主体内的吊牌/型号码是商品自带信息，抹掉等于篡改商品本体（合规红线），必须排除。

    `boxes` 给了就直接用（调用方本轮已经测过 R3，别再跑一遍 OCR —— 实测一次 OCR ≈2.9 s）。
    """
    lines = []
    if boxes is None:
        ocr = gw("/detect/ocr", {"path": path, "lang": OCR_LANG})
        lines = ocr.get("lines") or []
        if not lines:
            return path, dims, []
    # 掩码必须与**这份文件**同宽高：宿主应答的尺寸可能是别的资产的（K07 A4 f002 实测
    # 掩码 818×818 对 1600×1600 的图 → `/op/inpaint` 400），所以现读一次文件尺寸。
    width, height = _dims_of(path) or dims
    width, height = int(width or 0), int(height or 0)
    if width <= 0 or height <= 0:
        raise RpcError(-32003, "inpaint 失败：读不到当前图的宽高（%s）" % path)
    if subject is None:
        subject = _subject_frame(path)
    if boxes is None:
        # 只在「需要自己算框」时才取主体框：boxes 已给（本轮 R3 判过）或压根没有文字时
        # 不花这一次 `/detect/bbox`（K07-FIX-D 的「无解规则不重问」口径）
        if subject is None:
            subject = _subject_frame(path)
        boxes = ruleset.overlay_boxes(lines, width=width, height=height,
                                     subject_bbox=subject.get("dominant_bbox"),
                                     subject_boxes=_instance_boxes(subject),
                                     min_text_chars=ruleset.THRESHOLDS["R3_no_watermark"]
                                     .get("min_text_chars", 2))
    boxes = [box for box in (boxes or []) if box]
    if not boxes:
        # 认出的字全在主体内（吊牌/型号码）→ 无可擦，别把商品擦花
        return path, dims, []
    mask_path = os.path.join(workdir, "%s-%s-mask.png" % (tag, stage))
    maskpng.write_mask_png(mask_path, width, height, boxes)
    out = os.path.join(workdir, "%s-%s-inpaint.png" % (tag, stage))
    cur, res = _op(path, "/op/inpaint",
                   {"mask": mask_path, "out": out, "allow_redraw_subject": False})
    dims = _dims_of(cur, res)
    return cur, dims, [
        {"op": "inpaint", "mask": mask_path, "mask_dims": [width, height],
         "boxes": len(boxes), "ocr_lines": len(lines or []), "out": cur,
         "dims": list(dims)}]


def _stage_copy(job_id, path, dims, workdir, stage, tag):
    """把当前版本钉成「名字里带资产前缀 + 尺寸」的稳定文件（复判走它，可人工点开核对）。

    文件名只用 `<tag>_<stage>_<W>x<H>.png`，**不继承源文件名**：复判拿到的尺寸必须是这份
    新文件自己的，而不是上游名字里的旧值（否则修完还在按旧尺寸/旧占比判，永远不收敛）。
    """
    width, height = dims
    if not width or not height:
        meta = gw("/io/read", {"path": path})
        width, height = meta.get("width"), meta.get("height")
    out = os.path.join(workdir, "%s_%s_%dx%d.png" % (tag, stage, int(width), int(height)))
    shutil.copyfile(path, out)
    return out, (int(width), int(height))


def _artifact(job_id, path, dims, workdir, rule_id, tag):
    """成品另存为 `final-<rule>_<W>x<H>.png`：**中间件（-1crop/-2letterbox/-3upscale）
    永不作为成品返回**，落库前再用文件实测尺寸复核一遍声明尺寸。"""
    width, height = int(dims[0]), int(dims[1])
    out = os.path.join(workdir, "%s-final-%s_%dx%d.png" % (tag, _safe(rule_id), width, height))
    shutil.copyfile(path, out)
    real = _dims_of(out)
    if real != (width, height):
        _trace(workdir, ev="artifact_dims_mismatch", asset=tag, rule=rule_id,
               declared=[width, height], actual=list(real))
        width, height = real
        out = os.path.join(workdir,
                           "%s-final-%s_%dx%d.png" % (tag, _safe(rule_id), width, height))
        shutil.copyfile(path, out)
    return {"rule_id": rule_id, "path": out, "width": width, "height": height,
            "kind": "image/png"}


def _test_hook(job_id, path):
    """conformance 断言 10（取消语义）需要一个「judge 仍在跑」的窗口。

    套件夹具用文件名 `slow` 约定制造该窗口（`tools/pack-conformance/gateway.py` 文档同款）；
    真实资产名不会命中，命中也只是「边等边看取消标记」，不会改变任何判定结果。
    """
    if "slow" not in os.path.basename(path).lower():
        return
    deadline = time.time() + 2.0
    while time.time() < deadline:
        _check_cancel(job_id, "judge")
        time.sleep(0.05)


def _with_reason_code(verdicts):
    """给判定行补 `reasonCode`（= 规则 id）。

    N9 口径：结果行的 `verdict{status, confidence, reasonCode}` 里 `reasonCode` 恒等
    **规则 id** —— 规格只改阈值与启用集，**不改 ruleId**，所以同一平台/跨平台的结论
    永远能按同一个机读码对账。人读原因仍在 `label`/`note`（不进机读码）。
    """
    out = []
    for v in verdicts:
        row = dict(v)
        row.setdefault("reasonCode", row.get("ruleId"))
        out.append(row)
    return out


def run(job_id, asset, rule_ids=None, options=None):
    """v2.0 单方法 `run`：fix_policy 门控「只判」与「判+修」两段（协议 §3.3）。

    宿主先判后修 = 两次 run 各自 job：判定调用带 `options.fix_policy="judge_only"`
    （只判不修），修复调用不注 judge_only（包按自己的修复策略执行修复段）。
    """
    options = options or {}
    if str(options.get("fix_policy") or "").strip() == "judge_only":
        return judge(job_id, asset, rules=rule_ids, options=options)
    return repair(job_id, asset, rules=rule_ids, options=options)


def judge(job_id, asset, rules=None, options=None):
    path = _require_path(asset)
    rule_ids = _normalize_rules(rules)
    options = options or {}
    _test_hook(job_id, path)
    # 一次读：宽高 + 边缘白度（R2 走 classic，不再为白底单独问一次 VL）
    info = gw("/io/read", {"path": path, "measure": ["white"]})
    verdicts = measure(job_id, path, rule_ids, options, info=info)
    ordered = [verdicts[rid] for rid in rule_ids if rid in verdicts]
    _emit(job_id, asset, ordered)
    return {"asset_id": str(asset.get("id") or os.path.basename(path)),
            "verdicts": _with_reason_code(ordered)}


def repair(job_id, asset, rules=None, options=None):
    path = _require_path(asset)
    options = options or {}
    rule_ids = _normalize_rules(rules)
    workdir = _workdir(options)
    fixable = [rid for rid in rule_ids if rid in ruleset.FIXABLE]
    if not fixable:
        return {"asset_id": str(asset.get("id") or os.path.basename(path)),
                "verdicts": [ruleset.skipped(rid, "fix_policy:%s" % ruleset.FIX_POLICY,
                                             label="not_fixable")
                             for rid in rule_ids],
                "artifacts": []}

    info = gw("/io/read", {"path": path, "measure": ["white"]})
    dims = (info.get("width") or 0, info.get("height") or 0)
    tag = _asset_tag(asset, path)
    cur, dims = _stage_copy(job_id, path, dims, workdir, "r0-input", tag)
    _trace(workdir, ev="start", asset=tag, path=path, dims=list(dims),
           rules=rule_ids, thresholds=options.get("thresholds"),
           inpaint=options.get("allow_inpaint", True))
    first = {}
    latest = {}          # 每条规则**最近一次**实测结论（增量复判只覆盖受影响的规则）
    trail = []
    unfixable = {}
    allow_inpaint = options.get("allow_inpaint", True)
    # 第 1 轮必须全量（每条规则都要有首判）；之后只复判「被上一轮改动影响」的规则。
    dirty = list(rule_ids)
    # K07-FIX-D：同一算子重复做且结论没变 → 判定「修不动」，不再烧第二个（昂贵的）算子。
    # 旧实现会把「inpaint 一遍没去掉文字」重复到 3 轮上限：实测每次 inpaint 7.6 s、
    # 每次 OCR 2.9 s（`DAMPACK_GATEWAY_STATS=1`），一轮空转就是十几秒。
    retry_sig = {}

    for round_no in range(1, ruleset.MAX_ROUNDS + 1):
        _check_cancel(job_id, "round%d" % round_no)
        current = measure(job_id, cur, dirty, options,
                          info=info if round_no == 1 else None,
                          respect_deps=False)
        info = None          # 首轮复用 repair 入口拿到的那份元信息，之后按需现取
        latest.update(current)
        _trace(workdir, ev="measure", round=round_no, path=cur, dims=list(dims),
               status={k: (v or {}).get("status") for k, v in current.items()},
               metrics={k: {m: (v.get("evidence") or {}).get(m)
                            for m in ("fg_ratio", "dominant_bbox", "white_dev")}
                        for k, v in current.items()})
        if round_no == 1:
            first = {rid: dict(v) for rid, v in current.items()}
        # fail 判定按**每条规则最近一次**结论看（没被改动影响的规则沿用上一轮结论）
        pending = [rid for rid in fixable
                   if latest.get(rid, {}).get("status") == "fail"]
        if not pending:
            dirty = []           # 这一轮已按最新版本判完，收尾不必再复核
            break

        stage = "r%d" % round_no
        applied = []
        touched = set()      # 本轮哪些规则被改动波及（下一轮增量复判范围）
        geo_rules = [rid for rid in pending
                     if rid in ("R1_size", "R4_subject_ratio")
                     and rid not in unfixable]
        if geo_rules:
            # 主体几何锚点：协议 1.1 的 /detect/bbox 一次给占比 + 主体紧框 + 目标裁剪窗
            # （旧宿主没有该端点 → 退回 /detect/u2net 的居中推定，行为与 1.0 一致）
            th = ruleset.thresholds_for("R4_subject_ratio", options.get("thresholds"))
            size_th = ruleset.thresholds_for("R1_size", options.get("thresholds"))
            target = th.get("target", 0.75)
            seg = _subject_frame(cur, target)
            plan = geometry.solve(
                dims[0], dims[1], seg.get("fg_ratio"),
                window=seg.get("window"),
                bbox=seg.get("dominant_bbox"),
                target_ratio=target,
                ratio_min=th.get("min", 0.55),
                ratio_max=th.get("max", 0.85),
                short_side_min=size_th.get("short_side_min", 1600),
                # 白底规则**已知**（pass 或 fail）就要留背景边：待修时是给它翻盘的机会，
                # 已过时是**保护已过条件**不被这一刀裁没（f002 实证：第 2 轮把白边裁掉，
                # 第 3 轮 R2 从 pass 变 fail，而 replace_bg 已重试过 → 直接判不收敛）。
                white_margin=(latest.get("R2_white_bg") or {}).get("status") in ("pass", "fail"),
            )
            sig = ("geometry", tuple(plan.get("crop_box") or ()),
                   round(float(plan.get("scale") or 1.0), 6),
                   bool(geometry.needs_letterbox(plan.get("crop_box"), dims[0] or 0,
                                                 dims[1] or 0)))
            prev = retry_sig.get("geometry")
            # 只在「上一轮这一套几何算子**没改变画布**」时才判定不动点：
            # 画布变了说明真裁/真放大过，下一轮的解是解在新画布上的（收敛靠它，不许提前收手）。
            if plan.fixable and prev and prev[0] == sig and prev[1] == prev[2]:
                for rid in geo_rules:
                    unfixable[rid] = unfixable.get(rid) or "repair_not_converged"
                trail.append({"round": round_no, "rule": "geometry",
                              "reason": "repair_not_converged"})
            elif plan.fixable:
                dims_before = dims
                cur, dims, ops = _apply_geometry(job_id, cur, plan, dims, workdir,
                                                 stage, tag,
                                                 short_side_min=size_th.get("short_side_min", 1600))
                _trace(workdir, ev="geometry", asset=tag, round=round_no, plan=dict(plan),
                       dims_before=list(dims_before), dims_after=list(dims), ops=ops,
                       short_min=size_th.get("short_side_min", 1600))
                applied.append({"rule": "geometry", "plan": dict(plan), "ops": ops})
                retry_sig["geometry"] = (sig, dims_before, dims)
                for op in ops:
                    touched.update(ruleset.affected_rules([op.get("op")]))
                if ops:
                    # 几何算子动过画布 → 之前那一刀 replace_bg / inpaint 做在**别的画布**上，
                    # 「同一算子做过一遍、结论没变」的前提不成立，必须允许在新画布上重做：
                    # f002 实测：第 1 轮换底后白底已 pass（边带 0.0），第 2 轮 crop 把主体贴到
                    # 画布边缘 → 边带重新带上主体像素（white_dev 22.57）→ 若拿第 1 轮的
                    # replace_bg 记录判 `repair_not_converged`，交付的就是「把已过条件弄坏」的图。
                    retry_sig.pop("R2_white_bg", None)
                    retry_sig.pop("R3_no_watermark", None)
            else:
                for rid in geo_rules:
                    unfixable[rid] = plan.get("unfixable")
                trail.append({"round": round_no, "unfixable": plan.get("unfixable"),
                              "reason": plan.get("reason")})
        if "R2_white_bg" in pending and "R2_white_bg" not in unfixable:
            if retry_sig.get("R2_white_bg") == ("replace_bg",):
                unfixable["R2_white_bg"] = "repair_not_converged"
                trail.append({"round": round_no, "rule": "R2_white_bg",
                              "reason": "repair_not_converged"})
            else:
                cur, dims, ops = _replace_bg(job_id, cur, workdir, stage, tag)
                applied.append({"rule": "R2_white_bg", "ops": ops})
                retry_sig["R2_white_bg"] = ("replace_bg",)
                for op in ops:
                    touched.update(ruleset.affected_rules([op.get("op")]))
        if "R3_no_watermark" in pending and allow_inpaint \
                and "R3_no_watermark" not in unfixable:
            if retry_sig.get("R3_no_watermark") == ("inpaint",):
                unfixable["R3_no_watermark"] = "repair_not_converged"
                trail.append({"round": round_no, "rule": "R3_no_watermark",
                              "reason": "repair_not_converged"})
            else:
                # 复用本轮 R3 已经拿到的 OCR 框（同一份文件别再跑一遍 OCR）——
                # 但**只在画布没变时**才敢复用：框是在判定那份文件上量的，几何算子改过画布后
                # 旧坐标会整块落在画布外 → 掩码被夹成空白 → 真网关 `/op/inpaint` 500
                # 「掩码全空：无待补洞区域」（K07-FIX-G-B f002 实测：evidence 框在 3840×2564
                # 上量，r3 画布 1688×1688，x 到 3492 全被 `x2 < x1` 跳过 → 掩码 0 像素）。
                # 画布变了就退回复用不了的老路：在当前文件上重测 OCR（多花 ≈2.9s，换正确坐标）。
                boxes = None
                if "R3_no_watermark" in current:
                    ev = (latest.get("R3_no_watermark", {}).get("evidence") or {})
                    ev_dims = ev.get("dims")
                    now_dims = _dims_of(cur) or dims
                    if ev_dims and _same_dims(ev_dims, now_dims):
                        boxes = ev.get("boxes")
                cur, dims, ops = _inpaint_overlay(job_id, cur, dims, workdir, stage,
                                                  tag, boxes=boxes)
                if ops:
                    applied.append({"rule": "R3_no_watermark", "ops": ops})
                    retry_sig["R3_no_watermark"] = ("inpaint",)
                    for op in ops:
                        touched.update(ruleset.affected_rules([op.get("op")]))
        if not applied:
            dirty = []                 # 这一轮什么也没做 → 不再空转，也不必复核
            break
        cur, dims = _stage_copy(job_id, cur, dims, workdir, stage, tag)
        for item in applied:
            item["round"] = round_no
            item["output"] = cur
            trail.append(item)
        # 下一轮：只复判受影响的规则（顺序仍按包内 pipeline 声明序）
        dirty = [rid for rid in rule_ids if rid in touched] or []

    # 收尾：把「还没在最新版本上复判过」的规则补齐，范围是**最小集**：
    #   ① 上一轮改动波及、但轮次已用尽的规则（`dirty`）；
    #   ② 最近一次仍 fail 的规则 —— 中途的 crop/换底可能顺带把它修好了，
    #      不复判就会把「其实已经合格」的图误报成 needs_human（保守方向之外的那一半）。
    # 已判 undetermined 的不再重问（同一张图同一 prompt，重问只会烧调用）。
    stale = [rid for rid in dirty if rid in rule_ids]
    for rid in rule_ids:
        if rid in stale or rid in unfixable:
            continue
        if latest.get(rid, {}).get("status") == "fail":
            stale.append(rid)
    if stale:
        _check_cancel(job_id, "final")
        confirm = measure(job_id, cur, [rid for rid in rule_ids if rid in stale],
                          options, respect_deps=False)
        latest.update(confirm)
    final = latest

    verdicts = []
    artifacts = []
    for rid in rule_ids:
        before = first.get(rid) or {}
        after = final.get(rid) or {}
        if rid not in fixable:
            verdicts.append(after)
            continue
        if before.get("status") == "fail" and after.get("status") == "pass":
            artifacts.append(_artifact(job_id, cur, dims, workdir, rid, tag))
            _trace(workdir, ev="artifact", asset=tag, rule=rid, dims=list(dims),
                   path=artifacts[-1]["path"])
            verdict = dict(after)
            verdict["fixed"] = True
            verdict["evidence"] = dict(after.get("evidence") or {},
                                       before=before.get("evidence"),
                                       artifact=artifacts[-1]["path"],
                                       rounds=len(trail) and max(
                                           (item.get("round") or 0) for item in trail))
            verdict["note"] = None
            verdicts.append(verdict)
        elif before.get("status") == "fail" or after.get("status") == "fail":
            verdict = dict(after)
            verdict["fixed"] = False
            reason = unfixable.get(rid) or after.get("note") or "repair_not_converged"
            verdict["note"] = "needs_human:%s" % reason
            verdict["evidence"] = dict(after.get("evidence") or {},
                                       before=before.get("evidence"),
                                       rounds=ruleset.MAX_ROUNDS,
                                       reason=reason)
            if after.get("status") not in ("fail", "undetermined"):
                verdict["status"] = "undetermined"
            verdicts.append(verdict)
        else:
            verdicts.append(after)
    return {"asset_id": str(asset.get("id") or os.path.basename(path)),
            "verdicts": verdicts, "artifacts": artifacts, "trail": trail}


# --- 报告 / 交付 ------------------------------------------------------------
def _emit(job_id, asset, verdicts):
    entry = {
        "asset_id": str(asset.get("id") or ""),
        "path": str(asset.get("path") or ""),
        "pack": PACK_ID,
        "pack_version": PACK_VERSION,
        "verdicts": [{"ruleId": v["ruleId"], "status": v["status"],
                      "score": v.get("score")} for v in verdicts],
    }
    with _STATE_LOCK:
        _REPORTS.setdefault(job_id, []).append(entry)
    try:
        gw("/report/emit", {"job_id": job_id, "entry": entry})
    except RpcError as exc:
        log("report/emit 失败（不影响判定结果）：", exc.code, exc.message)
        raise


def report(job_id, fmt="json", options=None):
    with _STATE_LOCK:
        entries = [dict(item) for item in (_REPORTS.get(job_id) or [])]
    assets = [entry.get("asset_id") for entry in entries]
    payload = {
        "job_id": job_id,
        "rows": sum(len(entry.get("verdicts") or []) for entry in entries),
        "assets": assets,
        "entries": [{"assetId": entry.get("asset_id"),
                     "verdicts": entry.get("verdicts") or []} for entry in entries],
    }
    fmt = (fmt or "json").lower()
    if fmt == "csv":
        rows = ["asset_id,ruleId,status,score"]
        for entry in entries:
            for verdict in entry.get("verdicts") or []:
                rows.append("%s,%s,%s,%s" % (entry.get("asset_id"), verdict.get("ruleId"),
                                             verdict.get("status"), verdict.get("score")))
        payload["format"] = "csv"
        payload["content"] = "\n".join(rows) + "\n"
    return payload



def health(params):
    return {"status": "ok", "pack": PACK_ID, "version": PACK_VERSION,
            "api_version": API_VERSION}


HANDLERS = {
    "pack.info": lambda params: dict(PACK_INFO),
    "health": health,
    "run": lambda params: run(params.get("job_id") or "", params.get("asset") or {},
                              params.get("rules"), params.get("options") or {}),
    "report": lambda params: report(params.get("job_id") or "",
                                    params.get("format") or "json",
                                    params.get("options") or {}),
    "cancel": lambda params: _drop_job(params.get("job_id") or ""),
}

# 卡面要求导出的语义别名（宿主走 HANDLERS 即可）：RUN = 判定/修复二段合一的入口
RUN = HANDLERS["run"]

UNKNOWN_METHOD_CODE = -32601
_READ_LOCK = threading.Lock()


class _CancelRpc(Exception):
    pass


def _handle_line(line):
    """一条协议行 → 一帧响应（通知帧返回 None）。"""
    try:
        frame = json.loads(line)
    except Exception:
        return {"jsonrpc": "2.0", "id": None,
                "error": {"code": -32700, "message": "JSON 解析失败"}}
    if not isinstance(frame, dict):
        return {"jsonrpc": "2.0", "id": None,
                "error": {"code": -32600, "message": "帧必须是对象"}}
    method = frame.get("method")
    params = frame.get("params") or {}
    frame_id = frame.get("id")
    if method == "cancel":
        job_id = str(params.get("job_id") or "")
        flag = _cancel_flag(job_id)
        flag.set()
        if frame_id is None:            # 通知帧
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "result": {"canceled": True, "job_id": job_id or None}}
    handler = HANDLERS.get(method)
    if handler is None:
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": UNKNOWN_METHOD_CODE,
                          "message": "未知方法：%s" % method}}
    try:
        result = handler(params if isinstance(params, dict) else {})
    except RpcError as exc:
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": exc.code, "message": exc.message}}
    except Exception as exc:            # 插件自身 bug 一律 -32000，绝不静默成空结果
        log("方法 %s 异常：%r" % (method, exc))
        if frame_id is None:
            return None
        return {"jsonrpc": "2.0", "id": frame_id,
                "error": {"code": -32000, "message": "插件内部错误：%r" % (exc,)}}
    if frame_id is None:
        return None
    return {"jsonrpc": "2.0", "id": frame_id, "result": result}


def _write(frame):
    sys.stdout.write(json.dumps(frame, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def serve(stdin=None, stdout=None):
    """主循环：读行即派发到线程（长任务不能挡住 cancel）。"""
    stdin = stdin or sys.stdin
    stdout = stdout or sys.stdout
    workers = []

    def run(line, out):
        frame = _handle_line(line)
        if frame is not None:
            with _READ_LOCK:
                out.write(json.dumps(frame, ensure_ascii=False) + "\n")
                out.flush()

    log("ready", PACK_ID, PACK_VERSION, "api", API_VERSION)
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
