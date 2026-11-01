#!/usr/bin/env python3
"""房产行业包（官方包）—— 插件入口（`docs/industry-pack-protocol.md` v1.1）。

启动形态：宿主 `pack-host` 以 stdio 行式 JSON-RPC 拉起本进程
（`python3 plugin/__init__.py`，cwd=包目录，env 带 `DAMPACK_GATEWAY_URL/TOKEN`）。

本文件三件事：
1. **协议壳**：收行 → 校验 → 派发 `HANDLERS` → 回帧；stdout 只走协议行，日志一律 stderr。
2. **编排**：`judge`（F0 领域 gate 短路 + 逐图规则 + 目录组规则）与
   `repair`（**只做几何**：短边不足 → `upscale`；其余失败一律转人工，不自动放行）。
3. **能力调用**：网关是唯一对外出口（`gw()`）；`report/emit` 账本按 job 隔离。

与 `packs/realestate/*.yaml`（旧引擎声明）的差异只有三处，全部写在 README
「与旧引擎的差异」一节：F1/F2 的像素测量在包内算（网关 v1 无 classic 端点）、
F3/F4 的组作用域由插件按目录自聚合（协议 judge 是逐资产调用）、新增 F5_min_size
（卡面要求本包具备「仅几何修复」能力面，而 F0~F4 的失败都不可算法修复）。
"""
from __future__ import annotations

import json
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request

API_VERSION = "2.0"
PACK_VERSION = "2.0.0"
PACK_ID = "com.soujpg.pack.realestate"

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import imagedec          # noqa: E402
import ruleset           # noqa: E402

RpcError = ruleset.RpcError
VL_MAX_TOKENS = 4096     # 协议 §6 上限；思考型模型低于此会 content 空
IMAGE_EXTS = (".jpg", ".jpeg", ".png", ".webp", ".bmp", ".tif", ".tiff")
CANCEL_POLL_SECS = 0.05
SLOW_HOOK_SECS = 10.0    # conformance 取消断言用的长任务桩上限
MAX_HASHABLE_MEMBERS = 24
CACHE_FILE = "realestate-cache.json"

PACK_INFO = {
    "id": PACK_ID,
    "name": "房产房源照片合规（房源图入库前审）",
    "version": PACK_VERSION,
    "api_version": API_VERSION,
    "rules": [dict(rule) for rule in ruleset.RULES],
    "capabilities": [
        "io.read",
        "op.geometric",
        "vl.judge",
        "report.emit",
    ],
    # 含 VL 规则（F0 领域 gate + F4 场景齐套）→ 整包按协议 §7 铁律 2 声明非确定；
    # 逐规则确定性看 rules[].deterministic（F1/F2/F3/F5 = true）。
    "deterministic": False,
    "timeout_secs": 180,
    "resources": ["resources/rules.json", "resources/scenes.json"],
}

# --- 运行时状态（进程内；按 job 隔离） ---------------------------------------
_STATE_LOCK = threading.Lock()
_CANCEL = {}
_REPORTS = {}
_SAFE = re.compile(r"[^0-9A-Za-z_.-]+")
_CACHE_LOCK = threading.Lock()
_MEM_CACHE = {"phash": {}, "scene": {}, "domain": {}}


def log(*parts):
    """日志只走 stderr：stdout 是协议通道，一行都不能脏。"""
    sys.stderr.write("[realestate] " + " ".join(str(p) for p in parts) + "\n")
    sys.stderr.flush()


def _safe(name, fallback="asset"):
    cleaned = _SAFE.sub("_", str(name or "")).strip("_")
    return (cleaned or fallback)[:64]


# --- 网关客户端（唯一能力出口） ----------------------------------------------
_GATEWAY_URL = (os.environ.get("DAMPACK_GATEWAY_URL") or "").rstrip("/")
_GATEWAY_TOKEN = os.environ.get("DAMPACK_GATEWAY_TOKEN") or ""


def _http_timeout():
    raw = os.environ.get("DAMPACK_TIMEOUT_SECS")
    try:
        return max(5.0, float(raw)) if raw else 120.0
    except ValueError:
        return 120.0


