"""电商行业包（官方包）—— 规则集与纯判定函数。

本模块**不碰网关、不碰 IO**：所有函数都是「测量值 → 结论」的纯函数，
这样判定语义可以被单测直接钉住（也便于与 K00 旧引擎基线逐行对账）。

规则集来源：`packs/ecommerce-taobao/rules.yaml`（旧引擎声明序写死的同一套 6 条），
迁移到插件形态时只做两处**能力面**改写（协议 1.1 起已收回默认路径，见 README §能力缺口）：

- `R2_white_bg`：旧引擎走 classic `white_edge`（像素统计）；协议 1.1 的 `/detect/white_stats`
  直接给同一份统计（`engine-classic` 同实现）→ 默认就走 classic 口径（确定性、可复现）；
  只有旧宿主（无该端点/能力）才退回 `vl.judge`，且在 evidence 里标明降级来源。
- `R5_attribute`：打标不是合格判定（`rules.yaml: on_fail=pass`），插件里默认 `skipped`，
  只有 `options.attribute=true` 才真的调 VL 打标。

阈值口径（用户裁决，`damdesk-repo/references/fix-policy-and-a4.md`）：
主体占比下限 **0.55**（0.85 是平台文案目标，不可落地）、上限 0.85；
短边 **1600**（Amazon/淘宝主图档）。旧引擎 main 上仍是 0.65，
插件包按裁决取 0.55，并允许调用方用 `options.thresholds` 临时改口径做对账。
"""
from __future__ import annotations

import hashlib
import json
import os

# --- 规则清单（PackInfo.rules 与 resources/rules.json 的同一份事实源） --------
# executor 只用于人读与 conformance 套件选规则，协议不强校验。
RULES = [
    {
        "id": "R0_product_main",
        "name": "领域 gate（画面里有没有商品主体；不判白底/文字/场景）",
        "executor": "vl",
        "deterministic": False,
    },
    {
        "id": "R1_size",
        "name": "尺寸达标（短边 ≥1600；长边门槛由规格给，默认不约束）",
        "executor": "classic",
        "deterministic": True,
    },
    {
        "id": "R2_white_bg",
        "name": "白底图纯白",
        "executor": "vl",
        "deterministic": False,
    },
    {
        "id": "R4_subject_ratio",
        "name": "主体占比 55%~85%",
        "executor": "local_model",
        "deterministic": True,
    },
    {
        "id": "R3_no_watermark",
        "name": "无促销文字/水印/牛皮癣",
        "executor": "vl",
        "deterministic": False,
    },
    {
        "id": "R5_attribute",
        "name": "属性打标（非判定，填词表）",
        "executor": "vl",
        "deterministic": False,
    },
]

RULE_IDS = [r["id"] for r in RULES]

# 执行序 = 旧引擎 pipeline.yaml 的 steps 声明序（R0 gate 首位，R4 在 R3 前）
PIPELINE_ORDER = [
    "R0_product_main",
    "R1_size",
    "R2_white_bg",
    "R4_subject_ratio",
    "R3_no_watermark",
    "R5_attribute",
]

# --- pack schema：阈值与提示词以 resources/rules.json 为权威（可版本化字段） ------
# 卡 task（K07-FIX-I）要求「判据参数与 prompt 文本进 pack schema，不写死代码」：
# 运行时**优先读**包内 `resources/rules.json` 的 `rules[].params` 与 `prompts{}`，
# 下面的 DEFAULT_* 只负责两件事：① 资源缺失时兜底；② 与资源逐键比对的漂移基线
# （`test_resources_in_sync` 钉住「资源 == 默认」）。改判据 = 改资源 + 同步默认值。
SCHEMA_PATH = os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "resources", "rules.json"))


def load_schema(path=None):
    """读包内 schema；缺失/坏 JSON 一律返回空 dict（插件不因资源缺失而拒绝启动）。"""
    try:
        with open(path or SCHEMA_PATH, encoding="utf-8") as fh:
            data = json.load(fh)
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


SCHEMA = load_schema()


def schema_params(rule_id):
    """schema 里某条规则的参数（`rules[].params`）；没有就返回空 dict。"""
    for rule in SCHEMA.get("rules") or ():
        if isinstance(rule, dict) and rule.get("id") == rule_id:
            params = rule.get("params")
            return dict(params) if isinstance(params, dict) else {}
    return {}


# --- 阈值（单一定义点；resources/rules.json 与之一致，有单测钉住） -------------
DEFAULT_THRESHOLDS = {
    # R0 领域 gate 的确定性前置（K07-FIX-H）：面积 ≥15% 直接认「有主体」，
    # ≤2% 直接认「没有可判主体」，2%~15% 的边界区间才交 VL 确认。
    # K07-FIX-I 三项硬否决（用户裁决 2026-10-22，f001 假阳性）：
    #   · `foreign_object_ratio_max`：非主体显著物体面积 / 主体面积 的上限（≥ 即 fail）；
    #   · `edge_margin_ratio`：主体 bbox 任一边距画面边界 < 该比例（短边口径）→ 主体被裁切；
    #   · `scene_prop_max`：允许的生活场景陪衬物个数上限（0 = 出现即 fail）。
    "R0_product_main": {"subject_area_min": 0.15, "subject_area_absent": 0.02,
                        "solidity_min": 0.25, "full_frame_ratio": 0.98,
                        "foreign_object_ratio_max": 0.30, "edge_margin_ratio": 0.02,
                        "scene_prop_max": 0},
    # `long_side_min`：长边门槛（N9 规格化的 A2）。**默认 0 = 不约束** ——
    # 淘宝档（700 短边）历史口径没有长边要求，默认给 1600 会改变既有判定；
    # 亚马逊规格在 spec 里显式给 1600（平台差异下沉到规格，不在插件默认值里悄悄收紧）。
    "R1_size": {"short_side_min": 1600, "long_side_min": 0},
    "R2_white_bg": {"max_dev": 8},
    # 修复目标取窗口中点 0.75（留两侧余量，修完一次就落进窗口）
    "R4_subject_ratio": {"min": 0.55, "max": 0.85, "target": 0.75},
    # R3 文字判据（K07-FIX-E / 用户裁决 D1=A）：OCR 只做候选定位，判据是
    # 「文字区性质 + 面积占比」——主体外的促销条按面积阈值，主体内的吊牌/型号码排除
    "R3_no_watermark": {"text_area_ratio_max": 0.02, "subject_cover_ratio_max": 0.08,
                        "min_text_chars": 2},
}

