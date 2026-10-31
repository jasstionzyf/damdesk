//! 装载期 lint（L2 部分，C8）：**注册表层**校验 —— 检测器 / 参数键 / 插件 id。
//!
//! 与 L0（`core_config::lint`）的分工：
//! - L0 管「YAML 里写的键 vs schema 读的键」（未知键、结构不符）——纯 schema，不需注册表；
//! - 本模块管「名字 vs 注册表」：`check.detector` 是否在对应执行档的注册表里、`params` 键是否
//!   合法、`fixer.type` / do 步骤的 `plugin` 是否存在。注册表都在 L1（engine-classic /
//!   engine-segment / 本层 vl 与插件注册表），所以这一半只能放在 L2。
//!
//! 为什么需要它（C7 缺口 G5）：`RuleExecutor::Vl` 原先**不查任何注册表**，`detector: vl_set_judge`
//! 这类不存在的键装载通过、跑起来不报错；classic 通道虽会在**运行时**报「未知检测器」，但代价是
//! 整张图的全部判定一起丢（G4）。装载期 lint 把这两类笔误提前到「装载/激活/导入」那一刻。
//!
//! 三个出口（同一份 issue 列表）：
//! 1. [`lint_dir`] / [`lint_pack`] —— 「装载前 lint」命令行与界面预检（不落库、不激活）；
//! 2. [`require_clean`] —— app-web 导入/激活门禁（有 issue 即拒，400 + 逐条消息）；
//! 3. [`rule_errors`] —— 编排器的**规则级失败隔离**输入（装载期已知坏规则 → 该规则不执行、
//!    落 `undetermined` + `reason=load_error` 证据；同图其余规则判定照落）。

use std::collections::BTreeMap;
use std::path::Path;

use core_config::lint::LintIssue;
use core_config::{Pack, RuleExecutor, StepKind};

use crate::compliance_group;
use crate::compliance_plugins;
use crate::compliance_vl;

/// 插件注册表已知 id（**全量清单**：见 `compliance_plugins::ALL_PLUGIN_IDS` 的说明 ——
/// web 容器没编的插件是部署态能力缺失，不是配置错误，lint 不许把它判成未知插件）。
fn plugin_known_ids() -> String {
    compliance_plugins::known_ids_all()
}

fn plugin_known(id: &str) -> bool {
    compliance_plugins::plugin_id_known(id)
}

fn issue(
    file: &'static str,
    path: String,
    code: &'static str,
    message: String,
    rule: Option<&str>,
) -> LintIssue {
    LintIssue {
        file,
        path,
        code,
        message,
        rule: rule.map(|r| r.to_string()),
    }
}

