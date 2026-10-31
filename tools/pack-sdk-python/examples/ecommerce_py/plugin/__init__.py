#!/usr/bin/env python3
"""电商样例包（SDK 版）—— 插件入口。

演示「第三方用 Python 写行业包」的**完整最小工程**：
    manifest.json + plugin/__init__.py（本文件）+ plugin/rules.py（纯判定）
    + requirements.txt（SDK 依赖）+ resources/rules.json
宿主 `pack-host` 以 `python -m dampack_launcher <pack_dir>` 拉起本进程（也可直跑本文件），
stdio 行式 JSON-RPC，能力全过网关（`pack.gateway`）。

与官方包 `packs/plugins/ecommerce` 的关系：
    规则/阈值/编排语义**逐条对齐**（同一套 R0..R5、同一套短路与修复策略），
    区别只在「协议壳」：官方包自己实现 dispatch/cancel/report 账本，
    本包全部交给 SDK（`PackServer`），所以本文件里看不到一行 JSON-RPC 处理代码。

判定与 Rust 旧引擎的逐图对账（10 张语料）：
    `python3 tools/pack-sdk-python/scripts/consistency_10.py`
"""
from __future__ import annotations

import json
import os
import shutil
import sys
import time

_HERE = os.path.dirname(os.path.abspath(__file__))
# 仓内直跑（未建包内 venv）时按仓库布局找 SDK：examples/ecommerce_py/plugin → tools/pack-sdk-python
try:
    import dampack  # noqa: F401
except ImportError:  # pragma: no cover - 只在「没装 SDK」的开发态触发
    _sdk_root = os.environ.get("DAMPACK_SDK_PATH") or os.path.abspath(
        os.path.join(_HERE, "..", "..", ".."))
    sys.path.insert(0, _sdk_root)
    import dampack  # noqa: F401

if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import geometry   # noqa: E402
import maskpng    # noqa: E402
import rules      # noqa: E402
from dampack import (   # noqa: E402
    ERR_INVALID_PARAMS,
    PackServer,
    RpcError,
    log,
)

PACK_ID = "com.soujpg.pack.ecommerce_py"
PACK_NAME = "电商主图合规（Python SDK 样例包）"
PACK_VERSION = "1.0.0"
API_VERSION = dampack.API_VERSION

VL_MAX_TOKENS = 4096          # 协议 §6 上限；思考型模型低于此会 content 空
OCR_LANG = "chi_sim+eng"

CAPABILITIES = [
    "io.read",
    "detect.u2net",
    "detect.ocr",
    "op.geometric",
    "op.generative",
    "vl.judge",
    "report.emit",
]

# 协议壳全部由 SDK 接管：pack.info / health / cancel / report 自动就位
pack = PackServer(
    pack_id=PACK_ID,
    name=PACK_NAME,
    version=PACK_VERSION,
    rules=rules.RULES,
    capabilities=CAPABILITIES,
    deterministic=False,      # 含 VL 规则 → 整包不声明确定性（确定性逐规则看 pack.info.rules）
    timeout_secs=90,
    resources=["resources/rules.json"],
)

PACK_INFO = pack.pack_info()  # 供 launcher --probe / 单测 / 宿主诊断读取


def gw():
    """能力网关客户端（唯一重活出口；SDK 负责错误码映射与回环免代理）。"""
    return pack.gateway


# --- 参数校验 ----------------------------------------------------------------
def _require_path(asset):
    if not isinstance(asset, dict) or not asset.get("path"):
        raise RpcError(ERR_INVALID_PARAMS, "asset.path 必填")
    return str(asset["path"])


def _normalize_rules(rule_ids):
    """请求的规则集：缺省 = 全部；顺序按包内 pipeline 声明序。"""
    if not rule_ids:
        return list(rules.PIPELINE_ORDER)
    wanted = []
    for item in rule_ids:
        rid = str(item)
        if rid not in wanted:
            wanted.append(rid)
    ordered = [rid for rid in rules.PIPELINE_ORDER if rid in wanted]
    ordered += [rid for rid in wanted if rid not in rules.PIPELINE_ORDER]
    return ordered


