//! 装载期 schema lint（C8）——「写在 YAML 里但引擎不读的键」必须在装载时响亮报出来。
//!
//! 背景（C7 房产包 spike，缺口 G5）：`serde` 默认**不** `deny_unknown_fields`，所以
//! 规则级拼错键（`evidance:`）、pipeline 自由文本键（`short_circuit:`）这类笔误
//! **装载通过、跑起来静默丢弃**——作者以为生效了，引擎从没看过它。本模块把
//! C7 探针里的 `schema_diff()` 搬进真实装载路径（`Pack::from_yaml_strs` 装载前先跑本 lint，
//! 有 issue 即整体 `Err`，不产出半包）。
//!
//! 设计要点：
//! - **合法键集合从 serde 结构体现derive**（构造样本实例 → 序列化 → 取 mapping 键），
//!   不手抄常量表：结构体加字段时白名单自动跟上，不会出现「schema 加了字段、lint 还判它非法」
//!   的假阳性。样本实例只在首次调用构造一次（`OnceLock`）。
//! - 本模块（L0）**只做 schema 层**的检查（未知键 / YAML 结构 / 已知取值面）。
//!   检测器名、插件名、params 键这些**注册表层**的校验在 L2
//!   （`svc_pipeline::compliance_lint`），因为注册表在 L1（engine-classic / engine-segment）。
//! - `taxonomy.yaml` 是**自由映射**（顶层 key 即词表字段名），不参与未知键检查，
//!   只校验「每个字段的值是字符串列表」。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;

use crate::packs::{
    Check, ConfidenceSource, ConfidenceSpec, Fixer, FixerTier, GroupBy, GroupSource,
    JudgementScope, LocalizedText, Pack, Pipeline, PipelineStep, ReportTemplate, Rule,
    RuleEvidence, RuleExecutor, RuleOnFail, StepKind,
};

/// 一条 lint 结果（面向 pack 作者的诊断：文件 + 键路径 + 已知取值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    /// 文件（`rules.yaml` / `pipeline.yaml` / `report.yaml` / `taxonomy.yaml`）
    pub file: &'static str,
    /// 键路径（`rules[0].check.params` / `pipeline.steps[2]`；文件级 = 空串）
    pub path: String,
    /// 机器可判的类别：`yaml_parse` / `unknown_key` / `bad_shape` / `rule_missing` /
    /// `plugin_empty`（注册表层的 `unknown_detector` / `unknown_param` / `unknown_plugin`
    /// 由 L2 lint 追加）
    pub code: &'static str,
    /// 人读消息（含已知取值）
    pub message: String,
    /// 归属规则 id（规则层/规则子层的 issue 才填；编排器据此做「规则级失败隔离」）
    pub rule: Option<String>,
}

impl std::fmt::Display for LintIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: [{}] {}", self.file, self.code, self.message)
        } else {
            write!(
                f,
                "{}: {}: [{}] {}",
                self.file, self.path, self.code, self.message
            )
        }
    }
}

/// 四个文件的合法键集合（derive 自 serde 结构体，见模块文档）。
#[derive(Debug, Clone, Default)]
pub struct KnownKeys {
    pub rule: BTreeSet<String>,
    pub check: BTreeSet<String>,
    pub fixer: BTreeSet<String>,
    pub step: BTreeSet<String>,
    pub pipeline: BTreeSet<String>,
    pub report: BTreeSet<String>,
}

