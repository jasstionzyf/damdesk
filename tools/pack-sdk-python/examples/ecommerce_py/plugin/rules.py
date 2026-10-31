"""电商样例包（SDK 版）—— 规则集与纯判定函数。

语义与官方包 `packs/plugins/ecommerce` v1.0.0 的 `plugin/ruleset.py` **逐条对齐**
（阈值/执行序/短路语义/测量口径都一样），差别只在工程形态：
    - 错误/verdict 来自 SDK（`dampack.RpcError` / `dampack.verdict`），本模块不再自己造；
    - 本模块仍是「测量值 → 结论」的纯函数，方便单测与 K00 基线逐行对账。

对账口径（`scripts/consistency_10.py`）：确定性规则 R1/R2/R4 的 **度量值** 必须与旧引擎
逐值一致（1e-6）；`score` 是派生值，按包内定义投影后比较（R1：短边 px → 短边/门槛）。

阈值来源（用户裁决，`damdesk-repo/references/fix-policy-and-a4.md`）：
主体占比下限 **0.55**、上限 0.85，短边 **1600**；`resources/rules.json` 与 `THRESHOLDS` 同一份事实源。

精度口径：`score`/`evidence` 里的度量值**一律不截断**（不 round）。旧引擎把短边记成 px 原值、
white_dev/fg_ratio 记成原始 float，截断到 3 位会破坏「逐值 1e-6 对账」；
需要展示精度的消费方自己格式化。官方包 `round(...,3)` 属展示口径差异，不影响判定。
"""
from __future__ import annotations

from dampack import verdict

# --- 规则清单（pack.info 的 rules，与 resources/rules.json 同源） --------------
RULES = [
    {"id": "R0_product_main", "name": "领域 gate（是不是商品主图/白底商品图）",
     "executor": "vl", "deterministic": False},
    {"id": "R1_size", "name": "尺寸达标（短边 ≥1600）",
     "executor": "classic", "deterministic": True},
    {"id": "R2_white_bg", "name": "白底图纯白",
     "executor": "vl", "deterministic": False},
    {"id": "R4_subject_ratio", "name": "主体占比 55%~85%",
     "executor": "local_model", "deterministic": True},
    {"id": "R3_no_watermark", "name": "无促销文字/水印/牛皮癣",
     "executor": "vl", "deterministic": False},
    {"id": "R5_attribute", "name": "属性打标（非判定，填词表）",
     "executor": "vl", "deterministic": False},
]

RULE_IDS = [rule["id"] for rule in RULES]

# 执行序 = 旧引擎 pipeline.yaml 的 steps 声明序（R0 gate 首位，R4 在 R3 前）
PIPELINE_ORDER = [
    "R0_product_main",
    "R1_size",
    "R2_white_bg",
    "R4_subject_ratio",
    "R3_no_watermark",
    "R5_attribute",
]

THRESHOLDS = {
    "R1_size": {"short_side_min": 1600},
    "R2_white_bg": {"max_dev": 8},
    "R4_subject_ratio": {"min": 0.55, "max": 0.85, "target": 0.75},
}

# 修复策略按包声明（geometric = 只做几何/换底，商品本体是证据，禁止生成式重绘本体）
FIXABLE = {
    "R1_size": "upscale",
    "R2_white_bg": "replace_bg",
    "R4_subject_ratio": "geometric",
    "R3_no_watermark": "inpaint_ocr_box",
}
MAX_ROUNDS = 3
FIX_POLICY = "geometric"

PROMPT_DOMAIN = (
    "这张图片是不是电商商品主图？只回答「是」或「否」。"
    "商品主图=以单个商品为主体、用于商品详情页首图的照片；"
    "室内实景、人像、食物、风景、截图、纯文字图都算否。"
)
PROMPT_WHITE_BG = (
    "这张电商图片的商品主体背景是不是纯白色（白底图，背景无明显灰度/阴影/杂物）？"
    "只回答「是」或「否」。"
)
PROMPT_NO_OVERLAY = (
    "这张电商主图上是否存在促销文字、水印、logo 等叠加遮挡物？"
    "只回答「是」或「否」。"
)
PROMPT_ATTRIBUTE = (
    "按 JSON 输出这张商品图的属性，不要解释："
    '{"dress_silhouette": "<A字|直筒|收腰|茧型|未知>", '
    '"scene": "<模特正面|模特背面|平铺|细节图|白底图|未知>"}'
)


