"""协议常量与校验（Python SDK 侧投影）。

权威文档：`docs/industry-pack-protocol.md` v1.1。
可执行规格：`tools/pack-conformance/protocol.py` —— 本模块与它**逐字同义**，
差别只有两处，都是为了 SDK 自己生成/自检：
    - `validate_pack_info()`（`pack.info` 应答自检，conformance 03 的断言面）；
    - `pack_info_from()`（由包元数据生成 pack.info，保证与 manifest 不漂移）。

一致性由 `tests/test_protocol_parity.py` 钉住：任何一个常量抄错，单测立刻红。
"""
from __future__ import annotations

# 协议版本：宿主按 major.minor 严格相等匹配，不兼容即拒装。
API_VERSION = "1.1"

# 宿主 → 插件必须实现的六个方法（缺一即 conformance 不过）。
METHODS = ("pack.info", "health", "judge", "repair", "report", "cancel")

# 插件 → 宿主的通知（无 id，宿主忽略；不得当作请求应答）。
NOTIFICATIONS = ("progress", "log")

# 判定状态闭集。`undetermined` 是「无法判定」，不是「失败」也不是「通过」。
STATUSES = ("pass", "fail", "undetermined", "skipped")

# Verdict 七个字段（缺一即协议违例）。
VERDICT_FIELDS = ("ruleId", "status", "score", "label", "evidence", "fixed", "note")

# ---- N5a：结果行契约（`docs/industry-pack-protocol.md` §4.4）----
# 插件种类闭集（manifest `kinds`）：判定类给审核结论，处理类只产出工作版本/导出文件。
KINDS = ("judge", "process")
# 执行结果三值（所有插件都必填）。
RESULT_STATUSES = ("ok", "failed", "skipped")
# 审核结论三态（只有判定类；没有 skipped —— 那是执行结果，不是结论）。
VERDICT_STATUSES = ("pass", "fail", "undetermined")
# 结果行必填键。
ROW_FIELDS = ("assetId", "result")
# 产物只准两种去处。
ARTIFACT_KINDS = ("working_version", "export_file")

# 错误码（JSON-RPC 2.0 保留区之外的自定义码）。
ERR_CAPABILITY_MISSING = -32001   # 网关能力未授权 / 包未声明
ERR_ASSET_UNREADABLE = -32002     # 资产不可读（不存在、权限、解码失败）
ERR_INVALID_PARAMS = -32003       # 参数非法（缺字段、类型错、越界）
ERR_CANCELED = -32004             # 已被宿主取消
ERR_TIMEOUT = -32005              # 插件自身判定超时（宿主超时另走铁律）
ERR_OTHER = -32000                # 其他（必须带 message）
ERR_METHOD_NOT_FOUND = -32601
ERR_PARSE = -32700

PLUGIN_ERROR_CODES = frozenset({
    ERR_CAPABILITY_MISSING, ERR_ASSET_UNREADABLE, ERR_INVALID_PARAMS,
    ERR_CANCELED, ERR_TIMEOUT, ERR_OTHER, ERR_METHOD_NOT_FOUND, ERR_PARSE,
})

# 网关 14 个端点 → 所需能力（manifest.capabilities 白名单）。
# `/op/encode`（v1.1 增量，N6b）沿用 `op.geometric`：确定性无模型像素算子，
# 与原 crop/letterbox/upscale 同一权限档；不新增能力 → api_version 仍是 1.1。
GATEWAY_ENDPOINTS = {
    "/io/read": "io.read",
    "/detect/u2net": "detect.u2net",
    "/detect/bbox": "detect.u2net",
    "/detect/ocr": "detect.ocr",
    "/detect/white_stats": "detect.white",
    "/op/crop": "op.geometric",
    "/op/letterbox": "op.geometric",
    "/op/upscale": "op.geometric",
    "/op/encode": "op.geometric",
    # `/op/watermark`（v1.1 增量，N6c）新增能力 `op.overlay`：纯追加的能力白名单，
    # 未声明的包看不到该端点（403 capability_missing）；api_version 仍为 1.1。
    "/op/watermark": "op.overlay",
    "/op/replace_bg": "op.generative",
    "/op/inpaint": "op.generative",
    "/vl/judge": "vl.judge",
    "/report/emit": "report.emit",
}

KNOWN_CAPABILITIES = frozenset(GATEWAY_ENDPOINTS.values())

MANIFEST_REQUIRED = ("id", "name", "version", "api_version", "entrypoint",
                     "capabilities", "deterministic", "timeout_secs")

PACK_INFO_FIELDS = ("id", "name", "version", "api_version", "rules",
                    "capabilities", "deterministic", "timeout_secs")


