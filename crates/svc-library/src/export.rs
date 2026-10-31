//! D11 — 图片集导出编排（裁决 2026-10-13）：标签/查询结果一键导出。
//!
//! 入口来源二选一：
//! - [`ExportSource::Ids`]：批量操作条「导出」（当前选择集）
//! - [`ExportSource::Filter`]：标签面板「导出此标签全部」——**直通 `list_assets`
//!   查询管道**（D5 复合过滤同一套 WHERE 语义，分页取全量，忽略 filter.limit）
//!
//! 内容：原图（reference/copyin 都从 absPath 拷贝）/ 缩略图（256/1024 档；
//! 现成 thumbs 优先，缺的用 core-media 现场补生成——原图已失联则跳过入 missing 清单）。
//!
//! 打包：目录拷贝 / zip（内相对路径平铺）。manifest.csv 可选：
//! 文件名/标签/宽高/文件大小/原 absPath/assetId。
//!
//! 文件名策略：原文件名优先，冲突自动追加序号（`name.jpg` → `name_1.jpg`）；
//! 目录模式的目标已存在文件名也参与去重（二次导出同目录自动 _1/_2）。
//!
//! 取消安全（验收：取消中途导出无半截文件残留）：
//! - 目录模式逐文件「临时 `.name.part` + rename」，取消时删除在途 `.part`，
//!   已 rename 的文件恒完整；zip 模式整体写 `<dest>.part`，取消即删（目标 zip 不出现）。
//!
//! 批次记录：`job_queue` 新 kind=`export`（targetId=0，payload=目标摘要），
//! 开始置 running、结束 complete/fail——仅为历史/统计留痕，无消费者循环。
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use core_types::{ExportProgress, ExportReport, ExportSkipped, ImportFilter};
// C2：判定行读取一律经 repo trait（存储解耦；不直接拼 SQL）
use core_storage::compliance::ComplianceRepo as _;
#[cfg(test)]
use core_storage::compliance::NewVerdict;
use rusqlite::Connection;

use crate::{LibraryError, Result};

/// job_queue 的导出 kind（D11 冻结值）。
pub const EXPORT_KIND: &str = "export";

/// 缩略图合法档位（对齐 core-media THUMB_SIZES）。
pub const THUMB_SIZES: &[i32] = &[256, 1024];

/// 导出内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportContent {
    /// 原图（absPath 拷贝）
    Original,
    /// 缩略图档位（256/1024）
    Thumb(i32),
}

/// 导出目标。
#[derive(Debug, Clone)]
pub enum ExportTarget {
    /// 拷贝到目录
    Dir(PathBuf),
    /// 打包 zip
    Zip(PathBuf),
}

/// 导出来源（二选一）。
#[derive(Debug, Clone)]
pub enum ExportSource {
    /// 批量选择集
    Ids(Vec<i64>),
    /// 查询过滤（标签面板等；走 list_assets 管道取全量命中）
    Filter(ImportFilter),
}

/// 导出编排参数（领域形态；IPC 形态见 core_types::ExportSpecParams，L3 转换）。
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub source: ExportSource,
    pub content: ExportContent,
    pub target: ExportTarget,
    pub manifest: bool,
    /// C2 判定报告：Some → manifest.csv 换成判定报告形态
    /// （pack report.yaml 的 detail_columns 逐规则列 + 末行汇总）。
    pub report: Option<VerdictReportSpec>,
    /// EXIF2：剥离 GPS（导出副本的 JPEG 重排 APP1 去 GPS IFD；清单恒不落坐标）。
    /// 默认 false = 原样导出。
    pub strip_gps: bool,
}

/// 判定报告导出声明：按插件元数据取列模板，按 job（或每资产最新批次）取判定行。
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictReportSpec {
    /// 行业元数据（IP-K16：唯一真源 = 插件包自带的 `resources/rules.json`）。
    /// 报告列模板（`report.detail_columns`）与合法列集合（规则 id）都从这里来，
    /// 宿主不再按 pack id 去装载内置 YAML 包。
    pub pack: core_config::Pack,
    /// None = 每 (asset, rule) 取最新一行（跨批次重跑后即最新状态）
    pub job_id: Option<i64>,
    /// DECouple-BE：按**一次执行**（`compliance_runs.runId`）取判定行 ——
    /// 任务中心「按这次 job 结果导出」用它（一次执行 = N 个资产队列行）。
    pub run_id: Option<i64>,
}

/// 导出项（解析后的资产快照）。
#[derive(Debug, Clone)]
pub struct ExportItem {
    pub asset_id: i64,
    pub file_name: String,
    pub abs_path: String,
    pub missing: bool,
    pub file_sz: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub tags: Vec<String>,
    /// EXIF2：库内是否有 GPS 坐标（清单 hasGps 列；坐标本身永不进清单）
    pub has_gps: bool,
    /// KI-15：approvalStatus == 'rejected'（拒选 = 留在库里但不交付；
    /// 导出/判定报告汇总一律排除，无开关——语义见裁决 2026-10-20）
    pub rejected: bool,
}

impl LibraryError {
    fn export(msg: impl Into<String>) -> Self {
        LibraryError::ImageIo(msg.into())
    }
}

/// 选择集命中数（导出对话框预览用；filter 走与 list_assets 同一 WHERE）。
pub fn count_assets(conn: &Connection, f: &ImportFilter) -> Result<i64> {
    let (wh, params) = crate::filter_where(f);
    let sql = format!("SELECT COUNT(*) FROM assets a WHERE {wh}");
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    Ok(conn.query_row(&sql, param_refs.as_slice(), |r| r.get(0))?)
}

/// 解析导出项（Ids 直查 / Filter 走 list_assets 分页取全量）。
pub fn resolve_items(conn: &Connection, source: &ExportSource) -> Result<Vec<ExportItem>> {
    let views = match source {
        ExportSource::Ids(ids) => assets_by_ids(conn, ids)?,
        ExportSource::Filter(f) => {
            // 忽略 filter.limit（导出要全量命中），按页取完
            let mut all = Vec::new();
            let mut offset = 0i64;
            loop {
                let page = crate::list_assets(
                    conn,
                    &ImportFilter {
                        limit: Some(1000),
                        offset: Some(offset),
                        ..f.clone()
                    },
                )?;
                let n = page.len();
                all.extend(page);
                if n < 1000 {
                    break;
                }
                offset += n as i64;
            }
            all
        }
    };
    let mut items: Vec<ExportItem> = views
        .into_iter()
        .map(|v| ExportItem {
            asset_id: v.asset.assetId,
            file_name: v.asset.fileName,
            abs_path: v.asset.absPath,
            missing: v.asset.missing == 1,
            file_sz: v.asset.fileSz,
            width: v.width,
            height: v.height,
            tags: Vec::new(),
            has_gps: v.asset.gpsLat.is_some() || v.asset.gpsLon.is_some(),
            rejected: v.asset.approvalStatus == "rejected",
        })
        .collect();
    items.sort_by(|a, b| a.asset_id.cmp(&b.asset_id)); // 稳定输出顺序
    load_tags(conn, &mut items)?;
    Ok(items)
}

/// 按 id 集取资产（deleted 恒排除；顺序按 assetId）。
fn assets_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<core_types::AssetView>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT a.assetId, a.mediaType, a.absPath, a.storeMode, a.missing, a.fileName, a.fileExt,
                    a.fileSz, a.mtime, a.fileHash, a.title_ch, a.title_en, a.description, a.source,
                    a.parentAssetId, a.searchable, a.deleted, a.approvalStatus, a.starRating,
                    a.colorLabel, a.cloudAssetId, a.importedAt,
                    COALESCE(a.emb_status, 'none'), a.emb_model,
                    COALESCE(a.dedup_status, 'none'), a.dedup_model,
                    r.thumbPath, r.width, r.height, r.pHash,
                    a.takenAt, a.cameraMake, a.cameraModel, a.lensModel, a.iso,
                    a.focalLenMm, a.aperture, a.shutterText, a.orientation,
                    a.aspectRatio, a.aspectClass, a.gps_lat, a.gps_lon, a.gpsSource, a.exif
             FROM assets a
             LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
             WHERE a.deleted = 0 AND a.assetId IN ({placeholders})
             ORDER BY a.assetId"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), crate::map_asset_view)?;
        out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    Ok(out)
}

/// 批量回填标签（manifest 用；按 assetId 排序去重，manual 优先展示顺序不做区分）。
fn load_tags(conn: &Connection, items: &mut [ExportItem]) -> Result<()> {
    let ids: Vec<i64> = items.iter().map(|i| i.asset_id).collect();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT assetId, value FROM tag WHERE assetId IN ({placeholders}) ORDER BY value",
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (aid, value) = row?;
            if let Some(it) = items.iter_mut().find(|i| i.asset_id == aid) {
                it.tags.push(value);
            }
        }
    }
    Ok(())
}