# 运行时生效值：默认参数 ← schema `rules[].params` 覆盖（只覆盖已声明的键）
THRESHOLDS = {}
for _rule_id, _defaults in DEFAULT_THRESHOLDS.items():
    _effective = dict(_defaults)
    _effective.update({k: v for k, v in schema_params(_rule_id).items() if k in _defaults})
    THRESHOLDS[_rule_id] = _effective

# --- 修复策略：按行业包声明，不写死在宿主 -------------------------------------
# geometric = 只做几何/换底（商品本体是证据，禁止生成式重绘本体）
FIXABLE = {
    "R1_size": "upscale",
    "R2_white_bg": "replace_bg",
    "R4_subject_ratio": "geometric",
    "R3_no_watermark": "inpaint_ocr_box",
}
MAX_ROUNDS = 3          # 修复→复判 轮次上限（硬上限，禁止无限循环）
FIX_POLICY = "geometric"

# --- 增量复判：每种修复算子会「波及」哪些规则 ---------------------------------
# K07-FIX-D：每轮把全部 6 条规则复判一遍是性能主因（K07 实测 plugin 25.80 s/张）。
# 复判只需覆盖「被改动影响」的规则集合；没被动到的规则沿用上一轮实测结论。
#
# **口径是推导出来的，不许手写清单**（用户裁决 2026-10-21「修复不得破坏已过条件」）：
# 每个网关算子声明它会改变哪些**测量量**（`OP_WRITES`），每条规则声明它**读**哪些测量量
# （`RULE_READS`），波及集合 = 所有 `reads ∩ writes(已执行 ops) ≠ ∅` 的规则。
# 手写「op → 规则」列表必漏项（旧表就漏了 crop 会改动文字区/占比的复判口径）。
MEASUREMENTS = ("size", "aspect", "fg_ratio", "white_edge", "text", "pixel_hash", "domain")

OP_WRITES = {
    "crop": ("size", "aspect", "fg_ratio", "white_edge", "text"),
    "letterbox": ("size", "aspect", "white_edge", "fg_ratio"),
    "upscale": ("size", "white_edge"),          # 插值不改白度判据，但画布变了要重测
    "replace_bg": ("white_edge", "fg_ratio", "text", "pixel_hash"),
    "inpaint": ("white_edge", "fg_ratio", "text", "pixel_hash"),
}

# `domain` 不挂在任何算子上：R0 判的是「画面里有没有商品」，剪裁/换底/补字都不改这个语义，
# 挂上就等于每轮都白花一次 VL（≈8 s）。pixel_hash 同理不参与规则 reads。
RULE_READS = {
    "R0_product_main": ("domain",),
    "R1_size": ("size",),
    "R2_white_bg": ("white_edge",),
    "R4_subject_ratio": ("fg_ratio",),
    "R3_no_watermark": ("text",),
    "R5_attribute": ("domain",),
}


def affected_rules(ops):
    """本轮执行过的算子 → 必须复判的规则集合（按 `PIPELINE_ORDER` 排序，便于对账）。"""
    writes = set()
    for op in ops or ():
        writes.update(OP_WRITES.get(op, ()))
    if not writes:
        return ()
    return tuple(rid for rid in PIPELINE_ORDER
                 if set(RULE_READS.get(rid, ())) & writes)


# 兼容旧调用点/旧测试：`AFFECTED_RULES[op]` == 单算子推导结果（单一来源，别再手写）
AFFECTED_RULES = {op: affected_rules([op]) for op in OP_WRITES}

