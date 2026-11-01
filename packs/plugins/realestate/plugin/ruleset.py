"""房产包（官方包）—— 规则集、阈值与纯判定函数。

本模块**不碰网关、不碰文件系统**：全部是「测量值 → verdict」的纯函数，
判定语义因此可以被单测与语料矩阵逐条钉住（像素测量在 `imagedec.py`，
网关调用与编排在 `__init__.py`）。

规则集来源：`packs/realestate/{rules,pipeline}.yaml`（旧引擎的 4 条 F1..F4 +
C11 追加的 F0 领域 gate），迁移到插件形态时保留**全部阈值与提示词原样**，
只做两处能力面改写：

- F1/F2（classic 像素统计）：宿主网关 v1 没有 classic 端点 → 包内用标准库
  解码出 Rec.709 luma 自己算（`imagedec.py`），口径与
  `crates/engine-classic/src/detectors.rs` 逐项相同（内核/分母/死区边界都对齐）。
  调用方若已算好测量值（`options.measurements`），优先用调用方的，避免重算。
- F3/F4（per_group）：插件协议 judge 是**逐资产**调用，没有组级 job 概念 →
  插件自己把「资产所在目录」当组（`options.group_dir` 可覆盖），逐成员测量后
  在本地汇总，结论按当前资产落一条 verdict。

F5_min_size 是**本形态新增**的一条（旧引擎没有）：
卡面要求本包具备「仅几何修复」的能力面，而 F0~F4 的失败**都不可算法修复**
（房源图修图 = 失真，见 rules.yaml 的裁决），没有可修规则时
conformance 的「repair 产物落盘」断言无实现可验。F5 取房源平台上传的最低
硬要求（短边 ≥800，链家/贝壳/安居客同档），修复方式只有 `upscale`（几何，
禁生成式），修完短边达标即 fixed=true；其余规则失败一律**转人工**
（undetermined + 原因），不自动放行。

阈值口径（`packs/realestate/rules.yaml`，C7 真图标定）：
    F1 拉普拉斯方差 ≥ 100（≈ 真图 p5，只拦明显糊的）
    F2 死区占比 ≤ 0.30（≈ p73）、均值亮度 ∈ [30, 245]
    F3 pHash 汉明距离 ≤ 8 判重复；`on_fail: pass`（软规则：只标记，不判不合格）
    F4 组内必须齐备 [客厅, 卧室, 厨房, 卫生间]
"""
from __future__ import annotations

# --- 规则清单（PackInfo.rules / resources/rules.json 的同一份事实源） --------
# scope：per_asset（逐图）| per_group（按目录成组）。fixer：null | "upscale"。
RULES = [
    {
        "id": "F0_domain",
        "name": "房源图领域 gate（是不是房源/室内实景照片）",
        "executor": "vl",
        "deterministic": False,
        "scope": "per_asset",
        "fixer": None,
        "on_fail": "fail",
        "fail_label": "非房源图",
    },
    {
        "id": "F1_sharpness",
        "name": "房源图清晰（拉普拉斯方差 ≥100）",
        "executor": "classic",
        "deterministic": True,
        "scope": "per_asset",
        "fixer": None,
        "on_fail": "fail",
        "fail_label": "图片模糊（拉普拉斯方差不足）",
    },
    {
        "id": "F2_exposure",
        "name": "曝光正常（死区占比 ≤0.30 且均值亮度 30~245）",
        "executor": "classic",
        "deterministic": True,
        "scope": "per_asset",
        "fixer": None,
        "on_fail": "fail",
        "fail_label": "曝光异常（死区过多 / 过曝欠曝）",
    },
    {
        "id": "F3_duplicate",
        "name": "重复房照（同组 pHash 近似重复，仅标记）",
        "executor": "classic",
        "deterministic": True,
        "scope": "per_group",
        "fixer": None,
        "on_fail": "pass",
        "fail_label": "与同组其它房照重复",
    },
    {
        "id": "F4_scene_complete",
        "name": "必要场景齐套（客厅/卧室/厨房/卫生间）",
        "executor": "vl",
        "deterministic": False,
        "scope": "per_group",
        "fixer": None,
        "on_fail": "fail",
        "fail_label": "必要场景缺失（客厅/卧室/厨房/卫生间）",
    },
    {
        "id": "F5_min_size",
        "name": "尺寸达标（长边 ≥800 且短边 ≥600，可几何放大修复）",
        "executor": "classic",
        "deterministic": True,
        "scope": "per_asset",
        "fixer": "upscale",
        "on_fail": "auto_fix",
        "fail_label": "尺寸不足（低于平台上传最低档 800×600）",
    },
]