/// 清洗输出文件名：去路径成分 + 替换分隔符（防 `../` 逃逸与跨平台非法字符）。
fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    base.replace(['/', '\\', ':'], "_")
}

/// 冲突追加序号：`name.jpg` → `name_1.jpg`、`name_2.jpg`…（无扩展名 → `name_1`）。
/// `taken` 既查也改（登记最终名）。
pub fn unique_name(taken: &mut HashSet<String>, raw: &str) -> String {
    let name = sanitize_name(raw);
    if !taken.contains(&name) {
        taken.insert(name.clone());
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    for n in 1.. {
        let cand = format!("{stem}_{n}{ext}");
        if !taken.contains(&cand) {
            taken.insert(cand.clone());
            return cand;
        }
    }
    unreachable!("序号空间耗尽")
}

/// 缩略图导出的输出名：`x1.jpg` → `x1_256.webp`（档位 + 缩略图真实格式）。
fn thumb_out_name(file_name: &str, size: i32) -> String {
    let clean = sanitize_name(file_name);
    let stem = clean.rsplit_once('.').map(|(s, _)| s).unwrap_or(&clean);
    format!("{stem}_{size}.webp")
}

/// CSV 字段转义（恒引号包裹，内部引号翻倍——最保守且任何表格软件可读）。
pub(crate) fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// manifest.csv 内容（含表头）；rows 与导出文件一一对应（fileName=最终唯一名）。
/// KI-15：末行恒记 `excludedRejected,<N>`（拒选不进交付；0 也记，口径可审计）。
pub fn manifest_csv(items: &[(ExportItem, String)], excluded_rejected: usize) -> String {
    // EXIF2：hasGps 列只标「有/无坐标」；经纬度数值永不进清单（隐私红线）
    let mut out = String::from("fileName,tags,width,height,fileSize,absPath,assetId,hasGps\n");
    for (it, final_name) in items {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            csv_field(final_name),
            csv_field(&it.tags.join(";")),
            csv_field(&it.width.map(|w| w.to_string()).unwrap_or_default()),
            csv_field(&it.height.map(|h| h.to_string()).unwrap_or_default()),
            csv_field(&it.file_sz.to_string()),
            csv_field(&it.abs_path),
            csv_field(&it.asset_id.to_string()),
            csv_field(if it.has_gps { "1" } else { "" }),
        ));
    }
    out.push_str(&format!(
        "excludedRejected,{}\n",
        csv_field(&excluded_rejected.to_string())
    ));
    out
}

// ---------- C2 判定报告（manifest.csv 的判定报告形态） ----------

/// 汇总档位（canonical 顺序 = pack report.yaml summary 声明序，三态口径冻结三档）。
/// 仅用于「未知列」报错文案（提示合法口径），不参与取值。
const SUMMARY_CANON: [&str; 3] = ["合格", "不合格", "未判定"];

/// KI-15：判定报告尾列列名与拒选标记（明细仍列出拒选资产，但不进汇总/交付）。
const REJECTED_COL: &str = "拒选";
const REJECTED_MARK: &str = "已拒选";

/// manifest 落盘内容分发：普通 manifest / 判定报告（spec.report）。
/// `excluded` = 选择集内被拒选排除的资产（KI-15：不出文件、不进 manifest 行、
/// 不进判定报告汇总；判定报告明细仍列出并带「已拒选」标记）。
fn manifest_text(
    conn: &Connection,
    spec: &ExportSpec,
    items: &[(ExportItem, String)],
    excluded: &[ExportItem],
) -> Result<String> {
    match &spec.report {
        Some(r) => verdict_report_csv(conn, r, items, excluded)
            .map_err(|e| LibraryError::export(format!("判定报告: {e}"))),
        None => Ok(manifest_csv(items, excluded.len())),
    }
}

/// 判定报告 csv：表头 = pack report.yaml detail_columns + 尾列「拒选」（KI-15）；
/// 每导出文件一行逐规则判定；末行 = 汇总（三档计数 + total + unjudged + excludedRejected）。
///
/// KI-15 拒选口径：`excluded`（选择集内 rejected）**不出导出文件、不进汇总计数**
/// （不进交付口径），但明细行仍列出、尾列标「已拒选」——交付范围与审计范围分开。
///
/// 列语义（列名 → 取值；未知列响亮报错）：
/// - `imageSN` → assetId；`<id>` → 该规则/步骤 verdict result；
/// - `<id>_score` / `<id>_fix` / `<id>_recheck` → 同名字段；
/// - `<id>_confidence` → 置信度（COMPLIANCE-DOMAIN：三态口径下每行都有；历史 NULL 行给空）；
/// - `<id>_tags` → evidence.outputs 数组 `;` 连接（打标型，C5 起填充）。
pub fn verdict_report_csv(
    conn: &Connection,
    spec: &VerdictReportSpec,
    items: &[(ExportItem, String)],
    excluded: &[ExportItem],
) -> std::result::Result<String, String> {
    // 列模板来自插件自带元数据（IP-K16：不再按 id 装载内置 YAML 包）
    let pack = &spec.pack;
    let columns = &pack.report.detail_columns;
    if columns.is_empty() {
        return Err(format!("pack {}: report.detail_columns 为空", pack.id));
    }
    // 合法列名集合 = imageSN + 规则 id + do 步骤 id
    let known: std::collections::BTreeSet<&str> = {
        let mut s: std::collections::BTreeSet<&str> =
            pack.rules.iter().map(|r| r.id.as_str()).collect();
        for st in &pack.pipeline.steps {
            if let Some(id) = &st.id {
                s.insert(id.as_str());
            }
        }
        s
    };
    for col in columns {
        let base = col.strip_suffix("_score").or_else(|| col.strip_suffix("_fix"))
            .or_else(|| col.strip_suffix("_recheck")).or_else(|| col.strip_suffix("_tags"))
            .or_else(|| col.strip_suffix("_confidence"))
            .unwrap_or(col);
        if col != &"imageSN" && !known.contains(base) {
            return Err(format!(
                "未知报告列 '{col}'（合法: imageSN 或 {:?}/{:?} 之一）",
                known, SUMMARY_CANON
            ));
        }
    }

    // 判定行（存储解耦：一律走 core-storage repo trait）
    let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(conn);
    let filter = match spec.run_id {
        Some(run) => core_storage::compliance::VerdictFilter::run(&pack.id, run),
        None => core_storage::compliance::VerdictFilter::job_opt(&pack.id, spec.job_id),
    };
    let verdicts = repo.find_verdicts(&filter).map_err(|e| e.to_string())?;
    // COMPLIANCE-HUMAN：人工改判（生效结论 humanStatus ?? result）—— 报告与界面必须同源，
    // 否则「页面上某张图已改判合格，导出的报告还写不合格」。历史行（已撤销/被覆盖）不参与。
    let reviews =
        crate::compliance_view::ReviewIndex::load(conn, &pack.id).map_err(|e| e.to_string())?;
    let mut idx: std::collections::BTreeMap<(i64, &str), &core_storage::compliance::VerdictRow> =
        Default::default();
    let mut by_asset: std::collections::BTreeMap<i64, Vec<&core_storage::compliance::VerdictRow>> =
        Default::default();
    for v in &verdicts {
        idx.entry((v.asset_id, v.rule_id.as_str())).or_insert(v);
        by_asset.entry(v.asset_id).or_default().push(v);
    }

    // 明细行（KI-15：交付项 + 拒选项合并按 assetId 稳定排序；拒选项尾列标「已拒选」）
    let mut out = String::new();
    out.push_str(&columns.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(","));
    out.push(',');
    out.push_str(&csv_field(REJECTED_COL));
    out.push('\n');
    let mut rows: Vec<(&ExportItem, bool)> = items
        .iter()
        .map(|(it, _)| (it, false))
        .chain(excluded.iter().map(|it| (it, true)))
        .collect();
    rows.sort_by_key(|(it, _)| it.asset_id);
    for (it, rejected) in rows {
        let mut cells: Vec<String> = columns
            .iter()
            .map(|col| verdict_column(col, it, &idx, &reviews))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .map(|v| csv_field(v))
            .collect();
        cells.push(csv_field(if rejected { REJECTED_MARK } else { "" }));
        out.push_str(&cells.join(","));
        out.push('\n');
    }

    // 汇总行：summary,<label>,<n>,… ×3 档（合格/不合格/未判定）+ total + unjudged +
    // excludedRejected（KI-15：汇总只数交付项——items；拒选不进交付口径）
    let mut counts = [0usize; 4];
    let mut unjudged = 0usize;
    for (it, _) in items {
        match by_asset.get(&it.asset_id) {
            None => unjudged += 1,
            Some(rows) => counts[asset_status(rows, &reviews)] += 1,
        }
    }
    let cells: Vec<String> = std::iter::once(csv_field("summary"))
        .chain(pack.report.summary.iter().zip(counts).take(4).flat_map(|(label, n)| {
            [csv_field(label), csv_field(&n.to_string())]
        }))
        .chain([
            csv_field("total"),
            csv_field(&items.len().to_string()),
            csv_field("unjudged"),
            csv_field(&unjudged.to_string()),
            csv_field("excludedRejected"),
            csv_field(&excluded.len().to_string()),
        ])
        .collect();
    out.push_str(&cells.join(","));
    out.push('\n');
    Ok(out)
}