# --- 提示词（模板在 resources/rules.json 的 `prompts{}`；此处是兜底默认值） --------
# R0 是**领域 gate**：画面里有没有「可售卖、完整、单一」的商品主体（K07-FIX-I 收紧）。
# 话术边界（用户裁决 2026-10-22，f001 假阳性）：
#   · 白底/促销文字/水印/logo/吊牌 → **不是** R0 的否决理由（由 R2/R3 判）；
#   · 多主体（另一件显著物体 ≥ 主体 30%）、主体被裁切、生活场景陪衬 → 是 R0 的硬否决；
#   · 模特穿着/使用该商品 → 不算陪衬（主体就是那件商品）。
#
# 为什么从「是/否」词表答案改成结构化 JSON：否决要可量化、可复现（面积比、裁切、陪衬计数），
# 单元词表只能给一个位，事后无法区分「VL 看见了陪衬物」还是「模型在猜」。
PROMPT_DOMAIN = (
    "你是电商主图合规审核员，判定这张图算不算合格的电商商品主图。"
    "只输出一个 JSON 对象，不要解释、不要代码块。\n"
    "看三件事：画面里有没有商品主体、主体是否完整、除主体外有没有显著陪衬物。\n"
    "JSON 结构（面积均为占整幅画面的比例 0~1，估不准也要给）：\n"
    '{"subject": {"present": true/false, "name": "主体商品名", "area_ratio": 0.0, '
    '"cut": true/false},\n'
    ' "others": [{"name": "物体名", "kind": "product|life_scene|person", '
    '"area_ratio": 0.0}],\n'
    ' "verdict": "pass|fail", "reason": "一句话"}\n'
    "verdict=fail 的硬条件（满足任意一条）：\n"
    "1) 没有可售卖的商品作为主体（人像写真、风景、截图、纯文字、没有商品的实景）；\n"
    "2) 除主体外还有显著物体，且其面积达到主体面积的 {foreign_object_ratio_pct} 以上"
    "（多主体 / 强陪衬）；\n"
    "3) 主体被画面边缘裁切（商品任一侧出画或贴边，距边不足短边的 {edge_margin_pct}），"
    "或 subject.cut=true；\n"
    "4) 画面里有生活场景陪衬物（桌面、餐具饮具、人手、家具、植物、包装箱、装饰摆件等"
    "与主体商品非同类的显著物件），允许个数上限 {scene_prop_max}。\n"
    "不构成否决：白底、促销文字/水印/logo、商品自带吊牌、模特穿着或使用该商品、纯色背景。\n"
    "others 只列显著物体（面积 ≥ 整幅 1% 或主体 10%），不列背景纹理；"
    "与主体同类的第二件商品 kind 记 product；与商品无关的人记 person。"
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

# --- 提示词版本化（K07-FIX-H）：prompt 必须可寻址，否则「同一张图两次结论相反」查不出原因 ---
# 每条 VL 规则一个稳定 id + 语义版本；evidence 里落 `prompt_id` + `prompt_sha256`，
# 于是「结论变了」能区分为「prompt 改了」还是「模型抖了」。
# 版本号规约：**改措辞就 +1**（哪怕只改标点），改回旧措辞也要新版本号（历史行必须可追溯）。
# v3（K07-FIX-I）：R0 从「是/否」词表答案改为结构化 JSON + 三项硬否决。
DEFAULT_PROMPT_SPECS = {
    "R0_product_main": {"id": "ecom.r0_domain", "version": 3},
    "R2_white_bg": {"id": "ecom.r2_white_bg", "version": 1},
    "R3_no_watermark": {"id": "ecom.r3_no_overlay", "version": 1},
    "R5_attribute": {"id": "ecom.r5_attribute", "version": 1},
}
DEFAULT_PROMPT_TEXTS = {
    "R0_product_main": PROMPT_DOMAIN,
    "R2_white_bg": PROMPT_WHITE_BG,
    "R3_no_watermark": PROMPT_NO_OVERLAY,
    "R5_attribute": PROMPT_ATTRIBUTE,
}

# schema `prompts{rule}` → 生效的 prompt 规格（id/版本/正文/解码参数）。
# 解码参数固定化：`temperature=0`、`top_p=1`（宿主 `compliance_vl::chat_inner` 写死），
# `max_tokens` 调用方显式给，`samples` 是多数票的样本数；四者一起落 evidence。
_SCHEMA_PROMPTS = SCHEMA.get("prompts") if isinstance(SCHEMA.get("prompts"), dict) else {}
PROMPT_SPECS = {}
PROMPT_TEXTS = {}
VL_TEMPERATURE = 0
VL_TOP_P = 1.0
VL_MAX_TOKENS = 4096
VL_SAMPLES = 3
for _rule_id, _default_spec in DEFAULT_PROMPT_SPECS.items():
    _declared = dict(_SCHEMA_PROMPTS.get(_rule_id) or {})
    PROMPT_SPECS[_rule_id] = {"id": str(_declared.get("id") or _default_spec["id"]),
                              "version": int(_declared.get("version")
                                             or _default_spec["version"])}
    PROMPT_TEXTS[_rule_id] = str(_declared.get("text") or DEFAULT_PROMPT_TEXTS[_rule_id])
    if _rule_id == "R0_product_main":
        VL_TEMPERATURE = float(_declared.get("temperature", VL_TEMPERATURE))
        VL_TOP_P = float(_declared.get("top_p", VL_TOP_P))
        VL_MAX_TOKENS = int(_declared.get("max_tokens") or VL_MAX_TOKENS)
        VL_SAMPLES = int(_declared.get("samples") or VL_SAMPLES)


def domain_prompt(thresholds=None):
    """R0 提示词：模板里的 `{foreign_object_ratio_pct}` 等占位符按**生效参数**渲染。

    阈值只写在 schema 一处（`rules[].params`），prompt 里出现的是同一组数字 ——
    否则「改阈值忘了改 prompt」会让模型按旧口径说话，而代码按新阈值判。
    不用 `str.format`：正文里有 JSON 花括号，只能按 `{key}` 精确替换。
    """
    th = dict(DEFAULT_THRESHOLDS["R0_product_main"])
    th.update(thresholds or {})
    values = {
        "foreign_object_ratio_pct": "%g%%" % (float(th["foreign_object_ratio_max"]) * 100),
        "edge_margin_pct": "%g%%" % (float(th["edge_margin_ratio"]) * 100),
        "scene_prop_max": "%d" % int(th["scene_prop_max"]),
    }
    text = PROMPT_TEXTS["R0_product_main"]
    for key, value in values.items():
        text = text.replace("{%s}" % key, value)
    return text


def prompt_spec(rule_id, text=None):
    """某条规则的 prompt 规格：稳定 id（带语义版本）+ **实际送入模型**的正文 sha256。"""
    spec = PROMPT_SPECS[rule_id]
    body = PROMPT_TEXTS[rule_id] if text is None else text
    return {
        "id": "%s.v%d" % (spec["id"], spec["version"]),
        "version": spec["version"],
        "sha256": hashlib.sha256(body.encode("utf-8")).hexdigest(),
        "temperature": VL_TEMPERATURE,
        "top_p": VL_TOP_P,
        "max_tokens": VL_MAX_TOKENS,
    }


def prompt_evidence(rule_id, text=None, **extra):
    """把 prompt 身份并进 evidence（id/sha256/temperature/top_p/max_tokens/samples）。"""
    spec = prompt_spec(rule_id, text)
    payload = {
        "prompt_id": spec["id"],
        "prompt_sha256": spec["sha256"],
        "temperature": spec["temperature"],
        "top_p": spec["top_p"],
        "max_tokens": spec["max_tokens"],
        "samples_requested": VL_SAMPLES,
    }
    payload.update(extra)
    return payload


class RpcError(Exception):
    """插件侧错误（错误码闭集见协议 §5）。"""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


def verdict(rule_id, status, score=None, label="", evidence=None,
            fixed=False, note=None):
    """构造一帧 Verdict（七字段齐全，协议 §4.2）。"""
    return {
        "ruleId": rule_id,
        "status": status,
        "score": score,
        "label": label,
        "evidence": evidence if evidence is not None else {},
        "fixed": bool(fixed),
        "note": note,
    }


def thresholds_for(rule_id, overrides=None):
    """包默认阈值 + 调用方覆盖（`options.thresholds`）。"""
    base = dict(THRESHOLDS.get(rule_id) or {})
    if overrides:
        for key, value in (overrides.get(rule_id) or {}).items():
            base[key] = value
    return base


# --- 纯判定函数：测量值 → verdict --------------------------------------------
def eval_size(width, height, *, short_side_min=1600, long_side_min=0,
              rule_id="R1_size"):
    """R1：尺寸门槛（A1 短边 / A2 长边）。score = 短边 / 短边门槛（≥1 记 1.0，口径同旧引擎）。

    `long_side_min=0` = 不约束长边（插件默认；淘宝档历史口径）。长边门槛是**规格**层的
    平台差异（亚马逊档 1600），因此这里的默认值刻意留成「关闭」，不在插件默认值里收紧。
    两条都判：任一不达标即 fail，note 指出是哪一条（`below_short_side` / `below_long_side`）。

    **量不出尺寸（0/缺失）→ undetermined（`no_image_size`）**，不是 fail：尺寸门槛的前提是
    读到了像素尺寸，前提不成立时「不达标」无从谈起（同 R3 的 `no_image_size` 口径，
    卡 task 的口径扫描项）。
    """
    try:
        width, height = int(width), int(height)
    except (TypeError, ValueError):
        width, height = 0, 0
    if width <= 0 or height <= 0:
        return verdict(rule_id, "undetermined", score=None, label="short_side",
                       evidence={"width": width, "height": height,
                                 "source": "io/read"},
                       note="no_image_size")
    short = min(width, height)
    long_ = max(width, height)
    long_min = int(long_side_min or 0)
    ok_short = short >= int(short_side_min)
    ok_long = long_min <= 0 or long_ >= long_min
    ok = ok_short and ok_long
    note = None
    if not ok:
        note = "below_short_side" if not ok_short else "below_long_side"
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=1.0 if ok_short else round(short / float(short_side_min), 3),
        label="short_side",
        evidence={"width": int(width), "height": int(height), "short_side": short,
                  "long_side": long_, "threshold": int(short_side_min),
                  "long_side_threshold": long_min},
        note=note,
    )