RULE_IDS = [r["id"] for r in RULES]

# 执行序 = pipeline.yaml 声明序（F0 gate 首位；本形态把 F5 排在 gate 之后、
# 像素规则之前 —— 尺寸是元数据档，最便宜）
PIPELINE_ORDER = [
    "F0_domain",
    "F5_min_size",
    "F1_sharpness",
    "F2_exposure",
    "F3_duplicate",
    "F4_scene_complete",
]

GROUP_RULES = [r["id"] for r in RULES if r["scope"] == "per_group"]

# --- 阈值（单一定义点；resources/rules.json 与之一致，有单测钉住） -----------
THRESHOLDS = {
    "F1_sharpness": {"min": 100.0},
    "F2_exposure": {"max_dead_ratio": 0.30, "black_cut": 10, "white_cut": 10,
                    "min_mean": 30.0, "max_mean": 245.0},
    "F3_duplicate": {"threshold": 8},
    "F4_scene_complete": {"require_all": ["客厅", "卧室", "厨房", "卫生间"]},
    # 房源平台上传最低档 = 800×600（长边 ≥800 且短边 ≥600）
    "F5_min_size": {"long_side_min": 800, "short_side_min": 600},
}

# 修复策略：本包只有几何放大（房源图禁止生成式重绘 —— 修图=失真）
FIXABLE = {"F5_min_size": "upscale"}
MAX_ROUNDS = 1                    # rule.yaml recheck_max（无修复器时代留 1）
FIX_POLICY = "geometric"

# --- 词表与提示词 ------------------------------------------------------------
SCENE_OPTIONS = ["客厅", "卧室", "厨房", "卫生间", "阳台", "园区", "户型图", "其它"]
REQUIRE_ALL_SCENES = ["客厅", "卧室", "厨房", "卫生间"]

# pipeline.yaml 的 vl_intro：逐图判定与组级归类共用这一句
VL_INTRO = "这是一套待上架的房源照片（同一目录归为同一套房源；下面每次只给你其中一张）。"

# COMPLIANCE-DOMAIN 定稿 v4（真图 132 张 4 轮迭代 + 独立模型交叉核对，勿改）
PROMPT_DOMAIN = (
    "这张图片是不是住宅房源的室内实景照片（含房源户型图/平面图）？先按下面的判定规则判断，再回答。"
    "判定规则：只有当画面主体是住宅（公寓/别墅/自建房）内部的居住空间——客厅、卧室、厨房、卫生间、"
    "餐厅、阳台、园区庭院——或是这套房源的户型图/平面图时，才回答「是」。以下一律回答「否」："
    "医院/诊所、商场/写字楼、餐厅、酒店、宿舍、公共卫生间等公共或商用空间；"
    "设备、商品、器材、家具的特写；绘画、插画、效果图、3D 渲染、截图、纯文字图；"
    "人像、宠物、食物、汽车、风景。只回答「是」或「否」。"
)

PROMPT_SCENE = "这张照片属于下列哪一类场景（只选一个）：%s。只回一个词。" % "、".join(SCENE_OPTIONS)

# 组内成员上限（防止一个目录几千张图把单次 judge 拖死；超出部分记进证据）
GROUP_MEMBER_MAX = 24


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


def skipped(rule_id, note, label="not_applicable"):
    return verdict(rule_id, "skipped", score=None, label=label, evidence={}, note=note)


def undetermined(rule_id, note, label, evidence=None):
    return verdict(rule_id, "undetermined", score=None, label=label,
                   evidence=evidence or {}, note=note)


def thresholds_for(rule_id, overrides=None):
    """包默认阈值 + 调用方覆盖（`options.thresholds`）。"""
    base = dict(THRESHOLDS.get(rule_id) or {})
    if overrides:
        for key, value in (overrides.get(rule_id) or {}).items():
            base[key] = value
    return base


def rule_meta(rule_id):
    for rule in RULES:
        if rule["id"] == rule_id:
            return rule
    return {}