def validate_manifest(manifest):
    """返回违例清单（空列表 = 合法）。只校验协议面，不校验业务内容。"""
    problems = []
    if not isinstance(manifest, dict):
        return ["manifest 不是 JSON 对象"]
    for field in MANIFEST_REQUIRED:
        if field not in manifest:
            problems.append("缺字段 %s" % field)
    if problems:
        return problems

    if not isinstance(manifest["id"], str) or not manifest["id"].strip():
        problems.append("id 必须是非空字符串")
    if not isinstance(manifest["name"], str) or not manifest["name"].strip():
        problems.append("name 必须是非空字符串")
    if not isinstance(manifest["version"], str) or not manifest["version"].strip():
        problems.append("version 必须是非空字符串（语义化版本）")
    if manifest["api_version"] != API_VERSION:
        problems.append("api_version=%r，宿主只接受 %r"
                        % (manifest["api_version"], API_VERSION))

    entry = manifest["entrypoint"]
    if not isinstance(entry, dict) or not isinstance(entry.get("cmd"), str) or not entry["cmd"]:
        problems.append("entrypoint 必须是 {cmd, args}")
    elif not isinstance(entry.get("args", []), list):
        problems.append("entrypoint.args 必须是数组")

    caps = manifest["capabilities"]
    if not isinstance(caps, list) or not all(isinstance(c, str) for c in caps):
        problems.append("capabilities 必须是字符串数组")
    else:
        unknown = sorted(set(caps) - KNOWN_CAPABILITIES)
        if unknown:
            problems.append("未知能力 %s（可用：%s）"
                            % (unknown, sorted(KNOWN_CAPABILITIES)))

    if not isinstance(manifest["deterministic"], bool):
        problems.append("deterministic 必须是布尔值")
    timeout = manifest["timeout_secs"]
    if not isinstance(timeout, int) or isinstance(timeout, bool) or not (1 <= timeout <= 3600):
        problems.append("timeout_secs 必须是 1..3600 的整数")
    # N5a：`kinds` 可选；声明了就必须是闭集、非空、不重复（宿主会拒装）。
    kinds = manifest.get("kinds")
    if kinds is not None:
        if not isinstance(kinds, list):
            problems.append("kinds 必须是数组（%s）" % list(KINDS))
        elif not kinds:
            problems.append("kinds 不能为空数组（%s）" % list(KINDS))
        else:
            unknown = [k for k in kinds if k not in KINDS]
            dups = sorted({k for k in kinds if kinds.count(k) > 1})
            if unknown:
                problems.append("kinds 含未知值 %s（可用：%s）" % (unknown, list(KINDS)))
            if dups:
                problems.append("kinds 重复声明 %s" % dups)
    return problems


def validate_verdict(verdict):
    """返回 Verdict 的违例清单（空列表 = 合法）。"""
    if not isinstance(verdict, dict):
        return ["verdict 不是 JSON 对象"]
    problems = []
    missing = [f for f in VERDICT_FIELDS if f not in verdict]
    if missing:
        return ["缺字段 %s" % missing]
    if not isinstance(verdict["ruleId"], str) or not verdict["ruleId"].strip():
        problems.append("ruleId 必须是非空字符串")
    if verdict["status"] not in STATUSES:
        problems.append("status=%r 不在 %s 内" % (verdict["status"], list(STATUSES)))
    if verdict["score"] is not None and not isinstance(verdict["score"], (int, float)):
        problems.append("score 必须是数字或 null")
    if not isinstance(verdict["label"], str):
        problems.append("label 必须是字符串（可为空串）")
    if not isinstance(verdict["evidence"], dict):
        problems.append("evidence 必须是对象（可为 {}）")
    if not isinstance(verdict["fixed"], bool):
        problems.append("fixed 必须是布尔值")
    if verdict["note"] is not None and not isinstance(verdict["note"], str):
        problems.append("note 必须是字符串或 null")
    return problems


def validate_pack_info(info, manifest=None):
    """`pack.info` 应答自检（conformance 03）。给 `manifest` 时同时比对一致性。"""
    if not isinstance(info, dict):
        return ["pack.info 不是对象"]
    problems = ["PackInfo 缺字段 %s" % [f for f in PACK_INFO_FIELDS if f not in info]] \
        if any(f not in info for f in PACK_INFO_FIELDS) else []
    if problems:
        return problems
    rules = info["rules"]
    if not isinstance(rules, list) or not rules:
        problems.append("rules 必须是非空数组")
    else:
        for rule in rules:
            if not isinstance(rule, dict) or not str(rule.get("id") or "").strip():
                problems.append("rules 每项必须有 id：%r" % (rule,))
                break
    if not isinstance(info["capabilities"], list):
        problems.append("capabilities 必须是数组")
    if not isinstance(info["deterministic"], bool):
        problems.append("deterministic 必须是布尔值")
    if manifest:
        for field in ("id", "version", "api_version", "deterministic", "timeout_secs"):
            if info.get(field) != manifest.get(field):
                problems.append("%s 与 manifest 不一致：info=%r manifest=%r"
                                % (field, info.get(field), manifest.get(field)))
        if set(info.get("capabilities") or []) != set(manifest.get("capabilities") or []):
            problems.append("capabilities 与 manifest 不一致")
    return problems