/// 单列取值（`_score/_fix/_recheck/_tags/_confidence` 后缀拆 base → 规则行字段）。
///
/// COMPLIANCE-HUMAN：无后缀的规则列给**生效结论**（humanStatus ?? result）—— 报告是给人看的
/// 最终结论；机器原值仍完整留在 `rule_verdict`（改判历史在 `verdict_human_review`，可查）。
fn verdict_column(
    col: &str,
    item: &ExportItem,
    idx: &std::collections::BTreeMap<(i64, &str), &core_storage::compliance::VerdictRow>,
    reviews: &crate::compliance_view::ReviewIndex,
) -> std::result::Result<String, String> {
    if col == "imageSN" {
        return Ok(item.asset_id.to_string());
    }
    let (base, suffix) = if let Some(b) = col.strip_suffix("_score") {
        (b, "score")
    } else if let Some(b) = col.strip_suffix("_fix") {
        (b, "fix")
    } else if let Some(b) = col.strip_suffix("_recheck") {
        (b, "recheck")
    } else if let Some(b) = col.strip_suffix("_tags") {
        (b, "tags")
    } else if let Some(b) = col.strip_suffix("_confidence") {
        (b, "confidence")
    } else {
        (col, "")
    };
    let Some(row) = idx.get(&(item.asset_id, base)) else {
        return Ok(String::new()); // 该资产无此规则判定（未判/短路例外列）→ 空单元格
    };
    Ok(match suffix {
        "" => crate::compliance_view::effective_result(row, reviews).to_string(),
        "score" => row.score.map(|s| s.to_string()).unwrap_or_default(),
        "fix" => row.fix_action.clone().unwrap_or_default(),
        "recheck" => row.recheck_result.clone().unwrap_or_default(),
        // 历史行（migration 0016 前）置信度不可知 → 空单元格（不造数）
        "confidence" => row.confidence.map(|c| c.to_string()).unwrap_or_default(),
        "tags" => row
            .evidence
            .as_deref()
            .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
            .and_then(|v| v.get("outputs").cloned())
            .and_then(|o| o.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join(";")
            })
            .unwrap_or_default(),
        other => return Err(format!("未知列后缀 '{other}'")),
    })
}

/// 单资产合规状态（canonical 档位下标）：0 合格 / 1 不合格 / 2 未判定。
///
/// C6/COMPLIANCE-HUMAN：口径在 `crate::compliance_view::state_of_verdicts_with`
/// （库视图筛选/审核队列/报告必须同源，含人工改判），这里只做「状态 → 报告档位下标」的映射
/// （Unjudged 无档位：报告里由"完全没判"单独计数）。
fn asset_status(
    rows: &[&core_storage::compliance::VerdictRow],
    reviews: &crate::compliance_view::ReviewIndex,
) -> usize {
    crate::compliance_view::state_of_verdicts_with(rows, reviews)
        .report_index()
        .unwrap_or(0)
}

/// 解析导出项的源文件路径（原图=absPath；缩略图=thumbs 档位，缺则现场补生成）。
/// 返回 Err 仅用于不可继续的系统级错误；「原图缺失」返回 None（跳过入清单）。
fn source_file(
    conn: &Connection,
    item: &ExportItem,
    content: &ExportContent,
    thumbs_root: &Path,
) -> std::result::Result<Option<PathBuf>, String> {
    let original = Path::new(&item.abs_path);
    match content {
        ExportContent::Original => {
            // G5-A：这里就是「导出原图」这个真实失败点 —— 顺手记账（不在了 → missing=1；
            // 文件回来了 → 清标）。跳过清单的形状不变：不在了 / 读不了都进 skipped「原图缺失」。
            match core_storage::presence::probe_and_note(conn, item.asset_id, original) {
                core_storage::presence::Presence::Present => Ok(Some(original.to_path_buf())),
                _ => Ok(None),
            }
        }
        ExportContent::Thumb(size) => {
            if !THUMB_SIZES.contains(size) {
                return Err(format!("非法缩略图档位 {size}"));
            }
            let tp = core_storage::thumb_path(thumbs_root, item.asset_id, *size);
            if tp.is_file() {
                return Ok(Some(tp));
            }
            // 现场补生成：需要原图可读 —— 同一个失败点记账（缩略图导出也要碰原图）
            if core_storage::presence::probe_and_note(conn, item.asset_id, original)
                != core_storage::presence::Presence::Present
            {
                return Ok(None);
            }
            if let Some(dir) = tp.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("建缩略图目录 {dir:?}: {e}"))?;
            }
            core_media::thumbnail_to(original, &tp, *size)
                .map_err(|e| format!("asset {} 补生成 {size} 缩略图: {e}", item.asset_id))?;
            Ok(Some(tp))
        }
    }
}

/// 原子拷贝：先写 `.name.part` 再 rename（取消/崩溃不留半截目标文件）。
/// 返回 Err 前 guarantee 临时文件已清理。
/// EXIF2 strip_gps 的落盘：JPEG 原图重排 APP1 去 GPS 后原子写；其余格式原样拷贝
/// （PNG/WebP 剥离 v1 未做，见报告未覆盖清单——诚实标注，不静默假装剥了）。
fn copy_atomic_maybe_strip(src: &Path, dest: &Path, strip_gps: bool) -> Result<()> {
    if !strip_gps {
        copy_atomic(src, dest).map_err(|e| LibraryError::export(e.to_string()))?;
        return Ok(());
    }
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "jpg" | "jpeg") {
        let bytes = std::fs::read(src).map_err(|e| LibraryError::export(e.to_string()))?;
        let stripped = core_media::strip_gps_jpeg(&bytes)
            .map_err(|e| LibraryError::export(format!("GPS 剥离失败: {e}")))?;
        let tmp = dest.with_file_name(format!(
            ".{}.part",
            dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        ));
        let r = std::fs::write(&tmp, &stripped).and_then(|_| std::fs::rename(&tmp, dest));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.map_err(|e| LibraryError::export(e.to_string()))?;
        return Ok(());
    }
    copy_atomic(src, dest).map_err(|e| LibraryError::export(e.to_string()))?;
    Ok(())
}