def gw(path, payload):
    """调网关端点；错误映射严格按协议 §5（不吞 403，不重试）。"""
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
        detail = ""
        try:
            detail = json.loads(exc.read().decode("utf-8")).get("error") or ""
        except Exception:
            detail = ""
        if exc.code == 401:
            raise RpcError(-32000, "网关鉴权失败（401）%s" % detail)
        if exc.code == 403:
            raise RpcError(-32001, "能力未授权（403）%s" % detail)
        if exc.code == 404:
            raise RpcError(-32002, "资产不可读（404）%s" % detail)
        if exc.code == 400:
            raise RpcError(-32003, "参数非法（400）%s" % detail)
        raise RpcError(-32000, "网关错误 %s %s" % (exc.code, detail))
    except urllib.error.URLError as exc:
        raise RpcError(-32000, "网关不可达：%s" % (exc.reason,))
    except (TimeoutError, OSError) as exc:
        raise RpcError(-32000, "网关调用失败：%r" % (exc,))


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


def _workdir(options, required=False):
    raw = (options or {}).get("workdir") or os.environ.get("DAMPACK_WORK_DIR")
    if not raw:
        if required:
            raise RpcError(-32003, "缺少 workdir（options.workdir 或 DAMPACK_WORK_DIR）")
        return ""
    os.makedirs(raw, exist_ok=True)
    return str(raw)


def _slow_hook(job_id, path):
    """conformance 取消断言桩：文件名带 `slow` 时原地等待，可被 cancel 打断。"""
    if "slow" not in os.path.basename(path).lower():
        return
    deadline = time.time() + SLOW_HOOK_SECS
    while time.time() < deadline:
        _check_cancel(job_id, "slow-asset")
        time.sleep(CANCEL_POLL_SECS)
    _check_cancel(job_id, "slow-asset")


# --- 组（目录）与测量缓存 ----------------------------------------------------
def _group_dir(path, options):
    override = (options or {}).get("group_dir")
    if override:
        return str(override)
    return os.path.dirname(os.path.abspath(path))


def _group_members(path, options):
    """组内成员 = 同目录下的图像文件（排序稳定）；当前资产排第一。"""
    directory = _group_dir(path, options)
    members = []
    try:
        names = sorted(os.listdir(directory))
    except OSError:
        return [path]
    for name in names:
        if not name.lower().endswith(IMAGE_EXTS):
            continue
        members.append(os.path.join(directory, name))
    if path not in members:
        members.insert(0, path)
    else:
        members.remove(path)
        members.insert(0, path)
    capped = members[:MAX_HASHABLE_MEMBERS]
    return capped


def _cache_key(path):
    try:
        stat = os.stat(path)
    except OSError:
        return ""
    return "%s|%d|%d" % (os.path.abspath(path), stat.st_size, int(stat.st_mtime))


def _cache_get(kind, path):
    key = _cache_key(path)
    if not key:
        return None
    with _CACHE_LOCK:
        value = _MEM_CACHE[kind].get(key)
    return value


def _cache_put(kind, path, value):
    key = _cache_key(path)
    if not key:
        return
    with _CACHE_LOCK:
        _MEM_CACHE[kind][key] = value


def _load_cache(workdir):
    """把落盘缓存并进进程内缓存（跨进程复用；缺失/损坏都只是没有缓存）。"""
    if not workdir:
        return
    path = os.path.join(workdir, CACHE_FILE)
    try:
        with open(path, encoding="utf-8") as fh:
            payload = json.load(fh)
    except (OSError, ValueError):
        return
    if not isinstance(payload, dict):
        return
    with _CACHE_LOCK:
        for kind in ("phash", "scene", "domain"):
            items = payload.get(kind) or {}
            if isinstance(items, dict):
                for key, value in items.items():
                    _MEM_CACHE[kind].setdefault(key, value)