def domain_gate_blocks(status):
    """F0 不是 pass 时后续规则一律 skipped（pipeline.yaml 的 depends_on 语义）。"""
    return status != "pass"


# ---------------------------------------------------------------------------
# 像素测量（纯函数，输入 luma 平面；口径对齐 engine-classic）
# ---------------------------------------------------------------------------
def laplacian_variance(luma, width, height):
    """灰度拉普拉斯核 [0,1,0;1,-4,1;0,1,0] 响应的方差（只算非边界像素）。

    与 `engine-classic::sharpness_laplacian_variance` 同一实现（含总体方差分母）。
    图不足 3x3 抛 ValueError（无法判锐度）。
    """
    if width < 3 or height < 3:
        raise ValueError("%dx%d 太小，拉普拉斯需 >= 3x3" % (width, height))
    if len(luma) != width * height:
        raise ValueError("luma 长度 %d 与 %dx%d 不符" % (len(luma), width, height))
    total = 0.0
    total_sq = 0.0
    n = 0
    w = width
    for y in range(1, height - 1):
        base = y * w
        for x in range(1, w - 1):
            i = base + x
            lap = luma[i - 1] + luma[i + 1] + luma[i - w] + luma[i + w] - 4 * luma[i]
            total += lap
            total_sq += lap * lap
            n += 1
    mean = total / n
    return total_sq / n - mean * mean, n


def exposure_stats(luma, *, black_cut=10, white_cut=10):
    """死黑/死白占比 + 均值亮度（边界压线即死区，与 engine-classic 一致）。"""
    n = len(luma)
    if not n:
        raise ValueError("空图")
    white_line = 255 - int(white_cut)
    black = 0
    white = 0
    total = 0
    for value in luma:
        total += value
        if value <= black_cut:
            black += 1
        if value >= white_line:
            white += 1
    return {
        "dead_black_ratio": black / n,
        "dead_white_ratio": white / n,
        "dead_ratio": (black + white) / n,
        "mean": total / n,
    }


_DCT_CACHE = {}


def _dct_matrix(size):
    matrix = _DCT_CACHE.get(size)
    if matrix is None:
        import math
        factor = math.pi / (2.0 * size)
        matrix = [[math.cos((2 * x + 1) * u * factor) for x in range(size)]
                  for u in range(size)]
        _DCT_CACHE[size] = matrix
    return matrix