/// 取一个可序列化值的 mapping 键集合（非 mapping → 空集）。
fn serde_keys<T: Serialize>(v: &T) -> BTreeSet<String> {
    match serde_yaml::to_value(v) {
        Ok(serde_yaml::Value::Mapping(m)) => m
            .keys()
            .filter_map(|k| k.as_str().map(|s| s.to_string()))
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// 合法键集合（首次构造一次；结构体加字段 → 白名单自动包含新键）。
pub fn known_keys() -> &'static KnownKeys {
    static KEYS: OnceLock<KnownKeys> = OnceLock::new();
    KEYS.get_or_init(|| KnownKeys {
        rule: serde_keys(&Rule {
            id: String::new(),
            name: LocalizedText::default(),
            executor: RuleExecutor::Classic,
            evidence: RuleEvidence::ImageOnly,
            check: Some(Check {
                detector: String::new(),
                params: serde_yaml::Value::Null,
                confidence: Some(ConfidenceSpec {
                    source: ConfidenceSource::Fixed,
                    value: Some(0.0),
                }),
            }),
            outputs: vec![String::new()],
            fixer: Some(Fixer {
                plugin: String::new(),
                tier: FixerTier::Guaranteed,
                params: serde_yaml::Value::Null,
            }),
            on_fail: RuleOnFail::Pass,
            // C9：作用域与组键也是**规则级**合法键（结构体加字段 → 白名单自动跟上）
            judgement_scope: JudgementScope::PerGroup,
            group_by: Some(GroupBy {
                source: GroupSource::Folder,
                field: None,
            }),
            // COMPLIANCE-DOMAIN：不合格业务原因（可选规则级键）
            fail_label: Some(String::new()),
        }),
        check: serde_keys(&Check {
            detector: String::new(),
            params: serde_yaml::Value::Null,
            confidence: Some(ConfidenceSpec {
                source: ConfidenceSource::Fixed,
                value: Some(0.0),
            }),
        }),
        fixer: serde_keys(&Fixer {
            plugin: String::new(),
            tier: FixerTier::Guaranteed,
            params: serde_yaml::Value::Null,
        }),
        step: serde_keys(&PipelineStep {
            kind: StepKind::Judge,
            id: None,
            rule: None,
            plugin: None,
            params: serde_yaml::Value::Null,
            depends_on: Vec::new(),
        }),
        pipeline: serde_keys(&Pipeline::default()),
        report: serde_keys(&ReportTemplate::default()),
    })
}

fn sorted(keys: &BTreeSet<String>) -> String {
    keys.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn unknown_key(
    file: &'static str,
    path: &str,
    key: &str,
    known: &BTreeSet<String>,
    rule: Option<&str>,
) -> LintIssue {
    LintIssue {
        file,
        path: path.to_string(),
        code: "unknown_key",
        message: format!("未知键 \"{key}\"（已知: {}）", sorted(known)),
        rule: rule.map(|r| r.to_string()),
    }
}

/// 逐层比对「YAML 写的键」与「schema 读的键」：多出来的键 = 会被静默吞掉的键。
fn check_unknown(
    file: &'static str,
    path: &str,
    node: &serde_yaml::Value,
    known: &BTreeSet<String>,
    rule: Option<&str>,
    out: &mut Vec<LintIssue>,
) {
    let serde_yaml::Value::Mapping(m) = node else {
        return;
    };
    for k in m.keys() {
        let Some(key) = k.as_str() else { continue };
        if !known.contains(key) {
            out.push(unknown_key(file, path, key, known, rule));
        }
    }
}

fn parse_file(
    file: &'static str,
    text: &str,
    out: &mut Vec<LintIssue>,
) -> Option<serde_yaml::Value> {
    match serde_yaml::from_str::<serde_yaml::Value>(text) {
        Ok(v) => Some(v),
        Err(e) => {
            out.push(LintIssue {
                file,
                path: String::new(),
                code: "yaml_parse",
                message: format!("YAML 解析失败: {e}"),
                rule: None,
            });
            None
        }
    }
}

/// 四文件文本 lint（装载前调用；不读取文件系统）。
///
/// 只报「作者写错但引擎静默吞掉」类问题；**不**重复 `Pack::validate()` 的语义校验
/// （规则唯一性 / judge 引用 / do 必填 id 等），那些装载期已经是硬错。
pub fn lint_texts(
    taxonomy_yaml: &str,
    rules_yaml: &str,
    pipeline_yaml: &str,
    report_yaml: &str,
) -> Vec<LintIssue> {
    let keys = known_keys();
    let mut out = Vec::new();

    // ---------- taxonomy.yaml：顶层 key 即词表字段（自由），值必须是字符串列表 ----------
    if let Some(v) = parse_file("taxonomy.yaml", taxonomy_yaml, &mut out) {
        match &v {
            serde_yaml::Value::Mapping(m) => {
                for (k, val) in m {
                    let Some(field) = k.as_str() else { continue };
                    let ok = matches!(val, serde_yaml::Value::Sequence(seq)
                        if seq.iter().all(|x| x.as_str().is_some()));
                    if !ok {
                        out.push(LintIssue {
                            file: "taxonomy.yaml",
                            path: format!("taxonomy.{field}"),
                            code: "bad_shape",
                            message: "词表字段的值必须是字符串列表（如 `scene: [白底图, 平铺]`）"
                                .into(),
                            rule: None,
                        });
                    }
                }
            }
            serde_yaml::Value::Null => {}
            other => out.push(LintIssue {
                file: "taxonomy.yaml",
                path: String::new(),
                code: "bad_shape",
                message: format!(
                    "词表文件必须是映射（字段名 → 候选值列表），实际是 {}",
                    kind_name(other)
                ),
                rule: None,
            }),
        }
    }

    // ---------- rules.yaml：逐规则 + check/fixer 子层 ----------
    if let Some(v) = parse_file("rules.yaml", rules_yaml, &mut out) {
        match &v {
            serde_yaml::Value::Sequence(rules) => {
                for (i, r) in rules.iter().enumerate() {
                    let path = format!("rules[{i}]");
                    let rid = r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if !matches!(r, serde_yaml::Value::Mapping(_)) {
                        out.push(LintIssue {
                            file: "rules.yaml",
                            path,
                            code: "bad_shape",
                            message: "规则必须是映射（id/executor/check/...）".into(),
                            rule: None,
                        });
                        continue;
                    }
                    check_unknown("rules.yaml", &path, r, &keys.rule, rid.as_deref(), &mut out);
                    if let Some(chk) = r.get("check") {
                        if matches!(chk, serde_yaml::Value::Mapping(_)) {
                            check_unknown(
                                "rules.yaml",
                                &format!("{path}.check"),
                                chk,
                                &keys.check,
                                rid.as_deref(),
                                &mut out,
                            );
                        }
                    }
                    if let Some(fx) = r.get("fixer") {
                        if matches!(fx, serde_yaml::Value::Mapping(_)) {
                            check_unknown(
                                "rules.yaml",
                                &format!("{path}.fixer"),
                                fx,
                                &keys.fixer,
                                rid.as_deref(),
                                &mut out,
                            );
                        }
                    }
                }
            }
            serde_yaml::Value::Null => {}
            other => out.push(LintIssue {
                file: "rules.yaml",
                path: String::new(),
                code: "bad_shape",
                message: format!("规则文件必须是列表，实际是 {}", kind_name(other)),
                rule: None,
            }),
        }
    }

    // ---------- pipeline.yaml：顶层 + 逐 step ----------
    if let Some(v) = parse_file("pipeline.yaml", pipeline_yaml, &mut out) {
        match &v {
            serde_yaml::Value::Mapping(_) => {
                check_unknown(
                    "pipeline.yaml",
                    "pipeline",
                    &v,
                    &keys.pipeline,
                    None,
                    &mut out,
                );
                if let Some(serde_yaml::Value::Sequence(steps)) = v.get("steps") {
                    for (i, s) in steps.iter().enumerate() {
                        let path = format!("pipeline.steps[{i}]");
                        if !matches!(s, serde_yaml::Value::Mapping(_)) {
                            out.push(LintIssue {
                                file: "pipeline.yaml",
                                path,
                                code: "bad_shape",
                                message: "步骤必须是映射（kind/rule/plugin/...）".into(),
                                rule: None,
                            });
                            continue;
                        }
                        check_unknown("pipeline.yaml", &path, s, &keys.step, None, &mut out);
                    }
                }
            }
            serde_yaml::Value::Null => {}
            other => out.push(LintIssue {
                file: "pipeline.yaml",
                path: String::new(),
                code: "bad_shape",
                message: format!(
                    "管道文件必须是映射（steps/recheck_max），实际是 {}",
                    kind_name(other)
                ),
                rule: None,
            }),
        }
    }

    // ---------- report.yaml：顶层 ----------
    if let Some(v) = parse_file("report.yaml", report_yaml, &mut out) {
        match &v {
            serde_yaml::Value::Mapping(_) => {
                check_unknown("report.yaml", "report", &v, &keys.report, None, &mut out)
            }
            serde_yaml::Value::Null => {}
            other => out.push(LintIssue {
                file: "report.yaml",
                path: String::new(),
                code: "bad_shape",
                message: format!(
                    "报告文件必须是映射（summary/detail_columns），实际是 {}",
                    kind_name(other)
                ),
                rule: None,
            }),
        }
    }

    out
}

/// 目录级 lint：读四文件文本（缺文件 → 一条 issue，不 panic）。
pub fn lint_dir(dir: &Path) -> Vec<LintIssue> {
    let mut texts = Vec::with_capacity(4);
    for name in crate::packs::PACK_FILES {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(t) => texts.push(t),
            Err(e) => {
                return vec![LintIssue {
                    file: Box::leak(name.to_string().into_boxed_str()),
                    path: String::new(),
                    code: "missing_file",
                    message: format!("四文件整包装载缺少 {name}: {e}"),
                    rule: None,
                }]
            }
        }
    }
    lint_texts(&texts[0], &texts[1], &texts[2], &texts[3])
}

/// 装载 lint 结果按规则 id 归并（key = rule id，value = 该规则的错误消息）。
///
/// 只收「规则自身写错」的 issue（`LintIssue.rule` 有值 = 路径挂在某条规则上）；
/// 文件级/步骤级 issue 不归到规则。
pub fn rule_issues_by_id(dir: &Path) -> BTreeMap<String, Vec<String>> {
    let Ok(rules_yaml) = std::fs::read_to_string(dir.join("rules.yaml")) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for issue in lint_texts("", &rules_yaml, "", "") {
        if let Some(id) = issue.rule.as_deref() {
            out.entry(id.to_string())
                .or_default()
                .push(issue.to_string());
        }
    }
    out
}

/// 编排器用的规则级错误表：rule id → 单行错误说明（多条用 `; ` 连接）。
///
/// 来源 = `Pack` 已解析后仍可判的注册表层错误（svc-pipeline 的 `compliance_lint` 会把自己
/// 的 issue 合并进来）；L0 这里只保证「结构层面」的规则级错误不漏。
pub fn rule_errors(pack: &Pack, extra: &[LintIssue]) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for issue in lint_pack(pack).iter().chain(extra.iter()) {
        if let Some(id) = issue.rule.as_deref() {
            out.entry(id.to_string())
                .and_modify(|m| {
                    m.push_str("; ");
                    m.push_str(&issue.message);
                })
                .or_insert_with(|| issue.message.clone());
        }
    }
    out
}