def _flush_cache(workdir):
    if not workdir:
        return
    path = os.path.join(workdir, CACHE_FILE)
    with _CACHE_LOCK:
        payload = {kind: dict(sorted(_MEM_CACHE[kind].items())[-512:])
                   for kind in ("phash", "scene", "domain")}
    tmp = path + ".tmp"
    try:
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False)
        os.replace(tmp, path)
    except OSError as exc:
        log("缓存落盘失败（不影响判定）：", exc)


def _phash_of(path):
    """DC 档 luma → pHash；解不了返回 None（不猜）。"""
    cached = _cache_get("phash", path)
    if cached is not None:
        return cached
    try:
        width, height, plane = imagedec.decode_dc_luma(path)
    except imagedec.DecodeError:
        return None
    value = ruleset.phash(plane, width, height)
    _cache_put("phash", path, value)
    return value


def _scene_of(path):
    """VL 归类（缓存在 workdir）；VL 不可判返回 ""。"""
    cached = _cache_get("scene", path)
    if cached is not None:
        return cached
    reply = gw("/vl/judge", {"path": path,
                             "prompt": ruleset.VL_INTRO + ruleset.PROMPT_SCENE,
                             "max_tokens": VL_MAX_TOKENS})
    scene = ruleset.pick_scene(reply)
    if scene:
        _cache_put("scene", path, scene)
    return scene


# --- 单规则判定（judge / repair 复判共用同一实现，保证首判与复判同语义） -------
def _measure_luma(job_id, path, options):
    """解 luma（调用方已给测量值则不重算）；返回 (luma, w, h, 错误原因, 来源)。"""
    measures = (options or {}).get("measurements") or {}
    if measures.get("sharpness_variance") is not None:
        stats = measures.get("exposure") or {}
        return None, 0, 0, "", "host_measurements", measures
    try:
        width, height, luma = imagedec.decode_luma(path)
    except imagedec.DecodeError as exc:
        return None, 0, 0, str(exc), "", measures
    _check_cancel(job_id, "decoded")
    return luma, width, height, "", "in_pack_decode", measures


def _judge_sharpness(job_id, path, options, thresholds, luma_info):
    luma, width, height, error, source, measures = luma_info
    th = ruleset.thresholds_for("F1_sharpness", thresholds)
    if source == "host_measurements":
        pixels = measures.get("pixels") or (width * height)
        return ruleset.eval_sharpness(measures["sharpness_variance"], pixels,
                                      minimum=th.get("min", 100.0),
                                      source="host_measurements")
    if luma is None:
        return ruleset.undetermined("F1_sharpness", "asset_decode_failed",
                                    "laplacian_variance",
                                    {"decode_error": error, "source": "in_pack_decode"})
    try:
        variance, pixels = ruleset.laplacian_variance(luma, width, height)
    except ValueError as exc:
        return ruleset.undetermined("F1_sharpness", "asset_too_small", "laplacian_variance",
                                    {"detail": str(exc)})
    return ruleset.eval_sharpness(variance, pixels, minimum=th.get("min", 100.0))


def _judge_exposure(job_id, path, options, thresholds, luma_info):
    luma, width, height, error, source, measures = luma_info
    th = ruleset.thresholds_for("F2_exposure", thresholds)
    if source == "host_measurements":
        stats = measures.get("exposure") or {}
        return ruleset.eval_exposure(stats,
                                     max_dead_ratio=th.get("max_dead_ratio", 0.30),
                                     min_mean=th.get("min_mean", 30.0),
                                     max_mean=th.get("max_mean", 245.0),
                                     black_cut=th.get("black_cut", 10),
                                     white_cut=th.get("white_cut", 10),
                                     source="host_measurements")
    if luma is None:
        return ruleset.undetermined("F2_exposure", "asset_decode_failed",
                                    "dead_zone_ratio",
                                    {"decode_error": error, "source": "in_pack_decode"})
    try:
        stats = ruleset.exposure_stats(luma,
                                       black_cut=th.get("black_cut", 10),
                                       white_cut=th.get("white_cut", 10))
    except ValueError as exc:
        return ruleset.undetermined("F2_exposure", "asset_unmeasurable", "dead_zone_ratio",
                                    {"detail": str(exc)})
    return ruleset.eval_exposure(stats,
                                 max_dead_ratio=th.get("max_dead_ratio", 0.30),
                                 min_mean=th.get("min_mean", 30.0),
                                 max_mean=th.get("max_mean", 245.0),
                                 black_cut=th.get("black_cut", 10),
                                 white_cut=th.get("white_cut", 10))