/// 注册表层 lint（已解析的 `Pack` 上做；L0 的键级 lint 见 `core_config::lint`）。
///
/// C9：校验**按作用域分流** —— per_asset 规则查逐图注册表（engine-classic / vl_judge），
/// per_group 规则查组级注册表（`compliance_group` 的 classic 组级检测器 / vl_group_judge）。
/// 作用域与检测器不匹配（如 per_group 规则用 `sharpness`、per_asset 规则用 `phash_dup`）
/// 单列一条 `unknown_detector` 并**说明该检测器服务于哪个作用域**——这正是 C7 缺口
/// 「vl 通道不校验 / 写错的键静默丢弃」在组级形态下的延伸。
pub fn lint_pack(pack: &Pack) -> Vec<LintIssue> {
    let mut out = Vec::new();

    for (i, rule) in pack.rules.iter().enumerate() {
        let rid = rule.id.as_str();
        let group_scoped = rule.is_group_scoped();
        // 打标型规则不查 check（词表由 outputs 驱动；「打标型不该写 check」由 L0 报）
        if !rule.is_tagging() {
            if let Some(check) = &rule.check {
                let base = format!("rules[{i}].check");
                match rule.executor {
                    RuleExecutor::Classic => {
                        if group_scoped {
                            // 组级 classic 检测器（L2 注册表：phash_dup 等）
                            if compliance_group::lookup(&check.detector).is_none() {
                                let msg = if engine_classic::registry::lookup(&check.detector).is_some() {
                                    format!(
                                        "规则 {rid}: 检测器 \"{}\" 是逐图检测器（per_asset 专用）—— \
                                         组级规则需用组级检测器（已知组级: {}）",
                                        check.detector,
                                        compliance_group::known_ids()
                                    )
                                } else {
                                    format!(
                                        "规则 {rid}: 未知组级检测器 \"{}\"（已知组级: {}）",
                                        check.detector,
                                        compliance_group::known_ids()
                                    )
                                };
                                out.push(issue("rules.yaml", base.clone(), "unknown_detector", msg, Some(rid)));
                            } else if let Some(msg) =
                                compliance_group::params_error(&check.detector, &check.params)
                            {
                                out.push(issue(
                                    "rules.yaml",
                                    base.clone(),
                                    "unknown_param",
                                    format!("规则 {rid}（{}）: {msg}", check.detector),
                                    Some(rid),
                                ));
                            }
                        } else if engine_classic::registry::lookup(&check.detector).is_none() {
                            let msg = if compliance_group::lookup(&check.detector).is_some() {
                                format!(
                                    "规则 {rid}: 检测器 \"{}\" 是组级检测器（judgement_scope: per_group \
                                     专用）—— 逐图规则不能用它（组级检测器已知: {}）",
                                    check.detector,
                                    compliance_group::known_ids()
                                )
                            } else {
                                format!(
                                    "规则 {rid}: 未知检测器 \"{}\"（classic 已知: {}）",
                                    check.detector,
                                    engine_classic::registry::known_ids()
                                )
                            };
                            out.push(issue("rules.yaml", base.clone(), "unknown_detector", msg, Some(rid)));
                        } else if let Some(msg) =
                            engine_classic::registry::params_error(&check.detector, &check.params)
                        {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unknown_param",
                                format!("规则 {rid}（{}）: {msg}", check.detector),
                                Some(rid),
                            ));
                        }
                    }
                    // local-infer 启用：L1 注册表已编译进来 → 校验检测器/参数 + 作用域
                    #[cfg(feature = "local-infer")]
                    RuleExecutor::LocalModel => {
                        if group_scoped {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unsupported_scope",
                                format!(
                                    "规则 {rid}: judgement_scope per_group 不支持 local_model 执行档 \
                                     （v1 组级形态只有 classic 组级检测器与 vl_group_judge）"
                                ),
                                Some(rid),
                            ));
                        } else if engine_segment::registry::lookup(&check.detector).is_none() {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unknown_detector",
                                format!(
                                    "规则 {rid}: 未知检测器 \"{}\"（local_model 已知: {}）",
                                    check.detector,
                                    engine_segment::registry::known_ids()
                                ),
                                Some(rid),
                            ));
                        } else if let Some(msg) = engine_segment::registry::params_error(
                            &check.detector,
                            &check.params,
                        ) {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unknown_param",
                                format!("规则 {rid}（{}）: {msg}", check.detector),
                                Some(rid),
                            ));
                        }
                    }
                    // local-infer 未启用（web 容器）：L1 注册表未编译进来，无从校验检测器/参数
                    // → 只保留作用域校验（不假报未知检测器）
                    #[cfg(not(feature = "local-infer"))]
                    RuleExecutor::LocalModel => {
                        if group_scoped {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unsupported_scope",
                                format!(
                                    "规则 {rid}: judgement_scope per_group 不支持 local_model 执行档 \
                                     （v1 组级形态只有 classic 组级检测器与 vl_group_judge）"
                                ),
                                Some(rid),
                            ));
                        }
                    }
                    RuleExecutor::Vl => {
                        let entry = compliance_vl::vl_lookup(&check.detector);
                        match entry {
                            None => {
                                out.push(issue(
                                    "rules.yaml",
                                    base.clone(),
                                    "unknown_detector",
                                    format!(
                                        "规则 {rid}: 未知 vl 检测器 \"{}\"（vl 已知: {}）",
                                        check.detector,
                                        compliance_vl::vl_known_ids()
                                    ),
                                    Some(rid),
                                ));
                            }
                            Some(e) if e.scope != rule.judgement_scope => {
                                out.push(issue(
                                    "rules.yaml",
                                    base.clone(),
                                    "unknown_detector",
                                    format!(
                                        "规则 {rid}: vl 检测器 \"{}\" 只服务 {:?} 作用域的规则（本规则是 {:?}）",
                                        check.detector, e.scope, rule.judgement_scope
                                    ),
                                    Some(rid),
                                ));
                            }
                            Some(_) => {}
                        }
                        // params 独立校验（作用域决定 schema）：检测器名与 params 键
                        // 同时写错时两条都要能看见（C7 探针 05 就是这个场景）
                        let params_err = if group_scoped {
                            compliance_vl::vl_group_params(&check.params).err()
                        } else {
                            compliance_vl::vl_judge_params(&check.params).err()
                        };
                        if let Some(msg) = params_err {
                            out.push(issue(
                                "rules.yaml",
                                base.clone(),
                                "unknown_param",
                                format!("规则 {rid}（{}）: {msg}", check.detector),
                                Some(rid),
                            ));
                        }
                    }
                }
            }
        }
        // ---- COMPLIANCE-TRISTATE 装载期门禁（2026-10-17 裁决）----
        // 1) 判定型规则必须声明置信度来源（三态口径要求每行带 confidence，不许默认 1.0）；
        // 2) `source: detector_margin` 只有注册表声明支持余量的检测器可用；
        // 3) `source: fixed` 必须给出 0..1 的值；
        // 4) **无 fixer 的规则禁止 retry_then_human** —— 没有修复器可重试，「转人工」以前就是
        //    落 pending_human 的暗道（房产包 fixer: null + retry_then_human → 27 张里不合格 0）。
        if !rule.is_tagging() {
            if let Some(check) = &rule.check {
                let conf_base = format!("rules[{i}].check.confidence");
                let margin_capable = engine_classic::registry::supports_margin(&check.detector);
                match check.confidence.as_ref() {
                    // 检测器本身能给判定余量（classic 四件套）→ 不强制声明，运行时自动派生；
                    // 其余（vl / 组级 / 分割）必须声明 fixed 值，**不许默认 1.0**
                    None if margin_capable => {}
                    None => out.push(issue(
                        "rules.yaml",
                        conf_base.clone(),
                        "missing_confidence",
                        format!(
                            "规则 {rid}: 检测器 \"{}\" 不提供判定余量，必须声明 check.confidence \
                             （source: fixed + value ∈ [0,1]）—— 三态口径要求每条规则结果带置信度，不许默认 1.0",
                            check.detector
                        ),
                        Some(rid),
                    )),
                    Some(spec) => match spec.source {
                        core_config::ConfidenceSource::DetectorMargin => {
                            if spec.value.is_some() {
                                out.push(issue(
                                    "rules.yaml",
                                    conf_base.clone(),
                                    "bad_confidence",
                                    format!("规则 {rid}: source=detector_margin 不携带 value（余量由检测器算）"),
                                    Some(rid),
                                ));
                            }
                            if !engine_classic::registry::supports_margin(&check.detector) {
                                out.push(issue(
                                    "rules.yaml",
                                    conf_base.clone(),
                                    "unsupported_confidence",
                                    format!(
                                        "规则 {rid}: 检测器 \"{}\" 不提供判定余量（支持 margin 的: size_min/white_edge/sharpness/exposure）—— \
                                         改用 source: fixed + value",
                                        check.detector
                                    ),
                                    Some(rid),
                                ));
                            }
                        }
                        core_config::ConfidenceSource::Fixed => match spec.value {
                            Some(v) if v.is_finite() && (0.0..=1.0).contains(&v) => {}
                            _ => out.push(issue(
                                "rules.yaml",
                                conf_base.clone(),
                                "bad_confidence",
                                format!("规则 {rid}: source=fixed 必须给 value ∈ [0,1]"),
                                Some(rid),
                            )),
                        },
                        // COMPLIANCE-DOMAIN：VL 自评置信度 —— 只有「逐图 vl judge 通道」能给；
                        // 拿不到就落 undetermined（引擎侧），这里只校验声明本身站得住。
                        core_config::ConfidenceSource::VlSelfReport => {
                            if spec.value.is_some() {
                                out.push(issue(
                                    "rules.yaml",
                                    conf_base.clone(),
                                    "bad_confidence",
                                    format!(
                                        "规则 {rid}: source=vl_self_report 不携带 value（置信度由 VL 逐图自评回，pack 常数会覆盖真实把握度）"
                                    ),
                                    Some(rid),
                                ));
                            }
                            let per_asset_vl_judge = rule.executor == RuleExecutor::Vl
                                && !rule.is_group_scoped()
                                && compliance_vl::vl_lookup(&check.detector).map(|e| e.scope)
                                    == Some(core_config::JudgementScope::PerAsset);
                            if !per_asset_vl_judge {
                                out.push(issue(
                                    "rules.yaml",
                                    conf_base.clone(),
                                    "unsupported_confidence",
                                    format!(
                                        "规则 {rid}: source=vl_self_report 只支持「逐图 vl judge 检测器」（executor: vl + 非组级 + 检测器在 vl_judge 词表里）—— \
                                         当前 executor={:?} detector=\"{}\"（组级/经典检测器请用 source: fixed 或 detector_margin）",
                                        rule.executor, check.detector
                                    ),
                                    Some(rid),
                                ));
                            }
                        }
                    },
                }
            }
        }
        if rule.fixer.is_none() && rule.on_fail == core_config::RuleOnFail::RetryThenHuman {
            out.push(issue(
                "rules.yaml",
                format!("rules[{i}].on_fail"),
                "retry_without_fixer",
                format!(
                    "规则 {rid}: on_fail=retry_then_human 但 fixer 为空 —— 没有修复器可重试，\
                     判定结果只能落 fail（要交人工请用低置信度复核，不要靠 retry_then_human）"
                ),
                Some(rid),
            ));
        }
        if let Some(fx) = &rule.fixer {
            if !plugin_known(&fx.plugin) {
                out.push(issue(
                    "rules.yaml",
                    format!("rules[{i}].fixer"),
                    "unknown_plugin",
                    format!(
                        "规则 {rid}: 未知修复插件 \"{}\"（已知: {}）",
                        fx.plugin,
                        plugin_known_ids()
                    ),
                    Some(rid),
                ));
            }
        }
    }

    for (i, step) in pack.pipeline.steps.iter().enumerate() {
        if step.kind != StepKind::Do {
            continue;
        }
        if let Some(pid) = step.plugin.as_deref().filter(|p| !p.trim().is_empty()) {
            if !plugin_known(pid) {
                let sid = step.id.clone().unwrap_or_else(|| format!("#{i}"));
                out.push(issue(
                    "pipeline.yaml",
                    format!("pipeline.steps[{i}].plugin"),
                    "unknown_plugin",
                    format!("do 步骤 {sid}: 未知插件 \"{pid}\"（已知: {}）", plugin_known_ids()),
                    None,
                ));
            }
        }
    }

    out
}

