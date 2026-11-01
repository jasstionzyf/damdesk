"""结果行构造器（N5a 契约：`result` 必填、`verdict` 只属判定类）。

**执行结果 ≠ 判定结论**：所有插件都要回 `result(ok|failed|skipped)`；
只有 `manifest.outputs` 含 `verdict` 的插件才有 `verdict(pass|fail|undetermined)`。
处理类插件（resize/换背景/超分…）**永不填 verdict** —— 伪造一个 pass 换取
「界面看起来完整」是审核信任红线，宿主侧会直接拒收整批应答
（`plugin_verdict_not_allowed`）。

    from dampack import process_row, judge_row, failed_row

    process_row("asset-42", outputs=["artifact"],
                artifacts=[working_version("asset-42", "v3")])
    judge_row("asset-42", rule_id="A4", status="fail", confidence=0.31,
              reason_code="A4_subject_ratio")
    failed_row("asset-43", "asset_unreadable", "解码失败", outputs=["artifact"])

口径（与 `crates/pack-host/src/result.rs`、`tools/pack-conformance/protocol.py` 一致）：
    - 行键 = `assetId`（+ `ruleId`）；判定类按规则出行，处理类按资产出行；
    - `result=failed` 必须带 `error{code,message}`；`result=skipped` 必须带 `skipReason`；
    - `verdict.status` 三态闭集，`skipped` **不是**结论（那是执行结果）；
    - 产物只准两种去处：资产工作版本 / 导出文件（带 sha256），不许自建目录树。
"""
from __future__ import annotations

from .errors import RpcError
from .protocol import (
    ARTIFACT_KINDS,
    ERR_INVALID_PARAMS,
    OUTPUTS,
    RESULT_STATUSES,
    ROW_FIELDS,
    VERDICT_STATUSES,
    validate_result_row,
)


def _norm_outputs(outputs):
    if outputs is None:
        return ("verdict",)
    if isinstance(outputs, str):
        outputs = [outputs]
    out = []
    for item in outputs:
        if item not in OUTPUTS:
            raise RpcError(ERR_INVALID_PARAMS, "outputs=%r 不在 %s 内" % (item, list(OUTPUTS)))
        if item not in out:
            out.append(item)
    if not out:
        raise RpcError(ERR_INVALID_PARAMS, "outputs 不能为空（判定类 verdict / 处理类 artifact）")
    return tuple(out)


def working_version(asset_id, version_id):
    """产物：资产工作版本（处理类插件的正常去处）。"""
    return {"kind": "working_version", "assetId": str(asset_id), "versionId": str(version_id)}


def export_file(path, sha256):
    """产物：导出文件（必须带 sha256，便于复现与对账）。"""
    return {"kind": "export_file", "path": str(path), "sha256": str(sha256)}


def result_row(asset_id, result, outputs=None, rule_id=None, verdict=None, artifacts=None,
               evidence=None, fields=None, error=None, skip_reason=None):
    """构造一行结果并自检（自检失败抛 `RpcError(-32003)`，不会写出半截行）。"""
    out = {"assetId": str(asset_id), "result": result}
    if rule_id is not None:
        out["ruleId"] = str(rule_id)
    if verdict is not None:
        out["verdict"] = dict(verdict)
    if artifacts:
        out["artifacts"] = [dict(a) for a in artifacts]
    if evidence:
        out["evidence"] = dict(evidence)
    if fields:
        out["fields"] = dict(fields)
    if error is not None:
        out["error"] = {"code": str(error[0]), "message": str(error[1])}
    if skip_reason is not None:
        out["skipReason"] = str(skip_reason)
    problems = validate_result_row(out, _norm_outputs(outputs))
    if problems:
        raise RpcError(ERR_INVALID_PARAMS, "结果行非法：%s" % "；".join(problems))
    return out


def judge_row(asset_id, rule_id, status, confidence=None, reason_code=None, outputs=None,
              evidence=None, fields=None, artifacts=None):
    """判定行（`verdict` 块由这里收口：状态闭集 + 置信度区间）。"""
    if status not in VERDICT_STATUSES:
        raise RpcError(ERR_INVALID_PARAMS,
                       "row_verdict_status_invalid: verdict.status=%r 不在 %s 内"
                       "（判不了用 undetermined，别用 skipped）"
                       % (status, list(VERDICT_STATUSES)))
    verdict = {"status": status}
    if confidence is not None:
        verdict["confidence"] = float(confidence)
    if reason_code is not None:
        verdict["reasonCode"] = str(reason_code)
    return result_row(asset_id, "ok", outputs=outputs or ("verdict",), rule_id=rule_id,
                      verdict=verdict, artifacts=artifacts, evidence=evidence, fields=fields)


def process_row(asset_id, outputs=None, artifacts=None, evidence=None, fields=None):
    """处理行（只有执行结果与产物，**没有** verdict —— 传了也构造不出来）。"""
    outputs = _norm_outputs(outputs or ("artifact",))
    if "verdict" in outputs:
        # 混合包里想表达「处理行」：显式给 artifact，避免 verdict 必填规则误伤
        outputs = ("artifact",)
    return result_row(asset_id, "ok", outputs=outputs, artifacts=artifacts,
                      evidence=evidence, fields=fields)


def failed_row(asset_id, code, message, outputs=None, rule_id=None, evidence=None):
    """执行失败行（`error{code,message}` 必填）。"""
    return result_row(asset_id, "failed", outputs=outputs, rule_id=rule_id,
                      error=(code, message), evidence=evidence)


def skipped_row(asset_id, skip_reason, outputs=None, rule_id=None, fields=None):
    """没跑的行（`skipReason` 必填：原图缺失 / 不适用 / 未选择）。"""
    return result_row(asset_id, "skipped", outputs=outputs, rule_id=rule_id,
                      skip_reason=skip_reason, fields=fields)


def reply(rows):
    """一次调用的应答体：`{"rows":[...]}`（宿主按行归一化）。"""
    return {"rows": [dict(r) for r in rows]}


__all__ = [
    "ARTIFACT_KINDS",
    "OUTPUTS",
    "RESULT_STATUSES",
    "ROW_FIELDS",
    "VERDICT_STATUSES",
    "export_file",
    "failed_row",
    "judge_row",
    "process_row",
    "reply",
    "result_row",
    "skipped_row",
    "working_version",
]