def eval_white_edge(dev, *, max_dev=8, band=None, worst_dev=None,
                    rule_id="R2_white_bg"):
    """R2 classic 口径：边缘白度偏差 ≤ max_dev（仅当宿主给得出像素统计时可用）。

    `band` / `worst_dev` 是宿主测量口径的一部分（协议 1.1 `/detect/white_stats` 与
    `/io/read` 的 `measure:["white"]` 都回）：band 决定「外圈几像素」参与均值，
    worst_dev 是最坏单像素偏差 —— 不落 evidence 就无法从结论复现这次测量（K07-FIX-G）。
    """
    value = float(dev)
    ok = value <= float(max_dev)
    evidence = {"white_dev": value, "threshold": float(max_dev),
                "source": "classic_white_edge"}
    if band is not None:
        evidence["band"] = int(band)
    if worst_dev is not None:
        evidence["white_worst_dev"] = float(worst_dev)
    return verdict(
        rule_id, "pass" if ok else "fail",
        # 不做四舍五入：D6-A 要求确定性规则 score 逐值一致（1e-6），
        # 旧引擎 classic 侧是 f64 原值，这里 round(3) 会让 R2 假红（167.546 vs 167.54606394621638）
        score=value,
        label="white_edge_dev",
        evidence=evidence,
        note=None if ok else "background_not_white",
    )


def eval_subject_ratio(ratio, *, minimum=0.55, maximum=0.85,
                       rule_id="R4_subject_ratio"):
    """R4：主体占比窗口 [min, max]。score = 实测 fg_ratio（不做四舍五入，保精度）。"""
    value = float(ratio)
    too_small = value < float(minimum)
    too_large = value > float(maximum)
    status = "fail" if (too_small or too_large) else "pass"
    if too_small:
        note = "below_min_ratio"
    elif too_large:
        note = "above_max_ratio"
    else:
        note = None
    return verdict(
        rule_id, status, score=value, label="subject_ratio",
        evidence={"fg_ratio": value, "min": float(minimum), "max": float(maximum)},
        note=note,
    )


# --- R0 领域 gate：确定性前置 + 边界区间 VL 确认（K07-FIX-H） ------------------
# 卡面事实（K07 二次重跑 f001）：同一张图、同一 prompt、同一模型，两次 VL 结论相反
# （legacy 第 1 跑 pass、第 2 跑 fail，见 K00 §4）。根因是「R0 完全由一次 VL 单发决定」。
# 收敛办法不是放宽判据，而是把判定拆成两段：
#   ① 确定性段：u2net 主体测量（面积 fg_ratio + bbox 填充度）先给结论；
#   ② VL 段：只有边界区间（2%~15%）与「掩码不像单个主体」的图才问 VL，
#      且必须 N 次采样一致（不一致 → undetermined），不允许在 pass/fail 间来回翻。
def classify_domain(fg_ratio, *, bbox_area_ratio=None, area_min=None,
                    area_absent=None, solidity_min=None,
                    full_frame_ratio=None):
    """R0 三态：`action=pass|fail|skip|vl` + 依据 `note` + 测量口径 `metrics`。

    - 测量缺失（宿主没装 u2net / 没回 fg_ratio）→ `vl`：不伪造确定性结论；
    - 面积 ≤ `subject_area_absent`(2%) → **skip**（画面里没有可判主体 ⇒ 本包规则对这张图
      **不适用**：判 `skipped` 而不是 `fail`，不把「不适用」写成「不合格」，也不给它
      伪造的 0.00 分/把握度。见 DOMAIN-GATE 口径修正（卡 task））；
    - 面积 < `subject_area_min`(15%) → `vl`（边界区间：小主体与背景纹理靠面积分不开）；
    - 面积达标但**掩码填充度** < `solidity_min`(0.25) → `vl`（散碎掩码≠单个商品）；
    - 面积达标但 bbox 铺满整幅 ≥ `full_frame_ratio`(0.98) → `vl`（场景/纹理整幅显著）；
    - 其余 → **pass**（有商品主体），一次 VL 都不花。
    """
    th = THRESHOLDS["R0_product_main"]
    area_min = float(th["subject_area_min"] if area_min is None else area_min)
    area_absent = float(th["subject_area_absent"] if area_absent is None else area_absent)
    solidity_min = float(th["solidity_min"] if solidity_min is None else solidity_min)
    full_frame = float(th["full_frame_ratio"] if full_frame_ratio is None
                       else full_frame_ratio)
    metrics = {"fg_ratio": None if fg_ratio is None else round(float(fg_ratio), 6),
               "bbox_area_ratio": (None if bbox_area_ratio is None
                                   else round(float(bbox_area_ratio), 6)),
               "solidity": None,
               "thresholds": {"subject_area_min": area_min,
                              "subject_area_absent": area_absent,
                              "solidity_min": solidity_min,
                              "full_frame_ratio": full_frame}}
    if fg_ratio is None:
        return {"action": "vl", "note": "no_subject_measurement", "metrics": metrics}
    ratio = float(fg_ratio)
    if ratio <= area_absent:
        return {"action": "skip", "note": "no_subject_area", "metrics": metrics}
    if ratio < area_min:
        return {"action": "vl", "note": "boundary_subject_area", "metrics": metrics}
    if bbox_area_ratio is not None and float(bbox_area_ratio) > 0:
        box_ratio = float(bbox_area_ratio)
        solidity = ratio / box_ratio
        metrics["solidity"] = round(solidity, 6)
        if solidity < solidity_min:
            return {"action": "vl", "note": "diffuse_mask", "metrics": metrics}
        if box_ratio >= full_frame:
            return {"action": "vl", "note": "full_frame_mask", "metrics": metrics}
    return {"action": "pass", "note": "subject_area", "metrics": metrics}