def _judge_duplicate(job_id, path, options, thresholds, group):
    th = ruleset.thresholds_for("F3_duplicate", thresholds)
    own = _phash_of(path)
    if own is None:
        return ruleset.undetermined("F3_duplicate", "asset_decode_failed",
                                    "phash_distance",
                                    {"detail": "本资产解不出 pHash，无法与组内比对"})
    best = None
    nearest = ""
    compared = 0
    for member in group:
        if os.path.abspath(member) == os.path.abspath(path):
            continue
        _check_cancel(job_id, "phash")
        other = _phash_of(member)
        if other is None:
            continue
        compared += 1
        distance = ruleset.hamming(own, other)
        if best is None or distance < best:
            best = distance
            nearest = os.path.basename(member)
    if compared == 0:
        return ruleset.undetermined("F3_duplicate", "no_comparable_sibling",
                                    "phash_distance",
                                    {"group_size": len(group),
                                     "detail": "组内没有可比对的成员（解不出或只有本张）"})
    return ruleset.eval_duplicate(best, threshold=th.get("threshold", 8),
                                  sibling=nearest, compared=compared)


def _judge_scene(job_id, path, options, thresholds, group):
    th = ruleset.thresholds_for("F4_scene_complete", thresholds)
    require = th.get("require_all") or ruleset.REQUIRE_ALL_SCENES
    answers = {}
    failed = 0
    probed = 0
    gated = []
    for member in group:
        _check_cancel(job_id, "vl-scene")
        cached_domain = _cache_get("domain", member)
        if cached_domain is not None and cached_domain != "pass":
            gated.append(os.path.basename(member))     # C11：被 gate 掉的成员不参与聚合
            continue
        scene = _scene_of(member)
        if scene:
            answers[member] = scene
            probed += 1
        else:
            answers[member] = ""
            failed += 1
    verdict = ruleset.eval_scene_complete(
        answers, require_all=require, probed=probed,
        unprobed=0, failed=failed,
    )
    if gated:
        verdict["evidence"]["domain_gated"] = sorted(gated)
    return verdict


def _size_thresholds(thresholds):
    th = ruleset.thresholds_for("F5_min_size", thresholds)
    return th.get("long_side_min", 800), th.get("short_side_min", 600)


def _judge_size(job_id, path, options, thresholds, info):
    long_min, short_min = _size_thresholds(thresholds)
    meta = info or gw("/io/read", {"path": path})
    return ruleset.eval_size(meta.get("width") or 0, meta.get("height") or 0,
                            long_side_min=long_min, short_side_min=short_min)


def measure(job_id, path, rule_ids, options=None, info=None, respect_deps=True):
    """按 pipeline 声明序判一组规则，返回 {rule_id: verdict}（含 F0 短路）。"""
    options = options or {}
    thresholds = options.get("thresholds")
    verdicts = {}
    domain_status = "pass" if "F0_domain" not in rule_ids else None
    group = None
    luma_info = None
    for rid in rule_ids:
        _check_cancel(job_id, "rule:" + rid)
        if rid == "F0_domain":
            reply = gw("/vl/judge", {"path": path,
                                     "prompt": ruleset.VL_INTRO + ruleset.PROMPT_DOMAIN,
                                     "max_tokens": VL_MAX_TOKENS})
            verdict = ruleset.eval_domain(reply)
            domain_status = verdict["status"]
            verdicts[rid] = verdict
            # 记进缓存：组级规则（F3/F4）按 C11 口径**排除被 domain gate 拦掉的成员**
            _cache_put("domain", path, domain_status)
            continue
        if domain_status is not None and ruleset.domain_gate_blocks(domain_status):
            verdicts[rid] = ruleset.skipped(
                rid, "domain_gate_%s" % domain_status, label="short_circuit")
            continue
        if rid in ("F1_sharpness", "F2_exposure") and luma_info is None:
            luma_info = _measure_luma(job_id, path, options)
        if rid in ruleset.GROUP_RULES and group is None:
            group = _group_members(path, options)
        if rid == "F1_sharpness":
            verdicts[rid] = _judge_sharpness(job_id, path, options, thresholds, luma_info)
        elif rid == "F2_exposure":
            verdicts[rid] = _judge_exposure(job_id, path, options, thresholds, luma_info)
        elif rid == "F3_duplicate":
            verdicts[rid] = _judge_duplicate(job_id, path, options, thresholds, group)
        elif rid == "F4_scene_complete":
            verdicts[rid] = _judge_scene(job_id, path, options, thresholds, group)
        elif rid == "F5_min_size":
            verdicts[rid] = _judge_size(job_id, path, options, thresholds, info)
        else:
            verdicts[rid] = ruleset.skipped(rid, "unknown_rule", label="unknown")
        if not respect_deps:
            continue
    return verdicts