fn copy_atomic(src: &Path, dest: &Path) -> std::io::Result<()> {
    let tmp = dest.with_file_name(format!(
        ".{}.part",
        dest.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let r = std::fs::copy(src, &tmp).and_then(|_| std::fs::rename(&tmp, dest));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// 执行导出（阻塞；L3 放后台线程调用）。进度经回调抛出，取消查 `cancel` 旗标。
pub fn run_export(
    conn: &Connection,
    spec: &ExportSpec,
    thumbs_root: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ExportProgress),
) -> Result<ExportReport> {
    let mut report = ExportReport {
        dest: match &spec.target {
            ExportTarget::Dir(d) => d.to_string_lossy().into_owned(),
            ExportTarget::Zip(z) => z.to_string_lossy().into_owned(),
        },
        ..Default::default()
    };

    // 1) 解析选择集
    progress(ExportProgress {
        phase: "resolve".into(),
        done: 0,
        total: 0,
        current: None,
    });
    let items = resolve_items(conn, &spec.source)?;
    report.total = items.len();
    // KI-15：拒选 = 留在库里但不交付 —— 选择集内 approvalStatus=rejected 一律排除，
    // **无开关**（不提供「含拒选导出」）。排除数进报告回执/manifest/判定报告，
    // 拒选资产保留给判定报告做「已拒选」明细（见 manifest_text）。
    let (deliverable, excluded): (Vec<ExportItem>, Vec<ExportItem>) =
        items.into_iter().partition(|it| !it.rejected);
    report.excluded_rejected = excluded.len();
    progress(ExportProgress {
        phase: "resolve".into(),
        done: 0,
        total: report.total,
        current: None,
    });

    // 2) job_queue 批次留痕（kind=export，无消费者；结束即终态）
    let payload = serde_json::json!({
        "dest": report.dest,
        "content": match &spec.content {
            ExportContent::Original => "original".to_string(),
            ExportContent::Thumb(s) => format!("thumb{s}"),
        },
        "total": report.total,
        "excludedRejected": report.excluded_rejected,
        "manifest": spec.manifest,
    })
    .to_string();
    let job_id = engine_jobq::enqueue(conn, EXPORT_KIND, 0, Some(&payload))?;
    conn.execute(
        "UPDATE job_queue SET status='running', updatedAt=unixepoch() WHERE id=?1",
        [job_id],
    )?;
    let finish_job = |failed: bool, err: Option<String>| -> Result<()> {
        if failed {
            engine_jobq::fail(conn, job_id, &err.unwrap_or_default())?;
        } else {
            engine_jobq::complete(conn, job_id)?;
        }
        Ok(())
    };

    // 3) 预定输出名（原图原名优先；缩略图加档位后缀；冲突追加序号）
    let mut taken: HashSet<String> = HashSet::new();
    if let ExportTarget::Dir(d) = &spec.target {
        std::fs::create_dir_all(d)
            .map_err(|e| LibraryError::export(format!("建目录 {d:?}: {e}")))?;
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                taken.insert(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    struct Planned {
        item: ExportItem,
        out_name: String,
    }
    let mut planned: Vec<Planned> = Vec::new();
    for it in deliverable {
        let raw = match &spec.content {
            ExportContent::Original => it.file_name.clone(),
            ExportContent::Thumb(s) => thumb_out_name(&it.file_name, *s),
        };
        planned.push(Planned {
            out_name: unique_name(&mut taken, &raw),
            item: it,
        });
    }

    // 4) 写出
    let mut manifest_rows: Vec<(ExportItem, String)> = Vec::new();
    let write_err = |report: &mut ExportReport, msg: String| {
        if report.errors.len() < 50 {
            report.errors.push(msg);
        }
    };

    match &spec.target {
        ExportTarget::Dir(dest) => {
            for (i, p) in planned.iter().enumerate() {
                if cancel.load(Ordering::SeqCst) {
                    report.cancelled = true;
                    break;
                }
                progress(ExportProgress {
                    phase: "copy".into(),
                    done: report.exported,
                    total: report.total,
                    current: Some(p.out_name.clone()),
                });
                match source_file(conn, &p.item, &spec.content, thumbs_root) {
                    Err(e) => write_err(&mut report, e),
                    Ok(None) => report.skipped.push(ExportSkipped {
                        asset_id: p.item.asset_id,
                        file_name: p.item.file_name.clone(),
                        reason: "原图缺失".into(),
                    }),
                    Ok(Some(src)) => {
                        let dest_file = dest.join(&p.out_name);
                        match copy_atomic_maybe_strip(&src, &dest_file, spec.strip_gps) {
                            Ok(_) => {
                                report.exported += 1;
                                manifest_rows.push((p.item.clone(), p.out_name.clone()));
                            }
                            Err(e) => write_err(
                                &mut report,
                                format!("asset {} 拷贝失败: {e}", p.item.asset_id),
                            ),
                        }
                    }
                }
                let _ = i;
            }
            if !report.cancelled && spec.manifest {
                progress(ExportProgress {
                    phase: "manifest".into(),
                    done: report.exported,
                    total: report.total,
                    current: Some("manifest.csv".into()),
                });
                let dest_file = dest.join("manifest.csv");
                std::fs::write(&dest_file, manifest_text(conn, spec, &manifest_rows, &excluded)?)
                    .map_err(|e| LibraryError::export(format!("写 manifest.csv: {e}")))?;
                report.manifest_written = true;
            }
        }
        ExportTarget::Zip(zip_path) => {
            // 整体先写 <zip>.part，取消/失败删除——目标 zip 永不出现半截
            let tmp = zip_path.with_extension("zip.part");
            if let Some(dir) = tmp.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| LibraryError::export(format!("建目录 {dir:?}: {e}")))?;
            }
            let file = std::fs::File::create(&tmp)
                .map_err(|e| LibraryError::export(format!("建临时 zip {tmp:?}: {e}")))?;
            let mut zw = zip::ZipWriter::new(file);
            let opts: zip::write::SimpleFileOptions = Default::default();
            let mut zip_err: Option<String> = None;
            for p in planned.iter() {
                if cancel.load(Ordering::SeqCst) {
                    report.cancelled = true;
                    break;
                }
                progress(ExportProgress {
                    phase: "copy".into(),
                    done: report.exported,
                    total: report.total,
                    current: Some(p.out_name.clone()),
                });
                match source_file(conn, &p.item, &spec.content, thumbs_root) {
                    Err(e) => write_err(&mut report, e),
                    Ok(None) => report.skipped.push(ExportSkipped {
                        asset_id: p.item.asset_id,
                        file_name: p.item.file_name.clone(),
                        reason: "原图缺失".into(),
                    }),
                    Ok(Some(src)) => {
                        let res: std::result::Result<(), String> = (|| {
                            zw.start_file(p.out_name.clone(), opts)
                                .map_err(|e| format!("zip start {}: {e}", p.out_name))?;
                            // EXIF2：strip_gps 时 JPEG 原图先剥 GPS 再入 zip
                            let src_ext = src
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_ascii_lowercase();
                            if spec.strip_gps && matches!(src_ext.as_str(), "jpg" | "jpeg") {
                                let bytes =
                                    std::fs::read(&src).map_err(|e| format!("asset {} 读取: {e}", p.item.asset_id))?;
                                let stripped = core_media::strip_gps_jpeg(&bytes)
                                    .map_err(|e| format!("asset {} GPS 剥离: {e}", p.item.asset_id))?;
                                std::io::copy(&mut std::io::Cursor::new(stripped), &mut zw)
                                    .map_err(|e| format!("asset {} 写 zip: {e}", p.item.asset_id))?;
                            } else {
                                let mut f = std::fs::File::open(&src).map_err(|e| {
                                    format!("asset {} 打开源文件: {e}", p.item.asset_id)
                                })?;
                                std::io::copy(&mut f, &mut zw)
                                    .map_err(|e| format!("asset {} 写 zip: {e}", p.item.asset_id))?;
                            }
                            Ok(())
                        })();
                        match res {
                            Ok(_) => {
                                report.exported += 1;
                                manifest_rows.push((p.item.clone(), p.out_name.clone()));
                            }
                            Err(e) => write_err(&mut report, e),
                        }
                    }
                }
            }
            if report.cancelled || report.errors.len() >= 50 {
                zip_err = Some("取消/错误过多，放弃 zip 收尾".into());
            } else {
                if spec.manifest {
                    progress(ExportProgress {
                        phase: "manifest".into(),
                        done: report.exported,
                        total: report.total,
                        current: Some("manifest.csv".into()),
                    });
                    zw.start_file("manifest.csv", opts)
                        .map_err(|e| LibraryError::export(format!("zip 写 manifest: {e}")))?;
                    zw.write_all(manifest_text(conn, spec, &manifest_rows, &excluded)?.as_bytes())
                        .map_err(|e| LibraryError::export(format!("zip 写 manifest: {e}")))?;
                    report.manifest_written = true;
                }
                progress(ExportProgress {
                    phase: "finalize".into(),
                    done: report.exported,
                    total: report.total,
                    current: None,
                });
                zw.finish()
                    .map_err(|e| LibraryError::export(format!("zip 收尾: {e}")))?;
                std::fs::rename(&tmp, zip_path)
                    .map_err(|e| LibraryError::export(format!("zip 落盘 {zip_path:?}: {e}")))?;
            }
            if report.cancelled || zip_err.is_some() {
                let _ = std::fs::remove_file(&tmp); // 半截 zip 不留
                if let Some(e) = zip_err.filter(|_| !report.cancelled) {
                    write_err(&mut report, e);
                }
            }
        }
    }

    let job_failed = !report.errors.is_empty();
    finish_job(job_failed, report.errors.first().cloned())?;
    progress(ExportProgress {
        phase: "done".into(),
        done: report.exported,
        total: report.total,
        current: None,
    });
    Ok(report)
}

// ---------------- 单测（卡验收：文件名冲突 / missing 跳过 / manifest 生成 / 取消 / zip） ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        conn
    }

    /// 判定行归属的包 id = 插件 id（IP-K16：判定按插件包 id 落库）。
    const PACK_ID: &str = "com.soujpg.pack.ecommerce";

    /// 报告列模板 = 插件自带的 `resources/rules.json`（生产同源，不再有内置 YAML 包）。
    fn ec_pack() -> core_config::Pack {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packs/plugins/ecommerce");
        core_config::Pack::from_plugin_dir(&dir).expect("电商插件元数据装载")
    }

    #[test]
    fn unique_name_conflict_suffix() {
        let mut taken = HashSet::new();
        assert_eq!(unique_name(&mut taken, "a.jpg"), "a.jpg");
        assert_eq!(unique_name(&mut taken, "a.jpg"), "a_1.jpg");
        assert_eq!(unique_name(&mut taken, "a.jpg"), "a_2.jpg");
        assert_eq!(unique_name(&mut taken, "b.tar.gz"), "b.tar.gz");
        assert_eq!(unique_name(&mut taken, "b.tar.gz"), "b.tar_1.gz");
        assert_eq!(unique_name(&mut taken, "noext"), "noext");
        assert_eq!(unique_name(&mut taken, "noext"), "noext_1");
        // 路径清洗
        let mut t2 = HashSet::new();
        assert_eq!(unique_name(&mut t2, "../evil/x.jpg"), "x.jpg");
        assert_eq!(unique_name(&mut t2, "a/b.jpg"), "b.jpg");
    }

    #[test]
    fn manifest_csv_escapes_and_columns() {
        let items = vec![
            (
                ExportItem {
                    has_gps: false,
                    asset_id: 7,
                    file_name: "a,quote\".jpg".into(),
                    abs_path: "/x/a.jpg".into(),
                    missing: false,
                    file_sz: 123,
                    width: Some(100),
                    height: Some(80),
                    tags: vec!["海".into(), "日落,黄昏".into()],
                    rejected: false,
                },
                "a_1.jpg".to_string(),
            ),
            (
                ExportItem {
                    has_gps: false,
                    asset_id: 8,
                    file_name: "b.jpg".into(),
                    abs_path: "/y/b.jpg".into(),
                    missing: false,
                    file_sz: 1,
                    width: None,
                    height: None,
                    tags: vec![],
                    rejected: false,
                },
                "b.jpg".to_string(),
            ),
        ];
        let csv = manifest_csv(&items, 0);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "fileName,tags,width,height,fileSize,absPath,assetId,hasGps"
        );
        assert_eq!(
            lines[1],
            "\"a_1.jpg\",\"海;日落,黄昏\",\"100\",\"80\",\"123\",\"/x/a.jpg\",\"7\",\"\""
        );
        assert_eq!(
            lines[2],
            "\"b.jpg\",\"\",\"\",\"\",\"1\",\"/y/b.jpg\",\"8\",\"\""
        );
        // KI-15：末行恒记拒选排除数（0 也记，口径可审计）
        assert_eq!(lines[3], "excludedRejected,\"0\"");
        let csv2 = manifest_csv(&items, 2);
        assert_eq!(
            csv2.lines().last().unwrap(),
            "excludedRejected,\"2\"",
            "拒选排除数必须进 manifest"
        );
    }

    #[test]
    fn dir_export_original_missing_skip_and_manifest() {
        let conn = setup();
        // 3 资产：1、2 正常，3 missing（盘上无文件）
        let tmp = std::env::temp_dir().join(format!("dam_export_miss_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[1u8; 12]);
        std::fs::write(src.join("ok.jpg"), &buf).unwrap();
        std::fs::write(src.join("ok2.jpg"), &buf).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '{p1}', 0, 'ok.jpg', 'jpg', 12, 1, 1),
                (2, '{p2}', 0, 'ok2.jpg', 'jpg', 12, 1, 2),
                (3, '{p3}', 1, 'gone.jpg', 'jpg', 12, 1, 3);
             INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES
                (1, '海', 'manual', 1.0, 1), (2, '山', 'auto', 0.8, 1);",
            p1 = src.join("ok.jpg").to_string_lossy(),
            p2 = src.join("ok2.jpg").to_string_lossy(),
            p3 = src.join("gone.jpg").to_string_lossy(),
        ))
        .unwrap();

        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let cancel = AtomicBool::new(false);
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.exported, 2);
        assert!(!report.cancelled);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].asset_id, 3);
        assert_eq!(report.skipped[0].reason, "原图缺失");
        assert!(report.manifest_written);
        // 文件落地
        assert!(dest.join("ok.jpg").is_file());
        assert!(dest.join("ok2.jpg").is_file());
        assert!(!dest.join("gone.jpg").exists());
        let csv = std::fs::read_to_string(dest.join("manifest.csv")).unwrap();
        assert!(csv.contains("ok.jpg"));
        assert!(!csv.contains("gone.jpg"));
        // job 留痕：done
        let st: String = conn
            .query_row(
                "SELECT status FROM job_queue WHERE kind='export'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(st, "done");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// G5-A（卡 task）：失联「失败即标」。
    /// 导出原图是真碰磁盘的点 —— 文件被外部删掉（watcher 没看到、DB 仍 missing=0）时，
    /// 这次导出必须顺手把该资产标 missing=1；文件放回再导出 → 清回 missing=0。
    /// 跳过清单形状不变（仍是「原图缺失」）。
    #[test]
    fn export_marks_missing_on_not_found_and_clears_on_return() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_mark_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[1u8; 12]);
        let alive = src.join("alive.jpg");
        let gone = src.join("gone.jpg");
        std::fs::write(&alive, &buf).unwrap();
        std::fs::write(&gone, &buf).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '{p1}', 0, 'alive.jpg', 'jpg', 12, 1, 1),
                (2, '{p2}', 0, 'gone.jpg', 'jpg', 12, 1, 2);",
            p1 = alive.to_string_lossy(),
            p2 = gone.to_string_lossy(),
        ))
        .unwrap();
        let missing_of = |id: i64| -> i64 {
            conn.query_row("SELECT missing FROM assets WHERE assetId=?1", [id], |r| r.get(0))
                .unwrap()
        };

        // 外部删除（模拟改名/移走/拔盘）：DB 里 missing 仍是 0 —— 正是线上「数字骗人」的形态
        std::fs::remove_file(&gone).unwrap();
        assert_eq!(missing_of(2), 0);

        let dest1 = tmp.join("out1");
        let spec1 = ExportSpec {
            source: ExportSource::Ids(vec![1, 2]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest1.clone()),
            manifest: false,
            report: None,
            strip_gps: false,
        };
        let r1 = run_export(
            &conn,
            &spec1,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r1.exported, 1);
        assert_eq!(r1.skipped.len(), 1);
        assert_eq!(r1.skipped[0].asset_id, 2);
        assert_eq!(r1.skipped[0].reason, "原图缺失", "跳过清单形状不变");
        assert_eq!(missing_of(2), 1, "导出遇 NotFound → 失败即标");
        assert_eq!(missing_of(1), 0, "读成功的资产不受影响");

        // 文件放回 → 再导出：读成功顺手清标（不需要额外核查动作）
        std::fs::write(&gone, &buf).unwrap();
        let dest2 = tmp.join("out2");
        let spec2 = ExportSpec {
            source: ExportSource::Ids(vec![1, 2]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest2.clone()),
            manifest: false,
            report: None,
            strip_gps: false,
        };
        let r2 = run_export(
            &conn,
            &spec2,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r2.exported, 2);
        assert!(r2.skipped.is_empty());
        assert_eq!(missing_of(2), 0, "文件回来了 → 清标");
        assert!(dest2.join("gone.jpg").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_export_duplicate_names_get_sequence() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_dup_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[2u8; 12]);
        for (aid, dir) in [(1, "d1"), (2, "d2"), (3, "d3")] {
            let d = src.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("same.jpg"), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, 'same.jpg', 'jpg', 12, 1, ?1)",
                rusqlite::params![aid, d.join("same.jpg").to_string_lossy()],
            )
            .unwrap();
        }
        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: false,
            report: None,
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.exported, 3);
        assert!(dest.join("same.jpg").is_file());
        assert!(dest.join("same_1.jpg").is_file());
        assert!(dest.join("same_2.jpg").is_file());
        // 二次导出到同目录 → 再追加序号（目标目录已有文件参与去重）
        let report2 = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report2.exported, 3);
        assert!(dest.join("same_3.jpg").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn zip_export_flat_with_manifest_and_missing() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_zip_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[3u8; 12]);
        std::fs::write(src.join("z1.jpg"), &buf).unwrap();
        std::fs::write(src.join("z2.jpg"), &buf).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt) VALUES
                (1, '{p1}', 0, 'z1.jpg', 'jpg', 12, 1, 1),
                (2, '{p2}', 0, 'z2.jpg', 'jpg', 12, 1, 2),
                (3, '/nope/lost.jpg', 1, 'lost.jpg', 'jpg', 12, 1, 3);",
            p1 = src.join("z1.jpg").to_string_lossy(),
            p2 = src.join("z2.jpg").to_string_lossy(),
        ))
        .unwrap();
        let zip_path = tmp.join("bundle.zip");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3]),
            content: ExportContent::Original,
            target: ExportTarget::Zip(zip_path.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.exported, 2);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.manifest_written);
        // 读回 zip：平铺 + manifest.csv
        let f = std::fs::File::open(&zip_path).unwrap();
        let mut za = zip::ZipArchive::new(f).unwrap();
        let mut names: Vec<String> = za
            .file_names()
            .map(|s| s.map(|c| c.into_owned()).unwrap_or_default())
            .collect();
        names.sort();
        assert_eq!(names, vec!["manifest.csv", "z1.jpg", "z2.jpg"]);
        let mut m = za.by_name("manifest.csv").unwrap();
        let mut csv = String::new();
        std::io::Read::read_to_string(&mut m, &mut csv).unwrap();
        assert!(csv.starts_with("fileName,tags,width,height,fileSize,absPath,assetId"));
        assert!(csv.contains("z1.jpg"));
        assert!(!csv.contains("lost.jpg"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn thumb_export_uses_generated_webp_and_lazy_gen() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_thumb_{}", std::process::id()));
        let src = tmp.join("src");
        let thumbs = tmp.join("thumbs");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n8 8\n255\n".to_vec();
        for i in 0..192u8 {
            buf.push(i.wrapping_mul(3));
        }
        std::fs::write(src.join("t1.ppm"), &buf).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, ?1, 0, 't1.ppm', 'ppm', 12, 1, 1)",
            rusqlite::params![src.join("t1.ppm").to_string_lossy()],
        )
        .unwrap();
        // 预置 256 档（导入器产物模拟），不预置 1024（触发懒生成）
        let t256 = core_storage::thumb_path(&thumbs, 1, 256);
        std::fs::create_dir_all(t256.parent().unwrap()).unwrap();
        std::fs::write(&t256, b"FAKE256").unwrap();

        let dest = tmp.join("out");
        // 256 档：直接拷现成 thumbs 文件
        let spec256 = ExportSpec {
            source: ExportSource::Ids(vec![1]),
            content: ExportContent::Thumb(256),
            target: ExportTarget::Dir(dest.clone()),
            manifest: false,
            report: None,
            strip_gps: false,
        };
        let r = run_export(
            &conn,
            &spec256,
            &thumbs,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.exported, 1);
        assert_eq!(std::fs::read(dest.join("t1_256.webp")).unwrap(), b"FAKE256");

        // 1024 档：thumbs 缺 → 从原图现场补生成（webp 头校验）
        let spec1024 = ExportSpec {
            source: ExportSource::Ids(vec![1]),
            content: ExportContent::Thumb(1024),
            target: ExportTarget::Dir(dest.clone()),
            manifest: false,
            report: None,
            strip_gps: false,
        };
        let r2 = run_export(
            &conn,
            &spec1024,
            &thumbs,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r2.exported, 1);
        let out1024 = std::fs::read(dest.join("t1_1024.webp")).unwrap();
        assert_eq!(&out1024[..4], b"RIFF"); // webp 容器头
        assert!(core_storage::thumb_path(&thumbs, 1, 1024).is_file()); // 缓存落盘
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cancel_leaves_no_partial_files() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_cancel_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[4u8; 12]);
        for aid in 1..=4 {
            std::fs::write(src.join(format!("c{aid}.jpg")), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, ?1)",
                rusqlite::params![aid, src.join(format!("c{aid}.jpg")).to_string_lossy(), format!("c{aid}.jpg")],
            )
            .unwrap();
        }
        // 目录模式：第 2 个文件后取消
        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3, 4]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let cancel = AtomicBool::new(false);
        let report = {
            let c2 = &cancel;
            let mut cb = |p: ExportProgress| {
                if p.phase == "copy" && p.done >= 1 {
                    // 第 1 个完成后置取消旗（源文件小，逐文件原子写，下一项开跑前生效）
                    c2.store(true, Ordering::SeqCst);
                }
            };
            run_export(&conn, &spec, Path::new("/nonexistent-thumbs"), c2, &mut cb).unwrap()
        };
        assert!(report.cancelled);
        assert!(report.exported >= 1 && report.exported < 4);
        assert!(!report.manifest_written); // 取消不写 manifest
                                           // 已落文件全部完整（内容等于源），无 .part 残留
        for e in std::fs::read_dir(&dest).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            assert!(!n.ends_with(".part"), "半截文件残留: {n}");
            let body = std::fs::read(e.path()).unwrap();
            assert_eq!(body.len(), buf.len());
            assert_eq!(&body[..9], b"P6\n2 2\n25");
        }
        // zip 模式：取消 → 目标 zip 与 .part 都不存在
        let zip_path = tmp.join("cx.zip");
        let spec_zip = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3, 4]),
            content: ExportContent::Original,
            target: ExportTarget::Zip(zip_path.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let cancel2 = AtomicBool::new(false);
        let mut cb2 = |p: ExportProgress| {
            if p.phase == "copy" && p.done >= 1 {
                cancel2.store(true, Ordering::SeqCst);
            }
        };
        let r2 = run_export(
            &conn,
            &spec_zip,
            Path::new("/nonexistent-thumbs"),
            &cancel2,
            &mut cb2,
        )
        .unwrap();
        assert!(r2.cancelled);
        assert!(!zip_path.exists());
        assert!(!tmp.join("cx.zip.part").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 验收主场景：标签过滤 50 图（含同名对）→ 导出原图到目录，文件数一致、重名有序号。
    #[test]
    fn acceptance_50_images_tag_filter_dir_export() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_50_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[9u8; 12]);
        for i in 1..=50i64 {
            // 每 5 张一组共用文件名 → 每组 1 个原名 + 4 个序号名
            let name = if i % 5 == 0 {
                format!("grp{}.jpg", i / 5)
            } else {
                format!("grp{}.jpg", i / 5 + 1)
            };
            let path = src.join(format!("a{i}_{name}"));
            std::fs::write(&path, &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, ?1)",
                rusqlite::params![i, path.to_string_lossy(), name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, '海', 'manual', 1.0, 1)",
                [i],
            )
            .unwrap();
        }
        let f = ImportFilter {
            tag: Some("海".into()),
            ..Default::default()
        };
        assert_eq!(count_assets(&conn, &f).unwrap(), 50);
        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Filter(f),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.total, 50);
        assert_eq!(report.exported, 50);
        assert!(report.skipped.is_empty());
        // 文件数 = 50 图 + manifest.csv
        let files: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 51, "50 图 + manifest.csv");
        assert!(files.contains(&"manifest.csv".to_string()));
        // 重名有序号：grp1.jpg / grp1_1.jpg ... grp1_4.jpg
        for n in [
            "grp1.jpg",
            "grp1_1.jpg",
            "grp1_2.jpg",
            "grp1_3.jpg",
            "grp1_4.jpg",
        ] {
            assert!(files.contains(&n.to_string()), "缺 {n}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filter_source_goes_through_query_pipeline() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_export_filter_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[5u8; 12]);
        for aid in 1..=3 {
            let path = src.join(format!("f{aid}.jpg"));
            std::fs::write(&path, &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, ?1)",
                rusqlite::params![aid, path.to_string_lossy(), format!("f{aid}.jpg")],
            )
            .unwrap();
        }
        // 标签命中 1、2；3 无标签；2 又带 missing（跳过 → missing 清单）
        conn.execute_batch(&format!(
            "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES
                (1, '海', 'manual', 1.0, 1), (2, '海', 'auto', 0.9, 1);
             UPDATE assets SET missing=1, absPath='/gone/f2.jpg' WHERE assetId=2;
             UPDATE assets SET deleted=1 WHERE assetId=3;
             INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (9, '/x/deleted.jpg', 0, 'deleted.jpg', 'jpg', 1, 1, 1);
             UPDATE assets SET deleted=1 WHERE assetId=9;"
        ))
        .unwrap();
        // count 与 resolve 同一 WHERE
        let f = ImportFilter {
            tag: Some("海".into()),
            ..Default::default()
        };
        assert_eq!(count_assets(&conn, &f).unwrap(), 2);
        let items = resolve_items(&conn, &ExportSource::Filter(f.clone())).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].tags, vec!["海"]);
        // 导出：missing 的 f2 跳过
        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Filter(f),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let r = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.exported, 1);
        assert_eq!(r.skipped[0].file_name, "f2.jpg");
        assert!(dest.join("f1.jpg").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------- C2 判定报告（manifest.csv 判定形态） ----------

    /// 内置电商包 + 4 资产（合格/修复后合格/转人工/未判）→ 列展开 + 汇总行计数正确；
    /// 软规则原始 fail（on_fail=pass）不影响结论；修复链在 fix/recheck 列可见。
    #[test]
    fn verdict_report_columns_summary_and_rows() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_vreport_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[7u8; 12]);
        for (aid, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg"), (4, "d.jpg")] {
            std::fs::write(src.join(name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![aid, src.join(name).to_string_lossy(), name],
            )
            .unwrap();
        }
        let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
        let v = |a: i64, rule: &str, result: &str, extra: NewVerdictPatch| NewVerdict {
            confidence: 0.9,
            pack_id: PACK_ID.into(),
            asset_id: a,
            job_id: 77,
            rule_id: rule.into(),
            result: result.into(),
            score: extra.score,
            evidence: extra.evidence,
            fix_action: extra.fix,
            fix_tier: extra.tier,
            recheck_result: extra.recheck,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        };
        // 1 = 合格（全 pass + 一条不影响结论的软 fail）
        for r in ["R1_size", "R2_white_bg", "R3_no_watermark", "R4_subject_ratio"] {
            repo.insert_verdict(&v(1, r, "pass", plain())).unwrap();
        }
        repo.insert_verdict(&v(
            1,
            "R5_attribute",
            "fail",
            NewVerdictPatch { evidence: Some(r#"{"on_fail":"pass"}"#.into()), ..plain() },
        ))
        .unwrap();
        // 2 = 修复后合格（R2 fail → replace_bg 修复 → 复检 pass）
        for r in ["R1_size", "R3_no_watermark", "R4_subject_ratio"] {
            repo.insert_verdict(&v(2, r, "pass", plain())).unwrap();
        }
        repo.insert_verdict(&v(
            2,
            "R2_white_bg",
            "pass",
            NewVerdictPatch {
                score: Some(3.0),
                fix: Some("replace_bg".into()),
                tier: Some("guaranteed".into()),
                recheck: Some("pass".into()),
                ..plain()
            },
        ))
        .unwrap();
        // 3 = 未判定（R1 undetermined）；4 = 完全没判 → unjudged
        repo.insert_verdict(&v(
            3,
            "R1_size",
            "undetermined",
            NewVerdictPatch { evidence: Some(r#"{"reason":"vl_timeout"}"#.into()), ..plain() },
        ))
        .unwrap();

        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3, 4]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: Some(VerdictReportSpec { pack: ec_pack(), job_id: Some(77), run_id: None }),
            strip_gps: false,
        };
        let cancel = AtomicBool::new(false);
        let r = run_export(&conn, &spec, Path::new("/nonexistent-thumbs"), &cancel, &mut |_| {})
            .unwrap();
        assert_eq!(r.exported, 4);
        assert!(r.manifest_written);
        let csv = std::fs::read_to_string(dest.join("manifest.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        // 表头 = pack detail_columns 全展开（COMPLIANCE-DOMAIN：多了 R0 领域 gate + 其置信度列）
        // + KI-15 尾列「拒选」
        assert_eq!(
            lines[0],
            "\"imageSN\",\"R0_product_main\",\"R0_product_main_confidence\",\"R1_size\",\"R2_white_bg\",\
             \"R3_no_watermark\",\"R3_no_watermark_fix\",\"R3_no_watermark_recheck\",\
             \"R4_subject_ratio\",\"R4_subject_ratio_score\",\"R5_attribute_tags\",\"拒选\""
        );
        assert_eq!(lines.len(), 4 + 2, "4 明细行 + 表头 + 汇总行");
        // 明细行 12 列（11 规则列 + 拒选尾列）
        assert_eq!(lines[1].matches("\",\"").count(), 11, "{}", lines[1]);
        // 汇总行（三态口径）：2 合格 + 0 不合格 + 1 未判定，unjudged = 1
        let s = lines[lines.len() - 1];
        assert!(s.contains("\"summary\",\"合格\",\"2\""), "{s}");
        assert!(s.contains("\"不合格\",\"0\""), "{s}");
        assert!(s.contains("\"未判定\",\"1\""), "{s}");
        assert!(s.contains("\"total\",\"4\"") && s.contains("\"unjudged\",\"1\""), "{s}");
        assert!(s.contains("\"excludedRejected\",\"0\""), "{s}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 卡验收原文：classic-only 迷你包 500 张 → 报告行数 = 表头 + 500 + 汇总 = 502。
    /// （判定行直接经 repo 灌入；队列侧 500 张全跑见 svc-pipeline::compliance_queue）
    #[test]
    fn verdict_report_500_assets_line_count() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_vreport500_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[3u8; 12]);
        let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
        for i in 1..=500i64 {
            let name = format!("f{i}.jpg");
            std::fs::write(src.join(&name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![i, src.join(&name).to_string_lossy(), name],
            )
            .unwrap();
            // 每资产 4 条 classic 判定；i%3==1 → R1 转人工
            for r in ["R1_size", "R2_white_bg", "R3_no_watermark", "R4_subject_ratio"] {
                let human = i % 3 == 1 && r == "R1_size";
                repo.insert_verdict(&NewVerdict {
                    pack_id: PACK_ID.into(),
                    asset_id: i,
                    job_id: 7,
                    rule_id: r.into(),
                    result: if human { "undetermined".into() } else { "pass".into() },
                    confidence: 0.9,
                    score: Some(1.0),
                    evidence: None,
                    fix_action: None,
                    fix_tier: None,
                    recheck_result: None,
                    judgement_scope: Some("per_asset".into()),
                    group_key: None,
                })
                .unwrap();
            }
        }
        let ids: Vec<i64> = (1..=500).collect();
        let items = resolve_items(&conn, &ExportSource::Ids(ids.clone())).unwrap();
        let rows: Vec<(ExportItem, String)> = items
            .into_iter()
            .map(|it| {
                let n = format!("f{}.jpg", it.asset_id);
                (it, n)
            })
            .collect();
        let csv = verdict_report_csv(
            &conn,
            &VerdictReportSpec { pack: ec_pack(), job_id: Some(7), run_id: None },
            &rows,
            &[],
        )
        .unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 502, "表头 + 500 明细 + 1 汇总");
        // 167 未判定（i%3==1）、333 合格、0 不合格
        let s = lines[501];
        assert!(s.contains("\"未判定\",\"167\""), "{s}");
        assert!(s.contains("\"合格\",\"333\""), "{s}");
        assert!(s.contains("\"不合格\",\"0\""), "{s}");
        assert!(s.contains("\"total\",\"500\"") && s.contains("\"unjudged\",\"0\""), "{s}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 元数据不完整 → 响亮报错（不静默输出空报告）。
    #[test]
    fn verdict_report_loud_failures() {
        let conn = setup();
        let mut pack = ec_pack();
        pack.report.detail_columns.clear();
        let err = verdict_report_csv(&conn, &VerdictReportSpec { pack, job_id: None, run_id: None }, &[], &[])
            .unwrap_err();
        assert!(err.contains("detail_columns 为空"), "{err}");
        // 未知列同样硬拒（插件包写错列名 = 报告作者错，不许出空单元格糊过去）
        let mut pack = ec_pack();
        pack.report.detail_columns.push("R9_not_a_rule".into());
        let err = verdict_report_csv(&conn, &VerdictReportSpec { pack, job_id: None, run_id: None }, &[], &[])
            .unwrap_err();
        assert!(err.contains("未知报告列"), "{err}");
    }

    struct NewVerdictPatch {
        score: Option<f64>,
        evidence: Option<String>,
        fix: Option<String>,
        tier: Option<String>,
        recheck: Option<String>,
    }

    fn plain() -> NewVerdictPatch {
        NewVerdictPatch { score: Some(1.0), evidence: None, fix: None, tier: None, recheck: None }
    }

    /// COMPLIANCE-HUMAN：报告汇总/明细都取**生效结论**（humanStatus ?? result）——
    /// 界面改判后导出的报告必须与界面一致，否则「页面上已放行，报告还写不合格」。
    #[test]
    fn verdict_report_summary_uses_human_override() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_vreport_hr_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[5u8; 12]);
        for (aid, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg")] {
            std::fs::write(src.join(name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![aid, src.join(name).to_string_lossy(), name],
            )
            .unwrap();
        }
        let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
        let v = |a: i64, result: &str| NewVerdict {
            pack_id: PACK_ID.into(),
            asset_id: a,
            job_id: 9,
            rule_id: "R1_size".into(),
            result: result.into(),
            confidence: 0.9,
            score: Some(0.2),
            evidence: None,
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        };
        repo.insert_verdict(&v(1, "fail")).unwrap();
        repo.insert_verdict(&v(2, "undetermined")).unwrap();
        repo.insert_verdict(&v(3, "pass")).unwrap();
        // 1：规则级改判 fail → pass（生效合格）
        repo.save_human_review(&core_storage::compliance::NewHumanReview {
            scheme_id: format!("pack:{PACK_ID}"),
            content_version: String::new(),
            asset_id: 1,
            rule_id: Some("R1_size".into()),
            judgement_scope: "per_asset".into(),
            group_key: String::new(),
            human_status: "pass".into(),
            human_reason: "阈值过严，人工确认尺寸达标".into(),
            machine_result: Some("fail".into()),
            machine_confidence: Some(0.9),
            reviewed_by: Some("reviewer".into()),
        })
        .unwrap();
        // 2：资产级改判 → fail（生效不合格）
        repo.save_human_review(&core_storage::compliance::NewHumanReview {
            scheme_id: format!("pack:{PACK_ID}"),
            content_version: String::new(),
            asset_id: 2,
            rule_id: None,
            judgement_scope: "per_asset".into(),
            group_key: String::new(),
            human_status: "fail".into(),
            human_reason: "整图模糊".into(),
            machine_result: Some("undetermined".into()),
            machine_confidence: Some(0.9),
            reviewed_by: None,
        })
        .unwrap();

        let items = resolve_items(&conn, &ExportSource::Ids(vec![1, 2, 3])).unwrap();
        let rows: Vec<(ExportItem, String)> = items
            .into_iter()
            .map(|it| {
                let n = format!("f{}.jpg", it.asset_id);
                (it, n)
            })
            .collect();
        let csv = verdict_report_csv(
            &conn,
            &VerdictReportSpec { pack: ec_pack(), job_id: Some(9), run_id: None },
            &rows,
            &[],
        )
        .unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 5, "表头 + 3 明细 + 汇总");
        // 明细：资产 1 的 R1_size 列给生效结论 pass（机器原值 fail 仍留在 rule_verdict）
        // 注意：列位置由 pack 的 report.detail_columns 决定（三态卡给内置包补了
        // `R0_product_main_confidence` 列，R1_size 不再是第 2 列）→ 按表头定位，别按下标硬编码。
        let header: Vec<&str> = lines[0].trim_matches('"').split("\",\"").collect();
        let col_r1 = header
            .iter()
            .position(|c| *c == "R1_size")
            .expect("detail_columns 必须含 R1_size");
        let cells: Vec<&str> = lines[1].trim_matches('"').split("\",\"").collect();
        assert_eq!(cells[0], "1", "{}", lines[1]);
        assert_eq!(
            cells[col_r1], "pass",
            "R1_size 列应给生效结论 pass，实际行：{}",
            lines[1]
        );
        // 汇总按生效结论：1 改判合格 + 3 合格 → 合格 2；2 资产级改判不合格 → 不合格 1
        let s = lines[4];
        assert!(s.contains("\"合格\",\"2\""), "{s}");
        assert!(s.contains("\"不合格\",\"1\""), "{s}");
        assert!(s.contains("\"未判定\",\"0\""), "{s}");
        assert!(s.contains("\"total\",\"3\"") && s.contains("\"unjudged\",\"0\""), "{s}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------- KI-15：拒选 = 留在库里但不交付（导出/报告口径，无开关） ----------

    /// 验收 1：拒选 2 张 → 导出选择集（含这 2 张）→ 文件不含它们，
    /// manifest 记 excludedRejected=2，报告回执一致（total=3 / exported=1 / excludedRejected=2）。
    #[test]
    fn export_excludes_rejected_records_count_in_manifest_and_report() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_ki15_export_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[7u8; 12]);
        for (aid, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg")] {
            std::fs::write(src.join(name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![aid, src.join(name).to_string_lossy(), name],
            )
            .unwrap();
        }
        // 拒选 2、3（真实 API，与图库 X 键同一落点）
        assert_eq!(crate::set_rejected(&conn, &[2, 3], true).unwrap(), 2);

        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.total, 3, "total = 选择集命中数（含拒选）");
        assert_eq!(report.exported, 1, "拒选不交付：只导出 1 个文件");
        assert_eq!(report.excluded_rejected, 2);
        assert!(report.skipped.is_empty(), "拒选不算 missing 跳过：{:?}", report.skipped);
        // 文件不含拒选资产
        assert!(dest.join("a.jpg").exists());
        assert!(!dest.join("b.jpg").exists());
        assert!(!dest.join("c.jpg").exists());
        // manifest 末行记排除数
        let csv = std::fs::read_to_string(dest.join("manifest.csv")).unwrap();
        assert_eq!(csv.lines().last().unwrap(), "excludedRejected,\"2\"", "{csv}");
        assert!(!csv.contains("b.jpg") && !csv.contains("c.jpg"));
        // 报告回执 JSON（原始形态，与前端 api.ts ExportReport 同形）
        let j = serde_json::to_value(&report).unwrap();
        println!("KI15-验收1 报告回执: {}", serde_json::to_string_pretty(&j).unwrap());
        assert_eq!(j["excludedRejected"], serde_json::json!(2), "{j}");
        assert_eq!(j["total"], serde_json::json!(3), "{j}");
        assert_eq!(j["exported"], serde_json::json!(1), "{j}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 验收 2：判定报告——拒选资产不进汇总计数（不进交付口径），明细行带「已拒选」标。
    #[test]
    fn verdict_report_excludes_rejected_from_summary_and_marks_details() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_ki15_vreport_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[7u8; 12]);
        for (aid, name) in [(1, "a.jpg"), (2, "b.jpg"), (3, "c.jpg")] {
            std::fs::write(src.join(name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![aid, src.join(name).to_string_lossy(), name],
            )
            .unwrap();
        }
        let repo = core_storage::compliance_sqlite::SqliteComplianceRepo::new(&conn);
        let v = |a: i64, rule: &str, result: &str| NewVerdict {
            confidence: 0.9,
            pack_id: PACK_ID.into(),
            asset_id: a,
            job_id: 77,
            rule_id: rule.into(),
            result: result.into(),
            score: None,
            evidence: None,
            fix_action: None,
            fix_tier: None,
            recheck_result: None,
            judgement_scope: Some("per_asset".into()),
            group_key: None,
        };
        // 三张全判 R1 pass（都是合格），其中 2、3 拒选
        for a in [1i64, 2, 3] {
            repo.insert_verdict(&v(a, "R1_size", "pass")).unwrap();
        }
        assert_eq!(crate::set_rejected(&conn, &[2, 3], true).unwrap(), 2);

        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Ids(vec![1, 2, 3]),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: Some(VerdictReportSpec { pack: ec_pack(), job_id: Some(77), run_id: None }),
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.exported, 1, "拒选不交付：只导出 1 个文件");
        assert_eq!(report.excluded_rejected, 2);

        let csv = std::fs::read_to_string(dest.join("manifest.csv")).unwrap();
        println!("KI15-验收2 判定报告:\n{csv}");
        let lines: Vec<&str> = csv.lines().collect();
        // 明细仍列出全部 3 张（含拒选），按 assetId 排序；表头带「拒选」尾列
        assert_eq!(lines.len(), 3 + 2, "表头 + 3 明细 + 汇总");
        assert!(lines[0].ends_with("\"拒选\""), "{}", lines[0]);
        let mark_col = |line: &str| line.rsplit(',').next().unwrap().to_string();
        assert_eq!(mark_col(lines[1]), "\"\"", "交付行尾列为空：{}", lines[1]);
        assert_eq!(mark_col(lines[2]), "\"已拒选\"", "拒选行带「已拒选」标：{}", lines[2]);
        assert_eq!(mark_col(lines[3]), "\"已拒选\"", "拒选行带「已拒选」标：{}", lines[3]);
        // 汇总：只数交付项——合格 1（不是 3）、total 1、excludedRejected 2
        let s = lines[4];
        assert!(s.contains("\"合格\",\"1\""), "拒选不进汇总：{s}");
        assert!(s.contains("\"total\",\"1\"") && s.contains("\"unjudged\",\"0\""), "{s}");
        assert!(s.contains("\"excludedRejected\",\"2\""), "{s}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Filter 来源同口径：标签面板全量命中里的拒选资产同样排除（不新增筛选入口）。
    #[test]
    fn filter_export_excludes_rejected_too() {
        let conn = setup();
        let tmp = std::env::temp_dir().join(format!("dam_ki15_filter_{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut buf = b"P6\n2 2\n255\n".to_vec();
        buf.extend_from_slice(&[7u8; 12]);
        for (aid, name) in [(1, "a.jpg"), (2, "b.jpg")] {
            std::fs::write(src.join(name), &buf).unwrap();
            conn.execute(
                "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (?1, ?2, 0, ?3, 'jpg', 12, 1, 1)",
                rusqlite::params![aid, src.join(name).to_string_lossy(), name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tag (assetId, value, origin, score, createdAt) VALUES (?1, '海', 'auto', 1.0, 1)",
                rusqlite::params![aid],
            )
            .unwrap();
        }
        assert_eq!(crate::set_rejected(&conn, &[2], true).unwrap(), 1);

        let dest = tmp.join("out");
        let spec = ExportSpec {
            source: ExportSource::Filter(ImportFilter { tag: Some("海".into()), ..Default::default() }),
            content: ExportContent::Original,
            target: ExportTarget::Dir(dest.clone()),
            manifest: true,
            report: None,
            strip_gps: false,
        };
        let report = run_export(
            &conn,
            &spec,
            Path::new("/nonexistent-thumbs"),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.exported, 1);
        assert_eq!(report.excluded_rejected, 1);
        assert!(dest.join("a.jpg").exists() && !dest.join("b.jpg").exists());
        let csv = std::fs::read_to_string(dest.join("manifest.csv")).unwrap();
        assert!(csv.lines().any(|l| l == "excludedRejected,\"1\""), "{csv}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

}