def _box_downscale(plane, width, height, target):
    """任意尺寸灰度平面 → target×target（面积平均，最近邻退化时取中心像素）。"""
    out = [0.0] * (target * target)
    for ty in range(target):
        y0 = ty * height // target
        y1 = max(y0 + 1, (ty + 1) * height // target)
        for tx in range(target):
            x0 = tx * width // target
            x1 = max(x0 + 1, (tx + 1) * width // target)
            total = 0
            count = 0
            for y in range(y0, y1):
                row = y * width
                for x in range(x0, x1):
                    total += plane[row + x]
                    count += 1
            out[ty * target + tx] = total / count if count else 0.0
    return out


def phash(plane, width, height, *, hash_size=8, img_size=32):
    """感知哈希（DCT 法）：32×32 灰度 → 8×8 低频（去 DC）→ 中位数二值，返回整数。"""
    small = _box_downscale(plane, width, height, img_size)
    cos = _dct_matrix(img_size)
    # 行变换
    tmp = [[0.0] * img_size for _ in range(img_size)]
    for u in range(img_size):
        cu = cos[u]
        for y in range(img_size):
            row = small[y * img_size:(y + 1) * img_size]
            acc = 0.0
            for x in range(img_size):
                acc += row[x] * cu[x]
            tmp[y][u] = acc
    # 列变换（只要前 hash_size 列）
    values = []
    for v in range(hash_size):
        cv = cos[v]
        for u in range(hash_size):
            acc = 0.0
            for y in range(img_size):
                acc += tmp[y][u] * cv[y]
            if u == 0 and v == 0:
                continue
            values.append(acc)
    ordered = sorted(values)
    mid = len(ordered) // 2
    median = ordered[mid] if len(ordered) % 2 else (ordered[mid - 1] + ordered[mid]) / 2.0
    bits = 0
    for index, value in enumerate(values):
        if value > median:
            bits |= 1 << index
    return bits


def hamming(a, b):
    return bin(int(a) ^ int(b)).count("1")


# ---------------------------------------------------------------------------
# 判定：测量值 → verdict
# ---------------------------------------------------------------------------
def eval_domain(reply):
    """F0：VL 词表答案（是/否）→ pass/fail/undetermined（不冒充）。"""
    return eval_answer(reply, expect_yes=True, rule_id="F0_domain",
                       label="domain_gate", evidence={"fail_label": "非房源图"})


def eval_sharpness(variance, pixels, *, minimum=100.0, rule_id="F1_sharpness",
                   source="classic_laplacian"):
    value = float(variance)
    ok = value >= float(minimum)
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=round(value, 3), label="laplacian_variance",
        evidence={"variance": round(value, 3), "threshold": float(minimum),
                  "pixels": int(pixels), "source": source},
        note=None if ok else "below_sharpness_min",
    )


def eval_exposure(stats, *, max_dead_ratio=0.30, min_mean=30.0, max_mean=245.0,
                  black_cut=10, white_cut=10, rule_id="F2_exposure",
                  source="classic_exposure"):
    dead = float(stats.get("dead_ratio") or 0.0)
    mean = float(stats.get("mean") or 0.0)
    too_much_dead = dead > float(max_dead_ratio)
    mean_low = mean < float(min_mean)
    mean_high = mean > float(max_mean)
    ok = not (too_much_dead or mean_low or mean_high)
    if too_much_dead:
        note = "dead_zone_ratio_high"
    elif mean_low:
        note = "mean_luma_too_dark"
    elif mean_high:
        note = "mean_luma_too_bright"
    else:
        note = None
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=round(dead, 4), label="dead_zone_ratio",
        evidence={"dead_ratio": round(dead, 4),
                  "dead_black_ratio": round(float(stats.get("dead_black_ratio") or 0.0), 4),
                  "dead_white_ratio": round(float(stats.get("dead_white_ratio") or 0.0), 4),
                  "mean": round(mean, 2),
                  "max_dead_ratio": float(max_dead_ratio),
                  "mean_range": [float(min_mean), float(max_mean)],
                  "black_cut": int(black_cut), "white_cut": int(white_cut),
                  "source": source},
        note=note,
    )


def eval_duplicate(distance, *, threshold=8, sibling=None, compared=0,
                   rule_id="F3_duplicate"):
    """F3：与组内其它成员的最小汉明距离 ≤ threshold 判重复。

    `on_fail: pass`（软规则）：判 fail 时 note 明写「仅标记」，宿主/报告按 note 区分，
    不当作不合格结论 —— 与旧引擎 rules.yaml 的语义一致。
    """
    if distance is None:
        return undetermined(rule_id, "no_comparable_sibling", "phash_distance",
                            {"compared": int(compared)})
    value = int(distance)
    ok = value > int(threshold)
    return verdict(
        rule_id, "pass" if ok else "fail",
        score=float(value), label="phash_distance",
        evidence={"min_distance": value, "threshold": int(threshold),
                  "nearest": sibling or "", "compared": int(compared),
                  "source": "classic_phash", "soft_rule": True},
        note=None if ok else "duplicate_marked(仅标记，不判不合格)",
    )


def eval_scene_complete(answers, *, require_all=None, probed=0, unprobed=0,
                        failed=0, rule_id="F4_scene_complete"):
    """F4：组内逐图归类 → 覆盖哪些场景；缺任一 require_all 场景判 fail。

    `answers` = {成员: 场景词或 ""（未解析出词表答案）}。
    只要**有成员归类失败**（VL 不可判 / 词表外答案），结论落 undetermined 转人工 ——
    拿不全证据就判「缺场景」等于把 VL 故障当业务结论。
    """
    require = list(require_all or REQUIRE_ALL_SCENES)
    seen = {}
    unknown = []
    for member, scene in (answers or {}).items():
        if scene in SCENE_OPTIONS:
            seen.setdefault(scene, []).append(member)
        else:
            unknown.append(member)
    missing = [scene for scene in require if scene not in seen]
    evidence = {
        "require_all": require,
        "seen": {scene: sorted(members) for scene, members in sorted(seen.items())},
        "missing": missing,
        "probed": int(probed),
        "unprobed": int(unprobed),
        "unclassified": sorted(unknown),
        "source": "vl_group_judge",
    }
    if unknown and not seen:
        return undetermined(rule_id, "group_classification_incomplete", "scene_coverage",
                            evidence)
    if unknown:
        evidence["partial"] = True
        return undetermined(rule_id, "group_classification_partial", "scene_coverage",
                            evidence)
    if missing:
        covered = len([scene for scene in require if scene in seen])
        return verdict(rule_id, "fail", score=round(covered / float(len(require)), 3),
                       label="scene_coverage", evidence=evidence,
                       note="missing_scene:" + ",".join(missing))
    return verdict(rule_id, "pass", score=1.0, label="scene_coverage",
                   evidence=evidence)