def pack_info_from(pack_id, name, version, rules, capabilities, deterministic,
                   timeout_secs, api_version=API_VERSION, resources=None):
    """由包元数据生成 `pack.info`（SDK 保证与 manifest 同源，不靠人抄）。"""
    info = {
        "id": pack_id,
        "name": name,
        "version": version,
        "api_version": api_version,
        "rules": [dict(rule) for rule in rules],
        "capabilities": list(capabilities),
        "deterministic": bool(deterministic),
        "timeout_secs": int(timeout_secs),
    }
    if resources is not None:
        info["resources"] = list(resources)
    return info


def validate_result_row(row, kinds=("judge",)):
    """返回结果行的违例清单（空列表 = 合法）。口径 = 协议 §4.4 / N5a。

    与 `tools/pack-conformance/protocol.py` 同名函数逐字同义
    （parity 由 `tests/test_protocol_parity.py` 钉住）；宿主侧对应
    `crates/pack-host/src/result.rs::parse_row`。差别的只有一处：宿主会用
    「宿主已知的资产 id」补 `assetId`，包侧不补（包必须自己回）。
    """
    if not isinstance(row, dict):
        return ["结果行不是 JSON 对象"]
    problems = []
    asset_id = row.get("assetId")
    if not (
        (isinstance(asset_id, str) and asset_id.strip())
        or (isinstance(asset_id, int) and not isinstance(asset_id, bool))
    ):
        problems.append("row_asset_id_missing: 缺 assetId（非空字符串或整数）")
    result = row.get("result")
    if result is None:
        problems.append("row_result_missing: 缺 result（%s）" % "/".join(RESULT_STATUSES))
    elif result not in RESULT_STATUSES:
        problems.append("row_result_invalid: result=%r 不在 %s 内（自造状态前端不可渲染）"
                        % (result, "/".join(RESULT_STATUSES)))
    is_judge = "judge" in kinds
    is_process = "process" in kinds
    judge_only = is_judge and not is_process
    rule_id = row.get("ruleId")
    rule_id = rule_id.strip() if isinstance(rule_id, str) else None
    verdict = row.get("verdict")
    if verdict is not None and not is_judge:
        problems.append("row_verdict_not_allowed: 处理类插件（kinds 不含 judge）不得返回 verdict")
    elif verdict is not None:
        if not isinstance(verdict, dict):
            problems.append("row_verdict_invalid: verdict 不是对象：%r" % (verdict,))
        else:
            status = verdict.get("status")
            if status is None:
                problems.append("row_verdict_invalid: verdict 缺 status")
            elif status not in VERDICT_STATUSES:
                problems.append("row_verdict_status_invalid: verdict.status=%r 不在 %s 内"
                                % (status, "/".join(VERDICT_STATUSES)))
            conf = verdict.get("confidence")
            if conf is not None and not (isinstance(conf, (int, float))
                                         and not isinstance(conf, bool) and 0.0 <= conf <= 1.0):
                problems.append("row_verdict_confidence_invalid: confidence=%r 越界（0..=1）"
                                % (conf,))
            code = verdict.get("reasonCode")
            if code is not None and not isinstance(code, str):
                problems.append("row_verdict_invalid: reasonCode 必须是字符串")
    if result == "ok" and is_judge and verdict is None and (judge_only or rule_id):
        problems.append("row_verdict_missing: 判定行 result=ok 必须给 verdict（判不了用 undetermined）")
    if result == "failed":
        err = row.get("error")
        if not isinstance(err, dict) or not (str(err.get("code") or "").strip()) \
                or not (str(err.get("message") or "").strip()):
            problems.append("row_error_missing: result=failed 必须带 error{code,message}")
    if result == "skipped":
        reason = row.get("skipReason")
        if not (isinstance(reason, str) and reason.strip()):
            problems.append("row_skip_reason_missing: result=skipped 必须带 skipReason")
    artifacts = row.get("artifacts")
    if artifacts is not None:
        if not isinstance(artifacts, list):
            problems.append("row_artifact_invalid: artifacts 必须是数组")
        else:
            for art in artifacts:
                if not isinstance(art, dict):
                    problems.append("row_artifact_invalid: 产物不是对象：%r" % (art,))
                    continue
                kind = art.get("kind")
                if kind not in ARTIFACT_KINDS:
                    problems.append("row_artifact_invalid: kind=%r 不是 %s"
                                    % (kind, "/".join(ARTIFACT_KINDS)))
                elif kind == "working_version":
                    for key in ("assetId", "versionId"):
                        if not (isinstance(art.get(key), str) and art[key].strip()):
                            problems.append("row_artifact_invalid: %s 产物缺 %s" % (kind, key))
                else:
                    for key in ("path", "sha256"):
                        if not (isinstance(art.get(key), str) and art[key].strip()):
                            problems.append("row_artifact_invalid: %s 产物缺 %s" % (kind, key))
    for key in ("evidence", "fields"):
        val = row.get(key)
        if val is not None and not isinstance(val, dict):
            problems.append("row_%s_not_object: %s 必须是对象" % (key, key))
    return problems