/// `Pack` 的 schema lint（L0 部分；注册表层在 L2 lint 里追加）。
pub fn lint_pack(pack: &Pack) -> Vec<LintIssue> {
    // 已解析的 Pack 无法再看到被 serde 吞掉的键 —— 所以要 lint 得回到原文。
    // 这里只做「解析后仍可判」的部分（外部调用方要完整检查请用 lint_dir/lint_texts）。
    let mut out = Vec::new();
    if pack.pipeline.steps.is_empty() {
        out.push(LintIssue {
            file: "pipeline.yaml",
            path: "pipeline.steps".into(),
            code: "bad_shape",
            message: "管道没有任何步骤（整包无判定可执行）".into(),
            rule: None,
        });
    }
    for (i, s) in pack.pipeline.steps.iter().enumerate() {
        if s.kind == StepKind::Judge && s.rule.as_deref().unwrap_or("").trim().is_empty() {
            out.push(LintIssue {
                file: "pipeline.yaml",
                path: format!("pipeline.steps[{i}]"),
                code: "rule_missing",
                message: "judge 步骤必须写 rule（规则 id）".into(),
                rule: None,
            });
        }
        if s.kind == StepKind::Do && s.plugin.as_deref().unwrap_or("").trim().is_empty() {
            out.push(LintIssue {
                file: "pipeline.yaml",
                path: format!("pipeline.steps[{i}]"),
                code: "plugin_empty",
                message: "do 步骤必须写 plugin（插件注册表键）".into(),
                rule: None,
            });
        }
    }
    // 规则级：tags/check 二选一（`Pack::validate()` 覆盖，这里补齐「装载前单独 lint」口径）
    for (i, r) in pack.rules.iter().enumerate() {
        if r.check.is_none() && r.outputs.is_empty() {
            out.push(LintIssue {
                file: "rules.yaml",
                path: format!("rules[{i}]"),
                code: "rule_missing",
                message: "规则既无 check（判定型）又无 outputs（打标型）".into(),
                rule: Some(r.id.clone()),
            });
        }
        if r.is_tagging() && r.check.is_some() {
            out.push(LintIssue {
                file: "rules.yaml",
                path: format!("rules[{i}]"),
                code: "bad_shape",
                message: "打标型规则（outputs 非空）不应再写 check（词表由 outputs 驱动）".into(),
                rule: Some(r.id.clone()),
            });
        }
    }
    out
}

