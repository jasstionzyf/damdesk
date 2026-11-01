//! C9 组级判定（按组判定，卡 t_ff666f9c）—— **L2 组级检测器注册表** + 组键解析。
//!
//! 为什么注册表在 L2（与 vl 通道同口径，见 `compliance_vl` 的注释）：组级检测器需要
//! **跨 L1 组合**（`phash_dup` = engine-dedup 的分组算法 + core-hash 的汉明距离），
//! 而 L1 之间禁止互引（`scripts/check_layers.sh`）—— L2 编排器是唯一合法的分发层。
//!
//! 分层：
//! - 逐图（per_asset）classic 检测器 → L1 `engine-classic::registry`（原样不动）；
//! - 组级（per_group）classic 检测器 → 本模块 `GROUP_DETECTORS`（新，C9 起）；
//! - 逐图 vl → L1/L2 `compliance_vl::vl_judge`；组级 vl → `compliance_vl::vl_group_judge`。
//!
//! 组级检测器的输入是**组**（成员资产列表 + 每成员的派生数据），不是单张图：
//! 这正是 C7 缺口 G1 的核心（「这套房源齐不齐 / 同组有几张重复房照」逐图判不了）。
//! 纯函数零 IO：文件/pHash 的取数由编排器（`compliance_queue`）在调用前完成，
//! 便于「组内成员不可判 → pending_human」这类降级判定只写一处。

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// 组成员输入（编排器组装：资产事实 + 去重所需派生数据）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMemberInput {
    pub asset_id: i64,
    /// SHA-256（assets.fileHash；缺失 = None，不参与精确边）
    pub file_hash: Option<String>,
    /// 64bit pHash（resource.pHash；缺失可由编排器现算，见 `phash_source`）
    pub phash: Option<u64>,
    /// pHash 来源（证据用）：`db` | `computed` | `none`
    pub phash_source: &'static str,
    /// fileHash 来源：`db` | `computed` | `none`
    pub file_hash_source: &'static str,
}

impl GroupMemberInput {
    /// 该成员是否有任何可用于重复检测的派生数据（都为 None → 不可判，编排器降级）。
    pub fn has_dup_data(&self) -> bool {
        self.phash.is_some() || self.file_hash.is_some()
    }
}

/// 组级检测器输入：一个组（`group_key = None` = 缺键组，见下）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupInput {
    /// 组键取值；None = 组键缺失（成员各自取不到组键时凑成的「缺键组」，判定直接降级）
    pub group_key: Option<String>,
    /// 组成员（assetId 升序确定；编排器保证非空）
    pub members: Vec<GroupMemberInput>,
}

/// 组级检测输出：pass/score/metrics + **组级证据**（成员列表只能在这里表达）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupDetectorOutput {
    pub pass: bool,
    /// 主分数（语义随检测器：phash_dup = 重复组个数）
    pub score: f64,
    pub metrics: BTreeMap<String, f64>,
    /// 组级证据（重复组 + 成员清单等；落库进 `rule_verdict.evidence`）
    pub evidence: serde_json::Value,
}

/// 组级检测器注册项。
pub struct GroupDetectorEntry {
    pub id: &'static str,
    pub name: &'static str,
    /// (组输入, 参数) → 组输出
    pub run: fn(&GroupInput, &serde_yaml::Value) -> std::result::Result<GroupDetectorOutput, String>,
}

/// 组级（classic）检测器静态注册表（编译进二进制）。
pub static GROUP_DETECTORS: &[GroupDetectorEntry] = &[GroupDetectorEntry {
    id: "phash_dup",
    name: "重复检测（SHA 精确 ∪ pHash 近似，组内连通分量）",
    run: run_phash_dup,
}];

/// 查表。
pub fn lookup(id: &str) -> Option<&'static GroupDetectorEntry> {
    GROUP_DETECTORS.iter().find(|e| e.id == id)
}

