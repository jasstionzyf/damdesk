"""Verdict 构造器（协议 §3：七个字段，缺一即违例）。

为什么用函数而不是 dataclass：Verdict 要**直接落 JSON / 落 evidence / 落 DB**，
dict 是最省事的载体（官方电商包与 conformance 都是 dict 口径）；
构造器只负责把七字段填满并挡住 `status` 写错这种低级错误。

    verdict("R1_size", "pass", score=2336.0, label="短边 2336 >= 1600")
    failed("R1_size", score=600, label="短边不足")
    skipped("R5_attribute", note="outputs_only:attribute_tagging")

设计口径（与 `references/fix-policy-and-a4.md` 一致）：
    - `score` 只放**本条规则的度量**，别放「修复后的预测值」；
    - `fixed=True` 只在**复检通过**后置真，表示这条结论是修复后的；
    - `undetermined` 必须带 `note`（用户可见的原因），否则等于「静默放行」。
"""
from __future__ import annotations

from .errors import RpcError
from .protocol import ERR_INVALID_PARAMS, STATUSES, validate_verdict


def verdict(rule_id, status, score=None, label="", evidence=None, fixed=False, note=None):
    """构造一条 Verdict 并自检（自检失败抛 `RpcError(-32003)`，不会写出半截字段）。"""
    if status not in STATUSES:
        raise RpcError(ERR_INVALID_PARAMS, "status=%r 不在 %s 内" % (status, list(STATUSES)))
    out = {
        "ruleId": str(rule_id),
        "status": status,
        "score": None if score is None else float(score),
        "label": str(label or ""),
        "evidence": dict(evidence or {}),
        "fixed": bool(fixed),
        "note": None if note is None else str(note),
    }
    if status == "undetermined" and not out["note"]:
        raise RpcError(ERR_INVALID_PARAMS,
                       "%s 判 undetermined 必须带 note（说明为什么判定不了）" % rule_id)
    problems = validate_verdict(out)
    if problems:
        raise RpcError(ERR_INVALID_PARAMS, "Verdict 非法：%s" % "；".join(problems))
    return out


def passed(rule_id, score=None, label="", **kw):
    return verdict(rule_id, "pass", score=score, label=label, **kw)


def failed(rule_id, score=None, label="", **kw):
    return verdict(rule_id, "fail", score=score, label=label, **kw)


def skipped(rule_id, note=None, label="", score=None, **kw):
    return verdict(rule_id, "skipped", score=score, label=label, note=note, **kw)


def undetermined(rule_id, note, score=None, label="", **kw):
    """判定不了（模型不可用/崩了/证据不足）——**不是** fail，别拿来兜底。"""
    return verdict(rule_id, "undetermined", score=score, label=label, note=note, **kw)