def _workdir(options):
    raw = (options or {}).get("workdir") or os.environ.get("DAMPACK_WORK_DIR")
    if not raw:
        raise RpcError(ERR_INVALID_PARAMS, "缺少 workdir（options.workdir 或 DAMPACK_WORK_DIR）")
    os.makedirs(raw, exist_ok=True)
    return raw


# --- 单规则判定（judge / repair 复判共用同一实现，保证首判与复判同语义） -------
def _judge_domain(path):
    reply = gw().vl_judge(path, rules.PROMPT_DOMAIN, max_tokens=VL_MAX_TOKENS)
    return rules.eval_answer(reply, expect_yes=True,
                             rule_id="R0_product_main", label="domain_gate")


def _judge_size(path, info, thresholds, rule_id="R1_size"):
    meta = info or gw().io_read(path)
    th = rules.thresholds_for("R1_size", thresholds)
    return rules.eval_size(meta.get("width") or 0, meta.get("height") or 0,
                           short_side_min=th.get("short_side_min", 1600), rule_id=rule_id)


def _judge_white(path, thresholds, options):
    """白底：宿主给像素统计就走 classic（确定性），否则退回 VL（非确定性）。"""
    measures = (options or {}).get("measurements") or {}
    if measures.get("white_dev") is not None:
        th = rules.thresholds_for("R2_white_bg", thresholds)
        return rules.eval_white_edge(measures["white_dev"], max_dev=th.get("max_dev", 8))
    reply = gw().vl_judge(path, rules.PROMPT_WHITE_BG, max_tokens=VL_MAX_TOKENS)
    return rules.eval_answer(reply, expect_yes=True,
                             rule_id="R2_white_bg", label="white_background")


def _measure_subject(path, thresholds, rule_id="R4_subject_ratio"):
    seg = gw().detect_u2net(path)
    ratio = seg.get("fg_ratio")
    th = rules.thresholds_for("R4_subject_ratio", thresholds)
    if ratio is None:
        from dampack import undetermined
        return undetermined(rule_id, "no_subject_measurement", label="subject_ratio",
                            evidence={"source": "u2net"})
    return rules.eval_subject_ratio(float(ratio), minimum=th.get("min", 0.55),
                                    maximum=th.get("max", 0.85), rule_id=rule_id)


def _judge_overlay(path):
    """R3：OCR 先查（确定性、便宜），有字直接 fail；无字再问 VL。"""
    ocr = gw().detect_ocr(path, lang=OCR_LANG)
    lines = ocr.get("lines") or []
    if lines:
        from dampack import failed
        return failed("R3_no_watermark", label="overlay_text",
                      evidence={"source": "ocr",
                                "lines": [ln.get("text") for ln in lines],
                                "boxes": [ln.get("box") for ln in lines]},
                      note="ocr_text_detected")
    reply = gw().vl_judge(path, rules.PROMPT_NO_OVERLAY, max_tokens=VL_MAX_TOKENS)
    return rules.eval_answer(reply, expect_yes=False,
                             rule_id="R3_no_watermark", label="overlay_text")


def _judge_attribute(path):
    """R5 打标：不是合格判定（`on_fail: pass`），只填词表。"""
    reply = gw().vl_judge(path, rules.PROMPT_ATTRIBUTE, max_tokens=VL_MAX_TOKENS)
    if (reply or {}).get("undetermined"):
        return rules.skipped("R5_attribute", "attribute_undetermined", label="attribute"), {}
    tags = _parse_json_block((reply or {}).get("raw") or "")
    return rules.skipped("R5_attribute", "outputs_only:attribute_tagging",
                         label="attribute"), tags


def _parse_json_block(text):
    start = text.find("{")
    end = text.rfind("}")
    if start < 0 or end <= start:
        return {}
    try:
        payload = json.loads(text[start:end + 1])
        return payload if isinstance(payload, dict) else {}
    except ValueError:
        return {}