/// 规则级错误表（rule id → 单行说明）：**装载期已知坏规则** → 编排器隔离执行。
pub fn rule_errors(pack: &Pack) -> BTreeMap<String, String> {
    core_config::lint::rule_errors(pack, &lint_pack(pack))
}

/// 目录级全量 lint（L0 键级 + L2 注册表层），不产出 `Pack`。
///
/// 「装载前 lint」用：即使 `Pack::from_dir` 会被 L0 拒（未知键），这里也能把**全部**问题
/// 一次列清（L0 issue 与 L2 issue 都拿到）。
pub fn lint_dir(dir: &Path) -> Vec<LintIssue> {
    let mut out = core_config::lint::lint_dir(dir);
    // L0 有 issue 时 from_dir 会拒（这是设计），但注册表层仍要能报 —— 直接读四文件文本解析
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
    match (
        read("taxonomy.yaml"),
        read("rules.yaml"),
        read("pipeline.yaml"),
        read("report.yaml"),
    ) {
        (Some(t), Some(r), Some(p), Some(rep)) => {
            let id = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match Pack::from_yaml_strs(&id, &t, &r, &p, &rep) {
                Ok(pack) => out.extend(lint_pack(&pack)),
                Err(e) => {
                    // 解析/校验失败：不重复 L0 已报的键问题，只补一条装载错误（含原因）
                    let msg = e.to_string();
                    if !out.iter().any(|i| msg.contains(&i.message)) {
                        out.push(issue(
                            "pack",
                            String::new(),
                            "load_error",
                            msg,
                            None,
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// 门禁：有 issue 即拒（app-web 导入/激活、CLI lint 退出码用）。返回逐条可读消息。
pub fn require_clean(pack: &Pack) -> Result<(), Vec<String>> {
    let issues = lint_pack(pack);
    if issues.is_empty() {
        return Ok(());
    }
    Err(issues.iter().map(|i| i.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn c7_defect_shapes_are_still_caught_by_the_lint() {
        // 「C7 五类写错」在 C9 之后仍必须响亮：作用域错配 / 未知检测器 / 未知参数键
        let rules = r#"
- id: X1_group_detector_on_per_asset
  executor: classic
  check: {detector: phash_dup, params: {threshold: 8}}
  on_fail: pass
- id: X2_per_asset_detector_on_group
  executor: classic
  judgement_scope: per_group
  group_by: {source: folder}
  check: {detector: sharpness, params: {min: 100}}
  on_fail: pass
- id: X3_unknown_vl_detector
  executor: vl
  check: {detector: vl_set_judge, params: {prompt: p, expect: 否}}
  on_fail: pass
- id: X4_group_by_inside_params
  executor: vl
  judgement_scope: per_group
  group_by: {source: folder}
  check:
    detector: vl_group_judge
    params: {prompt: p, options: [客厅], require_all: [客厅], group_by: 房源号}
  on_fail: pass
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: X1_group_detector_on_per_asset}\n  \
                        - {kind: judge, rule: X2_per_asset_detector_on_group}\n  \
                        - {kind: judge, rule: X3_unknown_vl_detector}\n  \
                        - {kind: judge, rule: X4_group_by_inside_params}\n";
        let pack = Pack::from_yaml_strs("x", "", rules, pipeline, "").unwrap();
        let issues = lint_pack(&pack);
        let joined = issues.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" | ");
        assert!(joined.contains("phash_dup") && joined.contains("per_group"), "{joined}");
        assert!(joined.contains("sharpness") && joined.contains("逐图检测器"), "{joined}");
        assert!(joined.contains("vl_set_judge"), "{joined}");
        assert!(joined.contains("group_by"), "{joined}");
        // 每条都挂到对应规则（规则级隔离的依据）
        let ruled: Vec<&str> = issues.iter().filter_map(|i| i.rule.as_deref()).collect();
        for want in [
            "X1_group_detector_on_per_asset",
            "X2_per_asset_detector_on_group",
            "X3_unknown_vl_detector",
            "X4_group_by_inside_params",
        ] {
            assert!(ruled.contains(&want), "{want} 未被报：{joined}");
        }
    }

    /// 干净包必须零 issue（门禁不许有假阳性把好包拒了）。
    ///
    /// IP-K16：真行业元数据已住进处理插件的 `resources/rules.json`（宿主不再内置 YAML 包），
    /// 这里用一份**内联**的合法 YAML 包（基线参考实现的装载形态）当样例。
    #[test]
    fn clean_pack_lints_clean() {
        let pack = Pack::from_yaml_strs(
            "clean",
            "scene: [客厅]\n",
            r#"
- id: R1_size
  name: 尺寸下限
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: {type: upscale, tier: best_effort}
  on_fail: retry_then_human
"#,
            "steps:\n  - {kind: judge, rule: R1_size}\n",
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size]\n",
        )
        .unwrap();
        let issues = lint_pack(&pack);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(require_clean(&pack).is_ok());
    }

    // ---------- COMPLIANCE-DOMAIN：领域 gate 的装载期门禁 ----------

    #[test]
    fn domain_gate_confidence_declarations_are_gated() {
        // ① vl_self_report 用在经典检测器上 → unsupported_confidence
        // ② vl_self_report 带 value → bad_confidence
        // ③ 组级 vl 规则声明 vl_self_report → unsupported_confidence（只有逐图通道能给自评）
        let rules = r#"
- id: X1_classic_self_report
  executor: classic
  check:
    detector: sharpness
    params: {min: 100}
    confidence: {source: vl_self_report}
  on_fail: fail
- id: X2_self_report_with_value
  executor: vl
  check:
    detector: vl_judge
    params: {prompt: p, expect: 是}
    confidence: {source: vl_self_report, value: 0.9}
  on_fail: fail
- id: X3_group_self_report
  executor: vl
  judgement_scope: per_group
  group_by: {source: folder}
  check:
    detector: vl_group_judge
    params: {prompt: p, options: [客厅], require_all: [客厅]}
    confidence: {source: vl_self_report}
  on_fail: fail
"#;
        let pipeline = "steps:\n  - {kind: judge, rule: X1_classic_self_report}\n  \
                        - {kind: judge, rule: X2_self_report_with_value}\n  \
                        - {kind: judge, rule: X3_group_self_report}\n";
        let pack = Pack::from_yaml_strs("x", "", rules, pipeline, "").unwrap();
        let issues = lint_pack(&pack);
        let joined = issues.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" | ");
        assert!(
            joined.contains("unsupported_confidence") && joined.contains("X1_classic_self_report"),
            "{joined}"
        );
        assert!(
            joined.contains("bad_confidence") && joined.contains("X2_self_report_with_value"),
            "{joined}"
        );
        assert!(
            joined.contains("unsupported_confidence") && joined.contains("X3_group_self_report"),
            "{joined}"
        );
        assert!(
            require_clean(&pack).is_err(),
            "门禁必须拒收（导入/激活 400）"
        );
    }

    #[test]
    fn domain_gate_without_confidence_is_rejected() {
        // 父卡口径：VL 检测器不提供余量，**不声明 confidence 就装载期硬拒**（不许默认 1.0）
        let rules = r#"
- id: F0_no_conf
  executor: vl
  check: {detector: vl_judge, params: {prompt: "是不是房源图", expect: 是}}
  fixer: null
  on_fail: fail
"#;
        let pack = Pack::from_yaml_strs("x", "", rules, "steps:\n  - {kind: judge, rule: F0_no_conf}\n", "").unwrap();
        let issues = lint_pack(&pack);
        let joined = issues.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" | ");
        assert!(joined.contains("missing_confidence"), "{joined}");
        assert!(joined.contains("F0_no_conf"), "{joined}");
    }

    #[test]
    fn domain_gate_wrong_detector_is_rejected_at_load() {
        // 领域 gate 写成不存在的检测器（笔误）→ 装载期硬拒，不是「跑起来才发现」
        let rules = r#"
- id: F0_domain
  executor: vl
  check:
    detector: vl_domain_gate
    params: {prompt: p, expect: 是}
    confidence: {source: vl_self_report}
  fixer: null
  on_fail: fail
"#;
        let pack = Pack::from_yaml_strs(
            "x",
            "",
            rules,
            "steps:\n  - {kind: judge, rule: F0_domain}\n",
            "",
        )
        .unwrap();
        let issues = lint_pack(&pack);
        let joined = issues.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" | ");
        assert!(joined.contains("unknown_detector"), "{joined}");
        assert!(joined.contains("vl_domain_gate"), "{joined}");
        // 规则级隔离依据
        assert!(rule_errors(&pack).contains_key("F0_domain"));
    }

    #[test]
    fn classic_param_typo_and_unknown_plugin_are_caught() {
        let rules = r#"
- id: R1
  executor: classic
  check: {detector: sharpness, params: {min_var: 100}}
  fixer: {type: no_such_fixer, tier: guaranteed}
  on_fail: auto_fix
"#;
        let pack = Pack::from_yaml_strs(
            "t",
            "",
            rules,
            "steps:\n  - {kind: judge, rule: R1}\n  - {kind: do, id: x, plugin: no_such_plugin}\n",
            "",
        )
        .unwrap();
        let issues = lint_pack(&pack);
        let joined = issues.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" | ");
        assert!(joined.contains("min_var"), "{joined}");
        assert!(joined.contains("no_such_fixer"), "{joined}");
        assert!(joined.contains("no_such_plugin"), "{joined}");
        // 规则级错误表：两条都挂 R1（do 步骤的插件错挂步骤，不挂规则）
        let errs = rule_errors(&pack);
        assert_eq!(errs.len(), 1);
        assert!(errs["R1"].contains("min_var"));
    }

    #[test]
    fn probe_fixtures_are_rejected_at_load() {
        // 5 个故意写错的探针包：装载前 lint 必须全部报错（键级 + 注册表层一起看）
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/c7-fixtures/schema-probes");
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 5, "探针夹具应有 5 个");
        for d in entries {
            let name = d.file_name().unwrap().to_string_lossy().to_string();
            let issues = lint_dir(&d);
            assert!(!issues.is_empty(), "{name} 应被 lint 拒绝");
        }
    }
}