/// 已知组级检测器 id 列表（错误信息用）。
pub fn known_ids() -> String {
    GROUP_DETECTORS
        .iter()
        .map(|e| e.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// 顶层分发：未知 id 显式报错（不静默跳过；与 classic 逐图通道同口径）。
pub fn run(
    id: &str,
    input: &GroupInput,
    params: &serde_yaml::Value,
) -> std::result::Result<GroupDetectorOutput, String> {
    let entry = lookup(id).ok_or_else(|| {
        format!(
            "未知组级检测器: {id}（已知组级: {}；逐图检测器不能用于 per_group 规则）",
            known_ids()
        )
    })?;
    (entry.run)(input, params)
}

/// 参数校验（装载期 lint 用；与运行期解析**同一个 struct**，白名单零漂移）。
pub fn params_error(id: &str, params: &serde_yaml::Value) -> Option<String> {
    if id != "phash_dup" {
        return None;
    }
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    serde_yaml::from_value::<PhashDupParams>(normalized)
        .err()
        .map(|e| e.to_string())
}

/// `phash_dup` 参数（C9）：`threshold` = pHash 汉明距离阈值（缺省 engine-dedup 的 8）。
/// `deny_unknown_fields`：写错键装载即拒（C7 的 `params.group: within_batch` 就是被静默忽略的键）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhashDupParams {
    #[serde(default = "default_threshold")]
    pub threshold: u32,
}

fn default_threshold() -> u32 {
    engine_dedup::PHASH_THRESHOLD
}

/// 重复检测（组级）：组内 SHA 精确重复 ∪ pHash 近似重复（汉明 ≤ threshold）连通分量。
///
/// 语义（房产包 F3「重复房照」）：`pass` = 组内没有重复（每个连通分量只剩单例）；
/// `fail` = 组内至少存在一对重复 → 证据列出**重复组 + 成员 assetId**（组键见 evidence.groupKey，
/// 报告/界面据此展示「这套房源里有几张是重复的」）。
///
/// 派生数据缺失的成员不计边（`engine-dedup` 的既有语义），但会在
/// `metrics.membersWithoutDupData` + `evidence.membersWithoutDupData` 里可见 ——
/// 编排器据此把该组降级为 `pending_human`（不拿残缺的组冒充结论）。
fn run_phash_dup(
    input: &GroupInput,
    params: &serde_yaml::Value,
) -> std::result::Result<GroupDetectorOutput, String> {
    let normalized = match params {
        serde_yaml::Value::Mapping(_) => params.clone(),
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let p: PhashDupParams = serde_yaml::from_value(normalized)
        .map_err(|e| format!("phash_dup: 参数错误: {e}"))?;

    let members: Vec<engine_dedup::DupMember> = input
        .members
        .iter()
        .map(|m| engine_dedup::DupMember {
            asset_id: m.asset_id,
            file_hash: m.file_hash.clone(),
            phash: m.phash,
        })
        .collect();
    let groups = engine_dedup::build_dup_groups(&members, p.threshold);

    let dup_groups: Vec<serde_json::Value> = groups
        .iter()
        .map(|g| {
            let ids: Vec<i64> = g.members.iter().map(|&i| input.members[i].asset_id).collect();
            serde_json::json!({
                "kind": g.kind.as_str(),
                "size": ids.len(),
                "members": ids,
            })
        })
        .collect();
    let dup_member_count: usize = groups.iter().map(|g| g.members.len()).sum();
    let largest = groups.iter().map(|g| g.members.len()).max().unwrap_or(0);
    let without_data: Vec<i64> = input
        .members
        .iter()
        .filter(|m| !m.has_dup_data())
        .map(|m| m.asset_id)
        .collect();
    let phash_from_db = input
        .members
        .iter()
        .filter(|m| m.phash_source == "db")
        .count();
    let phash_computed = input
        .members
        .iter()
        .filter(|m| m.phash_source == "computed")
        .count();

    let mut metrics = BTreeMap::new();
    metrics.insert("memberCount".to_string(), input.members.len() as f64);
    metrics.insert("dupGroups".to_string(), groups.len() as f64);
    metrics.insert("dupMembers".to_string(), dup_member_count as f64);
    metrics.insert("largestDupGroup".to_string(), largest as f64);
    metrics.insert("threshold".to_string(), p.threshold as f64);
    metrics.insert("phashFromDb".to_string(), phash_from_db as f64);
    metrics.insert("phashComputed".to_string(), phash_computed as f64);
    metrics.insert(
        "membersWithoutDupData".to_string(),
        without_data.len() as f64,
    );

    let evidence = serde_json::json!({
        "detector": "phash_dup",
        "scope": "group",
        "groupKey": input.group_key,
        "threshold": p.threshold,
        "memberCount": input.members.len(),
        "members": input.members.iter().map(|m| m.asset_id).collect::<Vec<_>>(),
        "dupGroups": dup_groups,
        "dupGroupsCount": groups.len(),
        "dupMembersCount": dup_member_count,
        "membersWithoutDupData": without_data,
        "phashSource": { "db": phash_from_db, "computed": phash_computed },
    });

    Ok(GroupDetectorOutput {
        pass: groups.is_empty(),
        score: groups.len() as f64,
        metrics,
        evidence,
    })
}

// ---------- 组键解析（pack 的 group_by → 组） ----------

/// 组键解析结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedGroups {
    /// 组键 → 成员（assetId 升序，成员 ≥1；键非空）
    pub by_key: BTreeMap<String, Vec<i64>>,
    /// 取不到组键的成员（缺键：落 `undetermined` + `group_key_missing`，不静默归组）
    pub missing: Vec<i64>,
}

/// 把「资产 + 组键取值」行解析成组（同键成组；缺键单独列出）。
///
/// 组键的**唯一来源**是本函数的输入（由 `ComplianceRepo::group_source_rows` 按 pack 的
/// `group_by` 取数）：`folder` → absPath 父目录；`asset_field` → 白名单列取值。
pub fn resolve_group_keys(rows: &[core_storage::compliance::GroupSourceRow]) -> ResolvedGroups {
    let mut out = ResolvedGroups::default();
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    for r in rows {
        if !seen.insert(r.asset_id) {
            continue; // 同一资产重复入参只算一次
        }
        match r.value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(k) => out.by_key.entry(k.to_string()).or_default().push(r.asset_id),
            None => out.missing.push(r.asset_id),
        }
    }
    for v in out.by_key.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    out.missing.sort_unstable();
    out.missing.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_storage::compliance::GroupSourceRow;

    fn member(id: i64, hash: Option<&str>, phash: Option<u64>) -> GroupMemberInput {
        GroupMemberInput {
            asset_id: id,
            file_hash: hash.map(|s| s.to_string()),
            phash,
            phash_source: if phash.is_some() { "db" } else { "none" },
            file_hash_source: if hash.is_some() { "db" } else { "none" },
        }
    }

    fn group(members: Vec<GroupMemberInput>) -> GroupInput {
        GroupInput {
            group_key: Some("/prop/g1".into()),
            members,
        }
    }

    #[test]
    fn exact_and_near_dups_are_grouped_with_member_ids() {
        // 1&2 字节相同（精确）｜3 与 1 汉明 1（近似）｜4 全 1（与 3 汉明 60，远超阈值 8）
        let g = group(vec![
            member(11, Some("aa"), Some(0b1010_1010)),
            member(12, Some("aa"), Some(0b1010_1010)),
            member(13, Some("bb"), Some(0b1010_1011)),
            member(14, Some("cc"), Some(u64::MAX)),
        ]);
        let out = run_phash_dup(&g, &serde_yaml::Value::Null).unwrap();
        assert!(!out.pass, "组内有重复 → fail");
        assert_eq!(out.score, 1.0, "一个连通分量");
        let dg = out.evidence["dupGroups"].as_array().unwrap();
        assert_eq!(dg.len(), 1);
        assert_eq!(dg[0]["kind"], "near", "含近似边 → near（混合组按 near 呈现）");
        assert_eq!(dg[0]["members"], serde_json::json!([11, 12, 13]));
        assert_eq!(out.metrics["dupMembers"], 3.0);
    }

    #[test]
    fn distinct_photos_pass_with_empty_evidence() {
        let g = group(vec![
            member(21, Some("a"), Some(0)),
            member(22, Some("b"), Some(u64::MAX)),
        ]);
        let out = run_phash_dup(&g, &serde_yaml::Value::Null).unwrap();
        assert!(out.pass);
        assert_eq!(out.score, 0.0);
        assert!(out.evidence["dupGroups"].as_array().unwrap().is_empty());
        assert_eq!(out.evidence["members"], serde_json::json!([21, 22]));
    }

    #[test]
    fn unknown_param_key_and_bad_type_are_errors() {
        let g = group(vec![member(1, Some("a"), Some(1))]);
        let bad = serde_yaml::from_str("group: within_batch").unwrap();
        let e = run_phash_dup(&g, &bad).unwrap_err();
        assert!(e.contains("参数错误"), "{e}");
        assert!(params_error("phash_dup", &bad).is_some());
        // 合法参数 + 未知检测器
        let ok = serde_yaml::from_str("threshold: 4").unwrap();
        assert!(params_error("phash_dup", &ok).is_none());
        let e = run("nope", &g, &ok).unwrap_err();
        assert!(e.contains("未知组级检测器"), "{e}");
        assert!(known_ids().contains("phash_dup"));
    }

    #[test]
    fn members_without_dup_data_are_visible_not_silent() {
        let g = group(vec![
            member(31, Some("a"), Some(0)),
            member(32, None, None), // 派生数据全缺 → 不可判
        ]);
        let out = run_phash_dup(&g, &serde_yaml::Value::Null).unwrap();
        assert_eq!(out.evidence["membersWithoutDupData"], serde_json::json!([32]));
        assert_eq!(out.metrics["membersWithoutDupData"], 1.0);
    }

    #[test]
    fn resolve_group_keys_splits_groups_and_lists_missing() {
        let rows = vec![
            GroupSourceRow {
                asset_id: 1,
                abs_path: "/p/a.jpg".into(),
                value: Some("/p".into()),
            },
            GroupSourceRow {
                asset_id: 2,
                abs_path: "/p/b.jpg".into(),
                value: Some("/p".into()),
            },
            GroupSourceRow {
                asset_id: 3,
                abs_path: "/q/c.jpg".into(),
                value: Some("  ".into()), // 空白 = 缺键
            },
            GroupSourceRow {
                asset_id: 4,
                abs_path: "/r/d.jpg".into(),
                value: None,
            },
            GroupSourceRow {
                asset_id: 1,
                abs_path: "/p/a.jpg".into(),
                value: Some("/p".into()), // 重复入参只算一次
            },
        ];
        let g = resolve_group_keys(&rows);
        assert_eq!(g.by_key.len(), 1);
        assert_eq!(g.by_key["/p"], vec![1, 2]);
        assert_eq!(g.missing, vec![3, 4]);
    }
}
