//! 结果行契约：manifest `outputs` × 应答 → 结果行（公开 API 层，纯函数）。
//!
//! 覆盖三件事：
//! 1. `outputs` 从 manifest 解析到运行期口径（v2.0 必填、无缺省、严格校验）；
//! 2. 处理类插件（`outputs=["artifact"]`）返回 verdict → **拒收**（`row_verdict_not_allowed`）；
//! 3. 旧判定应答（`verdicts[]`）逐规则 1:1 归一化成行（N5b 的 backfill 映射）。

use pack_host::result::{parse_row, rows_from_reply, ResultStatus, RowViolation, VerdictStatus};
use pack_host::{Manifest, PluginOutput};
use serde_json::{json, Value};

fn manifest(outputs: Value) -> Manifest {
    let mut v = json!({
        "id": "com.soujpg.pack.imaging",
        "name": "图片处理插件",
        "version": "0.1.0",
        "api_version": "2.0",
        "outputs": outputs,
        "entrypoint": {"cmd": "python3", "args": ["plugin/__init__.py"]},
        "capabilities": ["io.read"],
        "deterministic": true,
        "timeout_secs": 60
    });
    let raw = v.to_string();
    Manifest::from_slice(raw.as_bytes()).expect("manifest 应可解析")
}

#[test]
fn t01_manifest_outputs_reach_runtime_shape() {
    assert_eq!(
        manifest(json!(["verdict"])).effective_outputs(),
        [PluginOutput::Verdict]
    );
    assert_eq!(
        manifest(json!(["artifact"])).effective_outputs(),
        [PluginOutput::Artifact]
    );
    assert_eq!(
        manifest(json!(["verdict", "artifact"])).effective_outputs(),
        [PluginOutput::Verdict, PluginOutput::Artifact]
    );
}

#[test]
fn t02_process_pack_row_is_ok_with_artifacts_and_no_verdict() {
    let m = manifest(json!(["artifact"]));
    let outputs = m.effective_outputs();
    let row = parse_row(
        outputs,
        &json!({
            "assetId": 42,
            "result": "ok",
            "artifacts": [{"kind": "working_version", "assetId": "42", "versionId": "v1"}],
            "evidence": {"op": "resize", "elapsed_ms": 120},
            "fields": {"out_width": 1600, "out_height": 1600}
        }),
    )
    .expect("处理类行应合法");
    assert_eq!(row.result, ResultStatus::Ok);
    assert!(row.verdict.is_none(), "处理类不得有 verdict");
    assert_eq!(row.artifacts.len(), 1);
}

#[test]
fn t03_process_pack_returning_verdict_is_rejected_and_coded() {
    let m = manifest(json!(["artifact"]));
    let outputs = m.effective_outputs();
    // 单行形态
    let err = parse_row(
        outputs,
        &json!({"assetId": 42, "result": "ok",
                "verdict": {"status": "pass", "confidence": 1.0}}),
    )
    .unwrap_err();
    assert_eq!(err, RowViolation::VerdictNotAllowed);
    assert_eq!(err.code(), "row_verdict_not_allowed");
    // 旧判定应答形态（整批拒收，不部分采信）
    let err = rows_from_reply(
        outputs,
        &json!({"asset_id": "42", "verdicts": [
            {"ruleId": "R1", "status": "pass", "score": 1.0, "label": "", "evidence": {}, "fixed": false, "note": null}
        ]}),
    )
    .unwrap_err();
    assert_eq!(err, RowViolation::VerdictNotAllowed);
}

#[test]
fn t04_verdict_pack_needs_verdict_on_ok_rows() {
    let m = manifest(json!(["verdict"]));
    let outputs = m.effective_outputs();
    let err = parse_row(outputs, &json!({"assetId": 1, "result": "ok"})).unwrap_err();
    assert_eq!(err, RowViolation::VerdictMissing);
    let row = parse_row(
        outputs,
        &json!({"assetId": 1, "result": "ok",
                "verdict": {"status": "undetermined", "reasonCode": "vl_unavailable"}}),
    )
    .unwrap();
    assert_eq!(
        row.verdict.unwrap().status,
        VerdictStatus::Undetermined,
        "判不了就是 undetermined，不是 pass"
    );
}