def measure(job_id, path, rule_ids, options=None, info=None, respect_deps=True):
    """按 pipeline 声明序判一组规则，返回 {rule_id: verdict}（含 gate 短路）。

    - `judge` 用 `respect_deps=True`：R0 与 R1 的短路语义照 pipeline 声明（省 VL/算力）；
    - `repair` 用 `respect_deps=False`：每轮全量复判每一规则，否则「R1 修好后 R4 才被判」
      会让 R4 永远拿不到自己的首判，产物与 fixed 对不上账。
    """
    options = options or {}
    thresholds = options.get("thresholds")
    verdicts = {}
    domain_status = "pass" if "R0_product_main" not in rule_ids else None
    tags = None
    for rid in rule_ids:
        pack.check_cancel(job_id, "rule:" + rid)
        if rid == "R0_product_main":
            verdict = _judge_domain(path)
            domain_status = verdict["status"]
            verdicts[rid] = verdict
            continue
        if domain_status is not None and rules.domain_gate_blocks(domain_status):
            verdicts[rid] = rules.skipped(rid, "domain_gate_%s" % domain_status,
                                          label="short_circuit")
            continue
        if rid == "R1_size":
            verdicts[rid] = _judge_size(path, info, thresholds)
        elif rid == "R2_white_bg":
            verdicts[rid] = _judge_white(path, thresholds, options)
        elif rid == "R4_subject_ratio":
            size = verdicts.get("R1_size")
            if respect_deps and size is not None and size.get("status") != "pass":
                verdicts[rid] = rules.skipped(rid, "depends_on:R1_size", label="short_circuit")
            else:
                verdicts[rid] = _measure_subject(path, thresholds)
        elif rid == "R3_no_watermark":
            verdicts[rid] = _judge_overlay(path)
        elif rid == "R5_attribute":
            if options.get("attribute"):
                verdict, tags = _judge_attribute(path)
                verdict["evidence"] = {"tags": tags or {}}
            else:
                verdict = rules.skipped(rid, "outputs_only:attribute_tagging", label="attribute")
            verdicts[rid] = verdict
        else:
            verdicts[rid] = rules.skipped(rid, "unknown_rule", label="unknown")
    return verdicts


# --- 修复：几何闭式解 + 换底 + OCR 局部 inpaint -------------------------------
def _op(path, name, payload):
    body = {"path": path}
    body.update(payload)
    out = gw().call(name, body)
    return str(out.get("path") or ""), out


def _apply_geometry(path, plan, dims, workdir, stage):
    """执行 crop → letterbox → upscale；每步产物名带尺寸，便于复判与人工核对。"""
    cur, cur_dims = path, dims
    trail = []
    if plan.get("crop_box"):
        out = os.path.join(workdir, "%s-1crop.png" % stage)
        cur, res = _op(cur, "/op/crop", {"box": plan["crop_box"], "out": out})
        cur_dims = (res.get("width"), res.get("height"))
        trail.append({"op": "crop", "box": plan["crop_box"], "out": cur})
    width, height = cur_dims
    if geometry.needs_letterbox(plan.get("crop_box"), width or 0, height or 0):
        out = os.path.join(workdir, "%s-2letterbox.png" % stage)
        cur, res = _op(cur, "/op/letterbox",
                       {"target_ratio": plan["target_ratio"], "background": "#ffffff",
                        "out": out})
        cur_dims = (res.get("width"), res.get("height"))
        trail.append({"op": "letterbox", "target_ratio": plan["target_ratio"], "out": cur})
    scale = plan.get("scale") or 1.0
    if float(scale) != 1.0:
        out = os.path.join(workdir, "%s-3upscale.png" % stage)
        cur, res = _op(cur, "/op/upscale", {"scale": scale, "out": out})
        cur_dims = (res.get("width"), res.get("height"))
        trail.append({"op": "upscale", "scale": scale, "out": cur})
    return cur, cur_dims, trail