def domain_gate_verdict(decision, *, rule_id="R0_product_main"):
    """确定性档（pass/fail/skip）→ verdict；`vl` 档由调用方采样 VL 后走 `combine_samples`。

    `skip` 档 = **本包对该图不适用**（画面里没有可判主体）：输出 `skipped`，
    **score / 把握度一律 None** —— 不适用不是不合格，也不该带一个「0.000 分」的假值
    （宿主对非确定性包的 fail 行会把缺省 score 显示成 0.00，看起来像「零把握」）。
    """
    metrics = decision["metrics"]
    action = decision["action"]
    if action == "skip":
        return verdict(
            rule_id, "skipped", score=None, label="domain_gate",
            evidence={"source": "u2net_area", "decided_by": "subject_absent",
                      "applicable": False, "note": decision["note"], "vl_calls": 0,
                      **metrics},
            note=decision["note"],
        )
    status = "pass" if action == "pass" else "fail"
    return verdict(
        rule_id, status,
        score=metrics.get("fg_ratio"),
        label="domain_gate",
        evidence={"source": "u2net_area", "decided_by": "subject_area",
                  "applicable": True,
                  "note": decision["note"], "vl_calls": 0, **metrics},
        note=None if status == "pass" else decision["note"],
    )


# --- R0 三项硬否决（K07-FIX-I）：判据可量化，参数只在 schema 里 --------------
# 卡面裁决（用户 2026-10-22）：f001「笔记本 + 咖啡杯」生活场景图**不是**商品主图，
# legacy 判 fail 正确，插件判 pass 是假阳性。根因不是「不确定」而是「判据太窄」：
# 旧 gate 只问「有没有商品主体」，于是主体被裁切、旁边摆着大杯子、桌面上拍 —— 全放行。
# 三条否决都是**可量化**的，所以写进 schema 的 `rules[].params`，代码只执行不写死数字：
#   ① 主体数量：非主体显著物体面积 / 主体面积 ≥ `foreign_object_ratio_max`(0.30) → fail；
#   ② 主体完整性：主体 bbox 任一边距画面边界 < `edge_margin_ratio`(0.02) 短边 → fail；
#   ③ 场景度：生活场景陪衬物个数 > `scene_prop_max`(0) → fail。
DOMAIN_REASONS = {
    "subject_missing": "no_product_subject",
    "subject_cut": "subject_cut_off",
    "foreign_object": "foreign_object_too_large",
    "scene_prop": "scene_prop_present",
    "vl_fail": "vl_verdict_fail",
}


def _num(value):
    """宽容取数：字符串/None/NaN/inf 一律 None（VL 给的 JSON 不能全信）。"""
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    if number != number or number in (float("inf"), float("-inf")):
        return None
    return number


def bbox_edge_margins(box, width, height):
    """主体框四边到画布边界的距离（按**短边**归一，便于跨尺寸比较）。"""
    try:
        x1, y1, x2, y2 = [float(v) for v in (box or [])[:4]]
        width, height = float(width), float(height)
    except (TypeError, ValueError):
        return None
    if width <= 0 or height <= 0:
        return None
    short = min(width, height)
    left, top = max(0.0, x1), max(0.0, y1)
    right, bottom = max(0.0, width - x2), max(0.0, height - y2)
    return {"left": round(left / short, 6), "top": round(top / short, 6),
            "right": round(right / short, 6), "bottom": round(bottom / short, 6),
            "min": round(min(left, top, right, bottom) / short, 6)}


def subject_cut(subject, *, margin_ratio=None):
    """主体是否被画面裁切（确定性档：`/detect/bbox` 的主体框贴边）。

    `subject` 是 `/detect/bbox` 应答（`bbox` + `width`/`height`）。两种「不算裁切」：
    · 拿不到框/尺寸 → 交 VL（不伪造结论）；
    · 框铺满整幅（≥ `full_frame_ratio`）→ 掩码本身不可信（细节特写/整幅纹理），交 VL。
    """
    th = DEFAULT_THRESHOLDS["R0_product_main"]
    ratio = float(th["edge_margin_ratio"] if margin_ratio is None else margin_ratio)
    if not isinstance(subject, dict):
        return {"cut": False, "known": False, "margins": None, "threshold": ratio,
                "note": "no_subject_measurement"}
    box, width, height = subject.get("bbox"), subject.get("width"), subject.get("height")
    margins = bbox_edge_margins(box, width, height)
    if margins is None:
        return {"cut": False, "known": False, "margins": None, "threshold": ratio,
                "note": "no_subject_measurement"}
    try:
        area = (max(0.0, float(box[2]) - float(box[0]))
                * max(0.0, float(box[3]) - float(box[1])))
        box_ratio = area / (float(width) * float(height))
    except (TypeError, ValueError, IndexError):
        box_ratio = None
    full_frame = float(th["full_frame_ratio"])
    if box_ratio is not None and box_ratio >= full_frame:
        return {"cut": False, "known": False, "margins": margins, "threshold": ratio,
                "bbox_area_ratio": round(box_ratio, 6), "note": "full_frame_mask"}
    return {"cut": margins["min"] < ratio, "known": True, "margins": margins,
            "threshold": ratio,
            "bbox_area_ratio": None if box_ratio is None else round(box_ratio, 6),
            "note": "subject_edge" if margins["min"] < ratio else "subject_inside"}


def parse_domain_answer(raw):
    """把 VL 正文解析成结构化应答；不是 JSON（旧宿主/旧 prompt）→ None，走词表降级。"""
    text = str(raw or "")
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end <= start:
        return None
    try:
        data = json.loads(text[start:end + 1])
    except ValueError:
        return None
    return data if isinstance(data, dict) else None