def eval_size(width, height, *, long_side_min=800, short_side_min=600,
              rule_id="F5_min_size"):
    """F5：平台上传最低档 800×600（长边 ≥800 且短边 ≥600；元数据档，不解码）。

    score = 两条门的**更紧**那个（min(长边/门槛, 短边/门槛)，上下限 1.0），
    这样「几近达标」与「差一大截」在报告里可区分。
    """
    width = int(width)
    height = int(height)
    short = min(width, height)
    long_side = max(width, height)
    ok_short = short >= int(short_side_min)
    ok_long = long_side >= int(long_side_min)
    ok = ok_short and ok_long
    ratio = min(long_side / float(long_side_min), short / float(short_side_min))
    score = 1.0 if ok else round(max(0.0, ratio), 3)
    if ok:
        note = None
    elif not ok_long:
        note = "below_long_side_min"
    else:
        note = "below_short_side_min"
    return verdict(
        rule_id, "pass" if ok else "fail", score=score, label="min_side",
        evidence={"width": width, "height": height, "short_side": short,
                  "long_side": long_side,
                  "long_side_min": int(long_side_min),
                  "short_side_min": int(short_side_min),
                  "source": "metadata"},
        note=note,
    )


def eval_answer(reply, *, expect_yes, rule_id, label, evidence=None,
                source="vl"):
    """把 `vl.judge` 应答映射成 verdict（`undetermined` 一律不冒充）。"""
    reply = reply or {}
    if reply.get("undetermined"):
        return undetermined(rule_id, "vl_undetermined", label,
                            {"reason": reply.get("reason") or "vl_undetermined",
                             "source": source})
    structured = reply.get("rules") or []
    status = None
    score = None
    if structured and isinstance(structured[0], dict):
        status = structured[0].get("status")
        score = structured[0].get("score")
        if structured[0].get("label"):
            label = str(structured[0]["label"])
    raw = str(reply.get("raw") or "")
    if status not in ("pass", "fail"):
        status = _status_from_text(raw, expect_yes=expect_yes)
    if status == "undetermined":
        return undetermined(rule_id, "vl_answer_unparsed", label,
                            {"raw": raw, "source": source})
    payload = {"raw": raw, "source": source}
    if evidence:
        payload.update(evidence)
    return verdict(rule_id, status, score=score, label=label, evidence=payload,
                   note=None if status != "fail" else "domain_not_realestate")


def pick_scene(reply):
    """组级归类应答 → 词表里的一个场景词（拿不到 → ""，不猜）。"""
    reply = reply or {}
    if reply.get("undetermined"):
        return ""
    candidates = []
    structured = reply.get("rules") or []
    if structured and isinstance(structured[0], dict):
        candidates.append(str(structured[0].get("label") or ""))
    candidates.append(str(reply.get("raw") or ""))
    for text in candidates:
        if not text:
            continue
        for scene in SCENE_OPTIONS:
            if scene in text:
                return scene
    return ""


def _status_from_text(raw, *, expect_yes):
    """从自由文本里取「是/否」词表答案；两种都没出现 → undetermined。"""
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


def needs_human_verdict(rule_id, base_note):
    """不可修复的失败 → 强制转人工（undetermined + 原因，不自动放行）。"""
    meta = rule_meta(rule_id)
    return undetermined(
        rule_id, "needs_human:" + (base_note or "unrepairable"),
        "manual_review_required",
        {"fail_label": meta.get("fail_label") or "",
         "fixer": meta.get("fixer"), "policy": FIX_POLICY},
    )