def _replace_bg(path, workdir, stage):
    out = os.path.join(workdir, "%s-bg.png" % stage)
    cur, res = _op(path, "/op/replace_bg",
                   {"color": "#ffffff", "background": "#ffffff", "out": out})
    return cur, (res.get("width"), res.get("height")), [{"op": "replace_bg", "out": cur}]


def _inpaint_overlay(path, dims, workdir, stage):
    """OCR 框 → 纯标准库掩码 PNG → 局部 inpaint（只改文字区，不重绘商品本体）。"""
    ocr = gw().detect_ocr(path, lang=OCR_LANG)
    lines = ocr.get("lines") or []
    if not lines:
        return path, dims, []
    width, height = int(dims[0]), int(dims[1])
    mask_path = os.path.join(workdir, "%s-mask.png" % stage)
    maskpng.write_mask_png(mask_path, width, height,
                           [ln.get("box") for ln in lines if ln.get("box")])
    out = os.path.join(workdir, "%s-inpaint.png" % stage)
    cur, res = _op(path, "/op/inpaint",
                   {"mask": mask_path, "out": out, "allow_redraw_subject": False})
    return cur, (res.get("width"), res.get("height")), [
        {"op": "inpaint", "mask": mask_path, "boxes": len(lines), "out": cur}]


def _stage_copy(path, dims, workdir, stage):
    """把当前版本钉成「名字里带尺寸」的稳定文件（复判走它，可人工点开核对）。"""
    width, height = dims
    if not width or not height:
        meta = gw().io_read(path)
        width, height = meta.get("width"), meta.get("height")
    out = os.path.join(workdir, "%s_%dx%d.png" % (stage, int(width), int(height)))
    shutil.copyfile(path, out)
    return out, (int(width), int(height))


def _artifact(path, dims, workdir, rule_id):
    width, height = int(dims[0]), int(dims[1])
    safe = "".join(ch if (ch.isalnum() or ch in "_.-") else "_" for ch in str(rule_id))[:64]
    out = os.path.join(workdir, "%s_%dx%d.png" % (safe or "asset", width, height))
    shutil.copyfile(path, out)
    return {"rule_id": rule_id, "path": out, "width": width, "height": height,
            "kind": "image/png"}


def _slow_hook(job_id, path):
    """conformance 断言 10（取消语义）需要一个「judge 仍在跑」的窗口。

    套件夹具用文件名 `slow` 约定制造该窗口；真实资产名不会命中，
    命中也只是「边等边看取消标记」，不改变任何判定结果。
    """
    if "slow" not in os.path.basename(path).lower():
        return
    deadline = time.time() + 2.0
    while time.time() < deadline:
        pack.check_cancel(job_id, "judge")
        time.sleep(0.05)


# --- 协议方法（SDK 只需注册 handler，帧/取消/账本/错误码都由 PackServer 负责） ---
@pack.method("judge")
def judge(params):
    job_id = str(params.get("job_id") or "")
    asset = params.get("asset") or {}
    options = params.get("options") or {}
    path = _require_path(asset)
    rule_ids = _normalize_rules(params.get("rules"))
    _slow_hook(job_id, path)
    info = gw().io_read(path)
    verdicts = measure(job_id, path, rule_ids, options, info=info)
    ordered = [verdicts[rid] for rid in rule_ids if rid in verdicts]
    try:
        pack.emit(job_id, {"asset_id": str(asset.get("id") or os.path.basename(path)),
                           "path": path}, ordered)
    except RpcError as exc:
        log("[ecommerce_py] report/emit 失败（不影响判定结果）：", exc.code, exc.message)
        raise
    return {"asset_id": str(asset.get("id") or os.path.basename(path)), "verdicts": ordered}