def _domain_others(answer):
    """`others` 规范化：只留 `name/kind/area_ratio`，kind 白名单外的记 unknown。"""
    out = []
    for item in (answer.get("others") or ()):
        if not isinstance(item, dict):
            continue
        kind = str(item.get("kind") or "unknown").strip().lower()
        if kind not in ("product", "life_scene", "person"):
            kind = "unknown"
        out.append({"name": str(item.get("name") or "")[:40], "kind": kind,
                    "area_ratio": _num(item.get("area_ratio"))})
    return out


def eval_domain_answer(answer, *, subject=None, thresholds=None,
                       rule_id="R0_product_main"):
    """VL 结构化应答 + 三项硬否决 → `{status, reasons, metrics}`（纯函数，可单测）。

    - `answer` 不是 dict → `undetermined`（不给确定性结论，也不冒充 pass）；
    - 主体框贴边（确定性测量）或模型说 `subject.cut=true` → `subject_cut_off`；
    - 任一非主体显著物体（product/person/life_scene）面积 ≥ 主体 × 0.30 → `foreign_object_too_large`；
    - 生活场景陪衬物个数 > `scene_prop_max` → `scene_prop_present`；
    - 没有否决且 `verdict != fail`（且主体在场）→ `pass`；模型说 fail 但没给出可归因
      否决 → `vl_verdict_fail`（不把模型的自由裁量当成通过）。
    """
    th = dict(DEFAULT_THRESHOLDS["R0_product_main"])
    th.update(thresholds or {})
    foreign_max = float(th["foreign_object_ratio_max"])
    scene_max = int(th["scene_prop_max"])
    cut = subject_cut(subject, margin_ratio=th["edge_margin_ratio"])
    metrics = {"subject": None, "others": [], "cut": cut,
               "thresholds": {"foreign_object_ratio_max": foreign_max,
                              "edge_margin_ratio": float(th["edge_margin_ratio"]),
                              "scene_prop_max": scene_max}}
    if not isinstance(answer, dict):
        return {"status": "undetermined", "reasons": ["unparsed_answer"],
                "metrics": metrics}
    subj = answer.get("subject") if isinstance(answer.get("subject"), dict) else {}
    subj_area = _num(subj.get("area_ratio"))
    present = subj.get("present")
    present = bool(present) if present is not None else None
    others = _domain_others(answer)
    verdict_text = str(answer.get("verdict") or "").strip().lower()
    metrics.update({"subject": {"present": present,
                                "name": str(subj.get("name") or "")[:40],
                                "area_ratio": subj_area,
                                "cut": bool(subj.get("cut"))},
                    "others": others,
                    "verdict": verdict_text or None})
    reasons = []
    if present is False:
        # 模型明确说「画面里没有商品主体」⇒ 本包对这张图**不适用**：skip（不判），
        # 不是 fail。其余否决项都以「有主体」为前提，一并跳过（不叠加无意义理由）。
        metrics["applicable"] = False
        return {"status": "skipped", "reasons": [DOMAIN_REASONS["subject_missing"]],
                "metrics": metrics}
    if cut.get("cut") or bool(subj.get("cut")):
        reasons.append(DOMAIN_REASONS["subject_cut"])
    scene_like = 0
    for item in others:
        if item["kind"] == "life_scene":
            scene_like += 1
        area = item["area_ratio"]
        if area is None or not subj_area:
            continue
        if area / subj_area >= foreign_max:
            reasons.append("%s(%s:%.3f/%.3f)" % (DOMAIN_REASONS["foreign_object"],
                                                 item["name"] or item["kind"],
                                                 area, subj_area))
    if scene_like > scene_max:
        reasons.append("%s(%d>%d)" % (DOMAIN_REASONS["scene_prop"], scene_like, scene_max))
    metrics["scene_props"] = scene_like
    if reasons:
        return {"status": "fail", "reasons": reasons, "metrics": metrics}
    if verdict_text == "pass" or (not verdict_text and present):
        return {"status": "pass", "reasons": [], "metrics": metrics}
    if verdict_text == "fail":
        return {"status": "fail", "reasons": [DOMAIN_REASONS["vl_fail"]], "metrics": metrics}
    return {"status": "undetermined", "reasons": ["answer_without_verdict"],
            "metrics": metrics}


def domain_veto_verdict(result, *, rule_id="R0_product_main", evidence=None):
    """VL/否决档 → verdict（evidence 里带理由清单与测量口径，事后可解释）。"""
    status = result["status"]
    metrics = result.get("metrics") or {}
    payload = {"source": "vl_domain_veto", "decided_by": "vl_veto",
               "reasons": list(result.get("reasons") or []),
               "thresholds": metrics.get("thresholds"),
               "cut": metrics.get("cut"),
               "others": metrics.get("others"),
               "vl_subject": metrics.get("subject"),
               "vl_verdict": metrics.get("verdict")}
    if evidence:
        payload.update(evidence)
    note = None
    if status in ("fail", "skipped"):
        note = (result.get("reasons") or ["vl_fail"])[0]
    elif status == "undetermined":
        note = "vl_undetermined"
    # skipped = 本包对该图不适用（模型说画面里没有商品主体）：不带 score（不造假分）
    return verdict(rule_id, status, score=None, label="domain_gate",
                   evidence=payload, note=note)