def thresholds_for(rule_id, overrides=None):
    """包默认阈值 + 调用方覆盖（`options.thresholds`）。"""
    base = dict(THRESHOLDS.get(rule_id) or {})
    if overrides:
        for key, value in (overrides.get(rule_id) or {}).items():
            base[key] = value
    return base


# --- 纯判定函数：测量值 → verdict --------------------------------------------
def eval_size(width, height, *, short_side_min=1600, rule_id="R1_size"):
    """R1：短边门槛。score = 短边 / 门槛（≥1 记 1.0；**不截断**，见模块头「精度口径」）。"""
    short = min(int(width), int(height))
    ok = short >= int(short_side_min)
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=1.0 if ok else short / float(short_side_min),
        label="short_side",
        evidence={"width": int(width), "height": int(height), "short_side": short,
                  "threshold": int(short_side_min)},
        note=None if ok else "below_short_side",
    )


def eval_white_edge(dev, *, max_dev=8, rule_id="R2_white_bg"):
    """R2 classic 口径：边缘白度偏差 ≤ max_dev（宿主给得出像素统计时走这条）。"""
    value = float(dev)
    ok = value <= float(max_dev)
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=value,
        label="white_edge_dev",
        evidence={"white_dev": value, "threshold": float(max_dev),
                  "source": "classic_white_edge"},
        note=None if ok else "background_not_white",
    )


def eval_subject_ratio(ratio, *, minimum=0.55, maximum=0.85,
                       rule_id="R4_subject_ratio"):
    """R4：主体占比窗口 [min, max]。score = 实测 fg_ratio（不四舍五入，保精度）。"""
    value = float(ratio)
    too_small = value < float(minimum)
    too_large = value > float(maximum)
    status = "fail" if (too_small or too_large) else "pass"
    note = "below_min_ratio" if too_small else ("above_max_ratio" if too_large else None)
    return verdict(
        rule_id, status, score=value, label="subject_ratio",
        evidence={"fg_ratio": value, "min": float(minimum), "max": float(maximum)},
        note=note,
    )


def eval_answer(reply, *, expect_yes, rule_id, label, source="vl", evidence=None):
    """把 `vl.judge` 应答映射成 verdict（`undetermined` 一律不冒充 pass/fail）。"""
    reply = reply or {}
    if reply.get("undetermined"):
        return verdict(
            rule_id, "undetermined", score=None, label=label,
            evidence={"reason": reply.get("reason") or "vl_undetermined", "source": source},
            note="vl_undetermined",
        )
    structured = reply.get("rules") or []
    status = None
    score = None
    if structured and isinstance(structured[0], dict):
        status = structured[0].get("status")
        score = structured[0].get("score")
        if structured[0].get("label"):
            label = str(structured[0]["label"])
    raw = str(reply.get("raw") or "")
    if status not in ("pass", "fail", "undetermined"):
        status = status_from_text(raw, expect_yes=expect_yes)
    if status == "undetermined":
        return verdict(rule_id, "undetermined", score=None, label=label,
                       evidence={"raw": raw, "source": source},
                       note="vl_answer_unparsed")
    payload = {"raw": raw, "source": source}
    if evidence:
        payload.update(evidence)
    return verdict(rule_id, status, score=score, label=label, evidence=payload,
                   note=None if status != "fail" else "vl_fail")


def status_from_text(raw, *, expect_yes):
    """从自由文本里取「是/否」词表答案；两种都没出现 → undetermined。

    中文里「不是」「不符合」含「是」「符合」，「是否」是问句回声，
    所以先去掉回声词、再按否定优先匹配（否定词更长、更具体）。
    """
    text = (raw or "").strip()
    if not text:
        return "undetermined"
    clean = text[:32].lower()
    for echo in ("是否", "有没有"):
        clean = clean.replace(echo, "")
    negatives = ("不是", "不符合", "不满足", "no", "false", "否")
    positives = ("符合", "满足", "yes", "true", "是")
    positive = None
    if any(token in clean for token in negatives):
        positive = False
    elif any(token in clean for token in positives):
        positive = True
    if positive is None:
        return "undetermined"
    return "pass" if positive == bool(expect_yes) else "fail"


def domain_gate_blocks(status):
    """R0 不是 pass 时，后续规则一律 skipped（旧引擎短路语义，K00 §4 硬结论）。"""
    return status != "pass"


def skipped(rule_id, note, label="not_applicable"):
    return verdict(rule_id, "skipped", note=note, label=label)