@pack.method("repair")
def repair(params):
    job_id = str(params.get("job_id") or "")
    asset = params.get("asset") or {}
    options = params.get("options") or {}
    path = _require_path(asset)
    rule_ids = _normalize_rules(params.get("rules"))
    workdir = _workdir(options)
    asset_id = str(asset.get("id") or os.path.basename(path))
    fixable = [rid for rid in rule_ids if rid in rules.FIXABLE]
    if not fixable:
        return {"asset_id": asset_id,
                "verdicts": [rules.skipped(rid, "fix_policy:%s" % rules.FIX_POLICY,
                                           label="not_fixable") for rid in rule_ids],
                "artifacts": []}

    info = gw().io_read(path)
    dims = (info.get("width") or 0, info.get("height") or 0)
    cur, dims = _stage_copy(path, dims, workdir, "r0-input")
    first = {}
    final = {}
    trail = []
    unfixable = {}
    allow_inpaint = options.get("allow_inpaint", True)

    for round_no in range(1, rules.MAX_ROUNDS + 1):
        pack.check_cancel(job_id, "round%d" % round_no)
        current = measure(job_id, cur, list(rule_ids), options, respect_deps=False)
        if round_no == 1:
            first = {rid: dict(v) for rid, v in current.items()}
        final = current
        pending = [rid for rid in fixable if current.get(rid, {}).get("status") == "fail"]
        if not pending:
            break

        stage = "r%d" % round_no
        applied = []
        geo_rules = [rid for rid in pending
                     if rid in ("R1_size", "R4_subject_ratio") and rid not in unfixable]
        if geo_rules:
            seg = gw().detect_u2net(cur)
            th = rules.thresholds_for("R4_subject_ratio", options.get("thresholds"))
            size_th = rules.thresholds_for("R1_size", options.get("thresholds"))
            plan = geometry.solve(
                dims[0], dims[1], seg.get("fg_ratio"),
                target_ratio=th.get("target", 0.75),
                ratio_min=th.get("min", 0.55),
                ratio_max=th.get("max", 0.85),
                short_side_min=size_th.get("short_side_min", 1600),
            )
            if plan.fixable:
                cur, dims, ops = _apply_geometry(cur, plan, dims, workdir, stage)
                applied.append({"rule": "geometry", "plan": dict(plan), "ops": ops})
            else:
                for rid in geo_rules:
                    unfixable[rid] = plan.get("unfixable")
                trail.append({"round": round_no, "unfixable": plan.get("unfixable"),
                              "reason": plan.get("reason")})
        if "R2_white_bg" in pending:
            cur, dims, ops = _replace_bg(cur, workdir, stage)
            applied.append({"rule": "R2_white_bg", "ops": ops})
        if "R3_no_watermark" in pending and allow_inpaint:
            cur, dims, ops = _inpaint_overlay(cur, dims, workdir, stage)
            if ops:
                applied.append({"rule": "R3_no_watermark", "ops": ops})
        if not applied:
            break                      # 这一轮什么也没做 → 不再空转
        cur, dims = _stage_copy(cur, dims, workdir, stage)
        for item in applied:
            item["round"] = round_no
            item["output"] = cur
            trail.append(item)
    else:
        pack.check_cancel(job_id, "final")
        final = measure(job_id, cur, list(rule_ids), options, respect_deps=False)

    verdicts = []
    artifacts = []
    for rid in rule_ids:
        before = first.get(rid) or {}
        after = final.get(rid) or {}
        if rid not in fixable:
            verdicts.append(after)
            continue
        if before.get("status") == "fail" and after.get("status") == "pass":
            artifacts.append(_artifact(cur, dims, workdir, rid))
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
                                       rounds=rules.MAX_ROUNDS, reason=reason)
            if after.get("status") not in ("fail", "undetermined"):
                verdict["status"] = "undetermined"
            verdicts.append(verdict)
        else:
            verdicts.append(after)
    return {"asset_id": asset_id, "verdicts": verdicts, "artifacts": artifacts, "trail": trail}


# 卡面要求的导出符号（宿主也可直接 import 这两个名字）
JUDGE = judge
REPAIR = repair


def serve():
    """launcher / `python plugin/__init__.py` 的统一入口。"""
    return pack.serve()


if __name__ == "__main__":
    raise SystemExit(serve())