#[test]
fn t05_mixed_pack_may_return_both_output_types_of_rows() {
    let m = manifest(json!(["verdict", "artifact"]));
    let outputs = m.effective_outputs();
    let rows = rows_from_reply(
        outputs,
        &json!({"rows": [
            {"assetId": "a1", "result": "ok", "verdict": {"status": "fail", "reasonCode": "R3_no_watermark"}},
            {"assetId": "a2", "result": "ok",
             "artifacts": [{"kind": "working_version", "assetId": "a2", "versionId": "v9"}]}
        ]}),
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].verdict.as_ref().unwrap().reason_code.as_deref(),
        Some("R3_no_watermark")
    );
    assert!(rows[1].verdict.is_none());
}

#[test]
fn t06_legacy_reply_maps_one_row_per_rule_and_keeps_text_in_evidence() {
    let m = manifest(json!(["verdict"]));
    let outputs = m.effective_outputs();
    let rows = rows_from_reply(
        outputs,
        &json!({"asset_id": "asset-7", "verdicts": [
            {"ruleId": "A2", "status": "fail", "score": 600.0, "label": "短边不足",
             "evidence": {"short_side": 600}, "fixed": false, "note": "need>=1600"},
            {"ruleId": "A3", "status": "pass", "score": 1.0, "label": "白底",
             "evidence": {}, "fixed": false, "note": null}
        ]}),
    )
    .unwrap();
    assert_eq!(rows.len(), 2, "逐规则一行（与 rule_verdict 表 1:1）");
    assert_eq!(rows[0].rule_id.as_deref(), Some("A2"));
    assert_eq!(rows[0].result, ResultStatus::Ok);
    assert_eq!(
        rows[0].verdict.as_ref().unwrap().status,
        VerdictStatus::Fail
    );
    // score 600 → 夹到 1.0（confidence 只收 0..=1，别把度量当把握度乱塞）
    assert_eq!(rows[0].verdict.as_ref().unwrap().confidence, Some(1.0));
    assert_eq!(rows[0].evidence["label"], "短边不足");
    assert_eq!(
        rows[1].verdict.as_ref().unwrap().status,
        VerdictStatus::Pass
    );
}

/// 处理类的「合法跳过」（K07-FIX-N `t_5c968bc9`）：`result=ok` + `skipReason` + **空 artifacts**。
///
/// 这条锁的是语义边界，不是宽松：`skipped` = 这一行没跑；`ok+skipReason` = 跑完了、结论是 no-op
/// （例：抠图包的「输入本来就是白底」→ 不产生产物、不落新工作版本）。两者都不许带 verdict，
/// 也都不许被当成「算了不做了」的出口——真失败必须走 `failed` + `error`。
#[test]
fn t07b_process_ok_with_skip_reason_is_a_legit_noop() {
    let m = manifest(json!(["artifact"]));
    let outputs = m.effective_outputs();
    let row = parse_row(
        outputs,
        &json!({"assetId": 1, "result": "ok", "skipReason": "already_white"}),
    )
    .expect("ok + skipReason 是合法形态（处理类合法跳过）");
    assert_eq!(row.result, ResultStatus::Ok);
    assert_eq!(row.skip_reason.as_deref(), Some("already_white"));
    assert!(row.artifacts.is_empty(), "跳过 = 不产生新工作版本");
    // 但跳过不许夹带判定结论：处理类回 verdict 一律拒收（红线不因跳过而松）
    assert_eq!(
        parse_row(
            outputs,
            &json!({"assetId": 1, "result": "ok", "skipReason": "already_white",
                    "verdict": {"status": "pass"}}),
        )
        .unwrap_err(),
        RowViolation::VerdictNotAllowed
    );
}

#[test]
fn t07_failed_and_skipped_rows_need_reason_fields() {
    let m = manifest(json!(["artifact"]));
    let outputs = m.effective_outputs();
    assert_eq!(
        parse_row(outputs, &json!({"assetId": 1, "result": "failed"})).unwrap_err(),
        RowViolation::ErrorMissing
    );
    assert_eq!(
        parse_row(outputs, &json!({"assetId": 1, "result": "skipped"})).unwrap_err(),
        RowViolation::SkipReasonMissing
    );
    let failed = parse_row(
        outputs,
        &json!({"assetId": 1, "result": "failed",
                "error": {"code": "asset_unreadable", "message": "解码失败"}}),
    )
    .unwrap();
    assert_eq!(failed.error.unwrap().code, "asset_unreadable");
    let skipped = parse_row(
        outputs,
        &json!({"assetId": 1, "result": "skipped", "skipReason": "原图缺失"}),
    )
    .unwrap();
    assert_eq!(skipped.result, ResultStatus::Skipped);
    assert_eq!(skipped.skip_reason.as_deref(), Some("原图缺失"));
}