def combine_samples(statuses, *, rule_id, label, evidence=None, details=None):
    """多份 VL 采样 → 一个 verdict：**过半多数票**采纳，无过半 → `undetermined`。

    卡面（K07-FIX-I 第 2 项）把 FIX-H 的「全一致才采纳」改成 3 次多数票：
    2:1 采纳多数派（`note=vl_majority_vote`，票数落 `evidence.votes`），
    1:1:1（含 undetermined 的平票）→ `undetermined(vl_no_majority)`。
    票数与逐票结论（`details`）都落 evidence，事后能看「哪一票为什么判 fail」。
    """
    samples = [s for s in (statuses or []) if s]
    votes = {}
    for status in samples:
        votes[status] = votes.get(status, 0) + 1
    payload = dict(evidence or {})
    payload.update({"samples": len(samples), "sample_statuses": samples,
                    "distinct": sorted(set(samples)), "votes": votes})
    if details:
        payload["sample_verdicts"] = list(details)
    if not samples:
        return verdict(rule_id, "undetermined", score=None, label=label,
                       evidence=payload, note="vl_no_sample")
    top = max(votes, key=lambda key: (votes[key], key != "undetermined"))
    if votes[top] * 2 <= len(samples):
        return verdict(rule_id, "undetermined", score=None, label=label,
                       evidence=payload, note="vl_no_majority")
    payload["majority"] = top
    payload["vote_margin"] = "%d/%d" % (votes[top], len(samples))
    if top == "undetermined":
        return verdict(rule_id, "undetermined", score=None, label=label,
                       evidence=payload, note="vl_undetermined")
    if len(votes) == 1:
        note = {"pass": None, "skipped": "vl_not_applicable"}.get(top, "vl_fail")
    else:
        note = {"pass": "vl_majority_vote",
                "skipped": "vl_majority_vote_not_applicable"}.get(
                    top, "vl_majority_vote_fail")
    # 把握度 = 多数票占比（3/3=1.0、2/3=0.667）：宿主对非确定性包正是拿 score 当 confidence，
    # 先前这里给 None → inspector 上「Fail / 置信度 0.00 / 原始分 0.000」，等于把「没给分」读成
    # 「零把握」（报障同族问题，卡 task）。skipped 仍不给分（不适用没有把握度可言）。
    margin = None if top == "skipped" else round(votes[top] / float(len(samples)), 6)
    return verdict(rule_id, top, score=margin, label=label, evidence=payload, note=note)


# --- R3 文字判据：OCR 只做候选定位，判据 = 文字区性质 + 面积占比 ------------------
# 旧语义「OCR 认出任何字就 fail」把商品自带吊牌/型号码/洗水标全判成牛皮癣（K07 门禁
# a001..a005 与 f004/f005 共 7 行硬不一致）。用户裁决 D1=A：文字区先分三档，只有
# 「覆盖主体的水印」与「主体外面积达标（≥2%）的促销条」才是 fail；主体内的吊牌排除；
# 主体外面积很小的（型号码/角标/少量文字）交 VL 裁量，不靠面积一刀切。
def _box4(box):
    """规范化 OCR 框 `[x1,y1,x2,y2]`（排序、去退化）；非法 → None。"""
    try:
        x1, y1, x2, y2 = [int(round(float(v))) for v in box[:4]]
    except (TypeError, ValueError):
        return None
    x1, x2 = sorted((x1, x2))
    y1, y2 = sorted((y1, y2))
    if x2 <= x1 or y2 <= y1:
        return None
    return [x1, y1, x2, y2]


def _inter_area(a, b):
    w = min(a[2], b[2]) - max(a[0], b[0])
    h = min(a[3], b[3]) - max(a[1], b[1])
    return w * h if w > 0 and h > 0 else 0


def overlay_regions(boxes, *, width, height, subject_bbox=None, subject_boxes=None,
                    subject_cover_ratio_max=0.08, min_text_chars=2):
    """把 OCR 行分档：`watermark`（覆盖主体）/ `outside`（主体外）/ `labels`（主体内吊牌）/ noise。

    - `boxes`：`/detect/ocr` 的 `lines`（`{"text","box","score"}`），也接受裸框
      `[[x1,y1,x2,y2], ...]`（裸框表示「文本未知」→ 不做噪声过滤，保持旧调用方语义）；
    - `subject_bbox`：`/detect/bbox` 的 `dominant_bbox`；缺省时所有文字按「主体外」处理
      （旧宿主没有主体框时退回面积口径，不假装能排除吊牌）；
    - `subject_boxes`：`/detect/bbox` 的 `instances`（**全部商品实例**的 bbox）。多商品图
      （一串胶囊 / 一排在架商品）里第二件商品是独立连通域，只认主主体会把它的自带文字
      判成「主体外文字」（K07 f002：3 行 OCR 合计 3.4% → 误判促销条）。给了就按实例集合
      判「文字落在商品上」，缺省/空 → 退回单主体口径（老宿主行为不变）；
    - 归属看**框中心**：中心落在任一实例框内 = 商品自带文字，但该框覆盖**主主体**紧框
      面积 ≥ `subject_cover_ratio_max`（默认 8%）时，它是叠加在商品上的水印，不是标签。
      水印判据只用主主体：小实例（一颗胶囊）的紧框太小，拿它当分母会把商品自带文字
      算成 100% 覆盖；
    - **噪声过滤**（真实语料标定）：文本少于 `min_text_chars`（默认 2）个字符的行不算文字
      —— 实测 a002 的 u2net/OCR 在裙面花纹上给出「P」「0」两只大框、合计占画面 42%，
      按「覆盖主体 8%」会把干净商品图判成水印（K07-FIX-E 标定实测）。
    """
    width, height = int(width), int(height)
    img_area = float(width * height) or 1.0
    subject = _box4(subject_bbox) if subject_bbox else None
    subject_area = (float((subject[2] - subject[0]) * (subject[3] - subject[1]))
                    if subject else 0.0)
    instances = [b for b in (_box4(item.get("bbox") if isinstance(item, dict) else item)
                             for item in (subject_boxes or [])) if b]
    if not instances and subject:
        instances = [subject]
    watermark, outside, labels = [], [], []
    total = 0
    noise = 0
    for raw in boxes or []:
        text = raw.get("text") if isinstance(raw, dict) else None
        box = _box4(raw.get("box") if isinstance(raw, dict) else raw)
        if not box:
            continue
        if text is not None and len(str(text).strip()) < int(min_text_chars):
            noise += 1          # 单字符/空文本 = 图案误检，既不算文字也不该被 inpaint
            continue
        total += (box[2] - box[0]) * (box[3] - box[1])
        if subject and subject_area > 0:
            cover = _inter_area(box, subject) / subject_area
            cx, cy = (box[0] + box[2]) / 2.0, (box[1] + box[3]) / 2.0
            inside = any(inst[0] <= cx <= inst[2] and inst[1] <= cy <= inst[3]
                         for inst in instances)
            if cover >= float(subject_cover_ratio_max):
                watermark.append({"box": box, "cover_ratio": round(cover, 4)})
            elif inside:
                labels.append({"box": box, "cover_ratio": round(cover, 4)})
            else:
                outside.append({"box": box})
        else:
            outside.append({"box": box})
    outside_area = sum((item["box"][2] - item["box"][0]) * (item["box"][3] - item["box"][1])
                       for item in outside)
    kept = len(watermark) + len(labels) + len(outside)
    return {
        "watermark": watermark, "outside": outside, "labels": labels,
        "metrics": {
            "width": width, "height": height,
            "lines": kept,
            "ocr_lines": kept + noise,
            "noise_lines": noise,
            "text_area_ratio": round(total / img_area, 4),
            "outside_area_ratio": round(outside_area / img_area, 4),
            "subject_bbox": subject,
            # 多实例主体：既落框也落个数，结论能按「文字落在哪块商品上」逐行复现
            "subject_boxes": instances,
            "subject_count": len(instances),
            "subject_cover_ratio_max": float(subject_cover_ratio_max),
            "min_text_chars": int(min_text_chars),
        },
    }