fn kind_name(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "list",
        serde_yaml::Value::Mapping(_) => "map",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::Pack;

    #[test]
    fn known_keys_come_from_serde_structs() {
        let k = known_keys();
        assert!(k.rule.contains("on_fail") && k.rule.contains("check") && k.rule.contains("fixer"));
        assert_eq!(
            k.check.iter().cloned().collect::<Vec<_>>(),
            vec!["confidence", "detector", "params"]
        );
        assert!(k.fixer.contains("type") && k.fixer.contains("tier") && k.fixer.contains("params"));
        assert!(k.step.contains("depends_on") && k.step.contains("plugin"));
        assert!(k.pipeline.contains("steps") && k.pipeline.contains("recheck_max"));
        assert!(k.report.contains("summary") && k.report.contains("detail_columns"));
    }

    /// IP-K16：真行业元数据的 schema 干净性由**插件包**保证（`resources/rules.json`），
    /// 旧的 `packs/<行业>/` 四件 YAML 已删除 —— 这里只保留「lint 无假阳性」的最小内联样例。
    #[test]
    fn clean_pack_texts_have_no_issues() {
        let rules = "- id: R1_size\n  executor: classic\n  evidence: image_only\n  \
                     check: {detector: sharpness, params: {min: 1}}\n  on_fail: fail\n";
        let issues = lint_texts(
            "scene: [客厅, 卧室]\n",
            rules,
            "steps: [{kind: judge, rule: R1_size}]\nrecheck_max: 1\n",
            "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1_size]\n",
        );
        assert!(issues.is_empty(), "schema 层不许有假阳性: {issues:?}");
    }

    #[test]
    fn typo_keys_are_reported_with_file_and_path() {
        let rules = "- id: R1\n  executor: classic\n  evidance: image_only\n  check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n";
        let issues = lint_texts("", rules, "steps: []\n", "summary: []\n");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].file, "rules.yaml");
        assert_eq!(issues[0].path, "rules[0]");
        assert_eq!(issues[0].code, "unknown_key");
        assert!(issues[0].message.contains("evidance"), "{}", issues[0]);
        assert!(
            issues[0].message.contains("on_fail"),
            "已知取值要列出来: {}",
            issues[0]
        );
    }

    #[test]
    fn free_text_pipeline_key_is_reported() {
        let pipeline = "steps:\n  - {kind: judge, rule: R1, wibble: 1}\nshort_circuit: 自由文本\n";
        let issues = lint_texts("", "", pipeline, "");
        let codes: Vec<&str> = issues.iter().map(|i| i.code).collect();
        assert!(codes.iter().all(|c| *c == "unknown_key"), "{issues:?}");
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"pipeline"));
        assert!(paths.contains(&"pipeline.steps[0]"));
    }

    #[test]
    fn pack_load_rejects_unknown_keys() {
        // 装载路径真的硬拒（不是只有 lint 函数会报）
        let rules = "- id: R1\n  executor: classic\n  evidance: image_only\n  check: {detector: sharpness, params: {min: 1}}\n  on_fail: pass\n";
        let err = Pack::from_yaml_strs("probe", "", rules, "steps: []\n", "").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("evidance"), "{msg}");
        assert!(msg.contains("rules.yaml"), "{msg}");
    }

    #[test]
    fn taxonomy_bad_shape_is_reported() {
        let issues = lint_texts("scene: 白底图\n", "", "", "");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].file, "taxonomy.yaml");
        assert!(issues[0].path.contains("scene"));
    }
}