# --- 账本 --------------------------------------------------------------------
def _emit(job_id, asset, verdicts):
    entry = {
        "asset_id": str(asset.get("id") or os.path.basename(str(asset.get("path") or ""))),
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


def _summary(verdicts):
    counts = {"pass": 0, "fail": 0, "undetermined": 0, "skipped": 0}
    for verdict in verdicts:
        status = verdict.get("status")
        if status in counts:
            counts[status] += 1
    counts["manual_review"] = sum(
        1 for v in verdicts
        if v.get("status") == "undetermined" or v.get("label") == "manual_review_required")
    counts["soft_marks"] = [v["ruleId"] for v in verdicts
                            if (v.get("evidence") or {}).get("soft_rule")
                            and v.get("status") == "fail"]
    return counts


# --- 方法实现 ----------------------------------------------------------------
def run(job_id, asset, rule_ids=None, options=None):
    """v2.0 单方法 `run`：fix_policy 门控「只判」与「判+修」两段（协议 §3.3）。

    宿主先判后修 = 两次 run 各自 job：判定调用带 `options.fix_policy="judge_only"`
    （只判不修），修复调用不注 judge_only（包按自己的修复策略执行修复段）。
    """
    options = options or {}
    if str(options.get("fix_policy") or "").strip() == "judge_only":
        return judge(job_id, asset, rule_ids=rule_ids, options=options)
    return repair(job_id, asset, rule_ids=rule_ids, options=options)


def judge(job_id, asset, rule_ids=None, options=None):
    path = _require_path(asset)
    options = options or {}
    workdir = _workdir(options)
    _load_cache(workdir)
    info = gw("/io/read", {"path": path})     # 不可读 → -32002（协议 §5）
    _slow_hook(job_id, path)
    rules = _normalize_rules(rule_ids)
    verdicts = measure(job_id, path, rules, options, info=info)
    ordered = [verdicts[rid] for rid in rules if rid in verdicts]
    _check_cancel(job_id, "emit")
    _emit(job_id, asset, ordered)
    _flush_cache(workdir)
    return {"job_id": job_id, "asset_id": str(asset.get("id") or ""),
            "verdicts": ordered, "summary": _summary(ordered),
            "deterministic": PACK_INFO["deterministic"]}


def _upscale_plan(width, height, long_side_min, short_side_min):
    """几何放大的闭式解：一次放大同时满足长边与短边两条门（禁生成式）。"""
    width = int(width)
    height = int(height)
    if width <= 0 or height <= 0:
        return None
    short = min(width, height)
    long_side = max(width, height)
    need = max(long_side_min / float(long_side), short_side_min / float(short))
    if need <= 1.0:
        return None
    scale = min(4.0, int(need * 100 + 0.999) / 100.0)   # 向上取到 0.01
    target_w = int(round(width * scale))
    target_h = int(round(height * scale))
    while min(target_w, target_h) < short_side_min or max(target_w, target_h) < long_side_min:
        scale = round(scale + 0.01, 2)
        target_w = int(round(width * scale))
        target_h = int(round(height * scale))
        if scale >= 4.0:
            break
    return scale, target_w, target_h


def repair(job_id, asset, rule_ids=None, options=None):
    """几何修复：只对**声明了几何修复器**的失败规则做 upscale，其余转人工。"""
    path = _require_path(asset)
    options = options or {}
    workdir = _workdir(options, required=True)
    _load_cache(workdir)
    info = gw("/io/read", {"path": path})
    _slow_hook(job_id, path)
    rules = _normalize_rules(rule_ids)
    verdicts = measure(job_id, path, rules, options, info=info, respect_deps=False)
    artifacts = []
    fixed_ids = set()
    rounds = 0

    size_verdict = verdicts.get("F5_min_size")
    if size_verdict and size_verdict.get("status") == "fail":
        long_min, short_min = _size_thresholds(options.get("thresholds"))
        plan = _upscale_plan(info.get("width") or 0, info.get("height") or 0,
                             long_min, short_min)
        if plan:
            rounds += 1
            scale, target_w, target_h = plan
            _check_cancel(job_id, "upscale")
            # 产物名把**目标尺寸放最前**：conformance 假网关的 io.read 按文件名首个
            # `WxH` 回尺寸，而真实网关回文件元数据 —— 前者要求尺寸在名字开头才对得上账。
            out = os.path.join(workdir, "%dx%d_%s_F5_min_size.png"
                               % (target_w, target_h,
                                  _safe(asset.get("id") or os.path.basename(path))))
            try:
                result = gw("/op/upscale", {"path": path, "scale": scale, "out": out})
            except RpcError as exc:
                log("upscale 失败：", exc.code, exc.message)
                result = {}
            out_path = str(result.get("path") or "")
            if out_path and os.path.isfile(out_path):
                meta = gw("/io/read", {"path": out_path})
                rejudge = ruleset.eval_size(meta.get("width") or result.get("width") or 0,
                                            meta.get("height") or result.get("height") or 0,
                                            long_side_min=long_min,
                                            short_side_min=short_min)
                if rejudge["status"] == "pass":
                    rejudge["fixed"] = True
                    rejudge["evidence"] = dict(rejudge.get("evidence") or {},
                                               artifact=out_path, scale=scale,
                                               source="geometric_upscale")
                    verdicts["F5_min_size"] = rejudge
                    fixed_ids.add("F5_min_size")
                    artifacts.append({
                        "rule_id": "F5_min_size",
                        "path": out_path,
                        "width": int(meta.get("width") or result.get("width") or 0),
                        "height": int(meta.get("height") or result.get("height") or 0),
                        "kind": "geometric_upscale",
                        "op": "upscale",
                    })
                else:
                    verdicts["F5_min_size"] = rejudge

    # 不可修复的失败（F0~F4）一律转人工：undetermined + 原因，不自动放行
    for rid, verdict in list(verdicts.items()):
        if verdict.get("status") != "fail" or rid in fixed_ids:
            continue
        meta = ruleset.rule_meta(rid)
        if meta.get("fixer"):
            continue
        if (verdict.get("evidence") or {}).get("soft_rule"):
            continue        # F3 软规则：只标记，不转人工
        verdicts[rid] = ruleset.needs_human_verdict(
            rid, verdict.get("note") or "unrepairable")

    ordered = [verdicts[rid] for rid in rules if rid in verdicts]
    _check_cancel(job_id, "emit")
    _emit(job_id, asset, ordered)
    _flush_cache(workdir)
    return {"job_id": job_id, "asset_id": str(asset.get("id") or ""),
            "verdicts": ordered, "artifacts": artifacts, "rounds": rounds,
            "fix_policy": ruleset.FIX_POLICY,
            "summary": _summary(ordered)}


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

# 卡面要求导出的两个符号（语义别名，宿主走 HANDLERS 即可）
# 语义别名（宿主走 HANDLERS 即可）：RUN = 判定/修复二段合一的入口
RUN = HANDLERS["run"]

UNKNOWN_METHOD_CODE = -32601
_READ_LOCK = threading.Lock()


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
        _cancel_flag(job_id).set()
        if frame_id is None:
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