def classify_text_overlay(boxes, *, width, height, subject_bbox=None, subject_boxes=None,
                          text_area_ratio_max=0.02, subject_cover_ratio_max=0.08,
                          min_text_chars=2):
    """R3 三档结论：`action=pass|fail|vl`（`vl` = 面积过小、需 VL 裁量文字性质）。"""
    regions = overlay_regions(boxes, width=width, height=height,
                              subject_bbox=subject_bbox, subject_boxes=subject_boxes,
                              subject_cover_ratio_max=subject_cover_ratio_max,
                              min_text_chars=min_text_chars)
    metrics = dict(regions["metrics"],
                   text_area_ratio_max=float(text_area_ratio_max))
    if not metrics["lines"]:
        return {"action": "pass", "note": "no_text", "regions": regions, "metrics": metrics}
    if regions["watermark"]:
        return {"action": "fail", "note": "text_covers_subject",
                "regions": regions, "metrics": metrics}
    if not regions["outside"]:
        # 只有主体内的吊牌/型号码/商品自带 logo → 不是牛皮癣
        return {"action": "pass", "note": "subject_label_text",
                "regions": regions, "metrics": metrics}
    if metrics["outside_area_ratio"] >= float(text_area_ratio_max):
        return {"action": "fail", "note": "promo_text_area",
                "regions": regions, "metrics": metrics}
    return {"action": "vl", "note": "small_outside_text",
            "regions": regions, "metrics": metrics}


def text_overlay_verdict(decision, *, rule_id="R3_no_watermark"):
    """确定性档（pass/fail）→ verdict；`vl` 档由调用方问 VL 后再 `eval_answer`。"""
    metrics = decision["metrics"]
    regions = decision["regions"]
    evidence = {
        "source": "ocr+subject_bbox" if metrics.get("subject_bbox") else "ocr",
        "lines": metrics["lines"],
        "ocr_lines": metrics.get("ocr_lines"),
        "noise_lines": metrics.get("noise_lines"),
        "text_area_ratio": metrics["text_area_ratio"],
        "outside_area_ratio": metrics["outside_area_ratio"],
        "subject_bbox": metrics.get("subject_bbox"),
        # 多实例主体：证据里带齐实例框与个数，f002 这种「第二件商品自带文字」的
        # 判定能逐框复现（谁被判成标签、为什么），不只留一个 boolean
        "subject_boxes": metrics.get("subject_boxes"),
        "subject_count": metrics.get("subject_count"),
        "watermark_boxes": len(regions["watermark"]),
        "excluded_label_boxes": len(regions["labels"]),
        "boxes": [item["box"] for item in regions["watermark"] + regions["outside"]],
        "thresholds": {"text_area_ratio_max": metrics["text_area_ratio_max"],
                       "subject_cover_ratio_max": metrics["subject_cover_ratio_max"],
                       "min_text_chars": metrics.get("min_text_chars")},
    }
    status = "pass" if decision["action"] == "pass" else "fail"
    return verdict(rule_id, status, score=None, label="overlay_text",
                   evidence=evidence, note=decision["note"])


def overlay_boxes(lines, *, width, height, subject_bbox=None, subject_boxes=None,
                  subject_cover_ratio_max=0.08, min_text_chars=2):
    """修复用：只返回该 inpaint 的框（覆盖主体的水印 + 主体外促销条），
    **不含主体内吊牌、也不含单字符噪声框**。

    吊牌被排除是有意的：inpaint 会把它抹掉，等于篡改商品本体（合规红线）；
    噪声框同理 —— 抹在花纹上只会把商品擦花。`subject_boxes`（多实例主体）与判定
    同口径：第二件商品上的自带文字同样受保护，不能被当成「主体外促销条」擦掉。
    """
    regions = overlay_regions(lines, width=width, height=height,
                              subject_bbox=subject_bbox, subject_boxes=subject_boxes,
                              subject_cover_ratio_max=subject_cover_ratio_max,
                              min_text_chars=min_text_chars)
    return [item["box"] for item in regions["watermark"] + regions["outside"]]


def eval_answer(reply, *, expect_yes, rule_id, label, source="vl", evidence=None):
    """把 `vl.judge` 应答映射成 verdict（`undetermined` 一律不冒充）。"""
    reply = reply or {}
    if reply.get("undetermined"):
        return verdict(
            rule_id, "undetermined", score=None, label=label,
            evidence={"reason": reply.get("reason") or "vl_undetermined",
                      "source": source},
            note="vl_undetermined",
        )
    structured = (reply.get("rules") or [])
    status = None
    score = None
    if structured and isinstance(structured[0], dict):
        status = structured[0].get("status")
        score = structured[0].get("score")
        if structured[0].get("label"):
            label = str(structured[0]["label"])
    raw = str(reply.get("raw") or "")
    if status not in ("pass", "fail", "undetermined"):
        status = _status_from_text(raw, expect_yes=expect_yes)
    if status == "undetermined":
        return verdict(rule_id, "undetermined", score=None, label=label,
                       evidence={"raw": raw, "source": source},
                       note="vl_answer_unparsed")
    payload = {"raw": raw, "source": source}
    if evidence:
        payload.update(evidence)
    return verdict(rule_id, status, score=score, label=label, evidence=payload,
                   note=None if status != "fail" else "vl_fail")


def _status_from_text(raw, *, expect_yes):
    """从自由文本里取「是/否」词表答案；两种都没出现 → undetermined。

    中文里「不是」「不符合」含「是」「符合」，「是否」是问句回声，
    所以先去掉回声词、再按否定优先匹配（否定词更长、更具体）。
    """
    text = (raw or "").strip()
    if not text:
        return "undetermined"
    head = text[:32]
    clean = head.lower()
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
    return verdict(rule_id, "skipped", score=None, label=label, evidence={},
                   note=note)
