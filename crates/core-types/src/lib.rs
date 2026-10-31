//! L0 `core-types` — 全部领域结构体 + 错误类型的唯一共享层（PLAN.md §2.1）。
//!
//! D2 落地：Asset/Resource/Tag/ImportBatch/ImportProgress/WatchEvent/
//! LibraryStats/ImportFilter（字段名与 SQLite 列名一致 camelCase，serde 直通前端）。
//! L2 同层互引禁止 → 跨 svc crate 共享的结构一律定义在此。

use serde::{Deserialize, Serialize};

/// assets 表读模型（exif json 不进 v1 读模型）。
#[derive(Debug, Clone, Serialize)]
pub struct AssetRecord {
    pub assetId: i64,
    pub mediaType: String,
    pub absPath: String,
    pub storeMode: String,
    pub missing: i64,
    pub fileName: String,
    pub fileExt: String,
    pub fileSz: i64,
    pub mtime: i64,
    pub fileHash: Option<String>,
    pub titleCh: Option<String>,
    pub titleEn: Option<String>,
    pub description: Option<String>,
    pub source: String,
    pub parentAssetId: Option<i64>,
    pub searchable: i64,
    pub deleted: i64,
    pub approvalStatus: String,
    pub starRating: i64,
    pub colorLabel: Option<String>,
    pub cloudAssetId: Option<String>,
    pub importedAt: i64,
    /// 嵌入状态机（D3）：none/pending/done/failed
    pub embStatus: String,
    /// 产出该向量的 provider spec.name（禁混算锚点）
    pub embModel: Option<String>,
    /// 去重向量状态机（D5 双嵌入器）：none/pending/done/failed
    pub dedupStatus: String,
    /// 产出该去重向量的 provider spec.name
    pub dedupModel: Option<String>,
    // ---- EXIF2 派生列（真值在 exif json；None = 未解析/无该元数据） ----
    /// 拍摄时间（unix 秒；无 EXIF 拍摄时间不编造，保持 NULL）
    pub takenAt: Option<i64>,
    pub cameraMake: Option<String>,
    pub cameraModel: Option<String>,
    pub lensModel: Option<String>,
    pub iso: Option<i64>,
    pub focalLenMm: Option<f64>,
    pub aperture: Option<f64>,
    pub shutterText: Option<String>,
    /// EXIF Orientation 原值 1..8
    pub orientation: Option<i64>,
    /// w/h（3 位小数，按 Orientation 摆正后）；解码失败 NULL
    pub aspectRatio: Option<f64>,
    /// landscape|portrait|square（±2% 方图容差）
    pub aspectClass: Option<String>,
    /// GPS 仅在显式开启档位后才有值（默认 NULL）
    pub gpsLat: Option<f64>,
    pub gpsLon: Option<f64>,
    pub gpsSource: Option<String>,
    /// exif json 真值摘要（含 parser/失败原因）
    pub exif: Option<String>,
}

/// resource 表读模型（检索最小单元：primary/frame/page/region）。
#[derive(Debug, Clone, Serialize)]
pub struct ResourceRecord {
    pub resourceId: i64,
    pub assetId: i64,
    pub role: String,
    pub ordinal: i64,
    pub thumbPath: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub pHash: Option<String>,
}

/// tag 表读模型（标签值集合：只有 value，无维度名）。
#[derive(Debug, Clone, Serialize)]
pub struct TagRecord {
    pub id: i64,
    pub assetId: i64,
    /// 标签值（唯一标签实体；多来源同值只留一行）
    pub value: String,
    /// 来源：auto(模板打标) / manual(人工) / path(目录路径) / custom(自定义任务模型)
    pub origin: String,
    /// 产出该标签的模型（manual/path 为 NULL）
    pub model: Option<String>,
    /// 产出该标签的自定义任务（仅 origin='custom'；结果表 tag_result 保留全量溯源）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taskId: Option<i64>,
    pub score: f64,
    pub createdAt: i64,
}

/// 打标任务结果表读模型：name=维度名（任务名快照），value=模型输出原文。
#[derive(Debug, Clone, Serialize)]
pub struct TagResultRecord {
    pub id: i64,
    pub assetId: i64,
    pub taskId: Option<i64>,
    /// 维度名（custom_tag_task.name 快照）
    pub name: String,
    /// 模型原始输出（多值不拆分；拆出的标签见 tag 表）
    pub value: String,
    pub model: Option<String>,
    pub score: f64,
    pub createdAt: i64,
    pub updatedAt: i64,
}

/// 列表项：asset + primary resource 扁平化（UI 网格一次拿全）。
#[derive(Debug, Clone, Serialize)]
pub struct AssetView {
    #[serde(flatten)]
    pub asset: AssetRecord,
    pub thumbPath: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub pHash: Option<String>,
}

/// import_batches 表读模型。
#[derive(Debug, Clone, Serialize)]
pub struct ImportBatchRecord {
    pub batchId: i64,
    pub rootPath: String,
    pub storeMode: String,
    pub status: String,
    pub totalFiles: i64,
    pub doneFiles: i64,
    pub error: Option<String>,
    pub startedAt: i64,
    pub endedAt: Option<i64>,
}

/// 单文件导入失败明细（任务中心「失败明细 / 重试失败项」用）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportFailure {
    /// 文件名（不含目录，界面列表展示用）
    pub fileName: String,
    /// 绝对路径（界面上「重试失败项」回传后端；也让日志可定位）
    pub path: String,
    /// 失败原因原文（不含路径前缀，便于按类归并/去重展示）
    pub error: String,
}

/// 失败明细上限：一批可能有成百上千张失败，进度/汇总 payload 只带前 N 条。
/// `failed` 计数永远是全量；界面用计数做标题、用列表做前 N 条证据。
pub const MAX_FAILED_ITEMS: usize = 200;

/// 导入进度（事件 payload：app-tauri emit "import:progress"）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportProgress {
    pub batchId: i64,
    pub status: String,
    pub done: i64,
    pub total: i64,
    pub imported: i64,
    pub duplicates: i64,
    pub failed: i64,
    pub lastError: Option<String>,
    pub currentPath: Option<String>,
    /// 累计失败明细（按处理序；上限 MAX_FAILED_ITEMS）
    #[serde(default)]
    pub failedItems: Vec<ImportFailure>,
    /// E5 导入即图库：尚未处理到的文件名（按处理序 = 扫描序）。
    /// 前端渲染为网格里的半透明占位卡，缩略图就绪（资产落库）即点亮。
    /// v1 单机库量级直接全量带出；十万级批次再考虑分页/截断。
    #[serde(default)]
    pub pendingNames: Vec<String>,
}

/// 一批次跑完的汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BatchOutcome {
    pub batchId: i64,
    pub status: String,
    pub total: i64,
    pub imported: i64,
    pub duplicates: i64,
    pub unchanged: i64,
    pub updated: i64,
    pub failed: i64,
    /// 失败明细（与 `failed` 同源，封顶 MAX_FAILED_ITEMS）——任务中心据此列出
    /// 「哪张失败、为什么」，不用再去翻日志；空列表 ⇒ 无失败。
    #[serde(default)]
    pub failedItems: Vec<ImportFailure>,
}

/// C6 合规状态过滤（库视图「合规」维度）：packId + 状态词。
///
/// 状态词 = 单资产合规状态（canonical：待人工 > 不合格 > 修复后合格 > 合格 > 未判定），
/// 由 svc-library 按最新判定口径翻译成 SQL（见 `svc_library::compliance_view`）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ComplianceStateFilter {
    /// 判定所属 pack（= 当前激活包 id）
    pub packId: String,
    /// pass | fixed | fail | pending_human | unjudged
    pub state: String,
}

/// C9 组级判定（group_by）允许的 `asset_field` 组键列（白名单，唯一真相）。
///
/// 放 core-types 的原因：pack schema（core-config，L0）要按它校验 YAML 里的
/// `group_by: {source: asset_field, field: X}`，而落 SQL 取值的 core-storage（同样 L0）
/// 也要按它挡列名注入 —— 两个 L0 crate 禁止互引，故收敛到「任何层可用」的共享类型层。
/// 取值 = assets 表列名本体（不另发明字段名；都是 0013 的 EXIF 派生列或既有列）。
pub const GROUP_ASSET_FIELDS: &[&str] = &[
    "cameraMake",
    "cameraModel",
    "lensModel",
    "aspectClass",
    "source",
];

/// `asset_field` 组键列是否在白名单内。
pub fn group_asset_field_ok(field: &str) -> bool {
    GROUP_ASSET_FIELDS.contains(&field)
}

/// 组键列的已知取值清单（错误信息用）。
pub fn group_asset_fields_joined() -> String {
    GROUP_ASSET_FIELDS.join(", ")
}

/// 标签查询入口归一化（task）：trim 首尾空白 + 内部空白串折叠成 `_`。
///
/// 统一标签的值形如 `尺寸-滚筒_10KG`（词间用下划线），用户输入 `滚筒 10KG` 应能对上；
/// 首尾空格/大小写不影响命中（B07-2④）。大小写不在字符串层折叠（`to_lowercase` 会
/// 把每个查询值改写、且 SQLite 侧用 `COLLATE NOCASE` 才能吃到 nocase 索引），在 SQL 处理。
pub fn normalize_tag_query(input: &str) -> String {
    let t = input.trim();
    let mut out = String::with_capacity(t.len());
    let mut in_ws = false;
    for c in t.chars() {
        if c.is_whitespace() {
            in_ws = true;
            continue;
        }
        if in_ws && !out.is_empty() {
            out.push('_');
        }
        in_ws = false;
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tag_query_tests {
    use super::normalize_tag_query;

    #[test]
    fn trims_and_collapses_whitespace_runs() {
        assert_eq!(normalize_tag_query("  尺寸-滚筒_10KG  "), "尺寸-滚筒_10KG");
        assert_eq!(normalize_tag_query("滚筒 10KG"), "滚筒_10KG");
        assert_eq!(normalize_tag_query("a \t b   c"), "a_b_c");
        assert_eq!(normalize_tag_query("   "), "");
        assert_eq!(normalize_tag_query(""), "");
    }
}

/// svc-library 查询过滤（前端 → Tauri command）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImportFilter {
    pub mediaType: Option<String>,
    /// None=不过滤；Some(1)=只看失联；Some(0)=只看正常
    pub missing: Option<i64>,
    pub ext: Option<String>,
    pub tag: Option<String>,
    pub dirPrefix: Option<String>,
    /// W2 web 图库：Some(true)=只看无任何标签的资产
    pub untagged: Option<bool>,
    /// E1 筛选芯片：星级下限（starRating >= n）
    pub minRating: Option<i64>,
    /// E3 智能文件夹：颜色标签精确匹配（red/orange/yellow/green/blue/purple）
    pub colorLabel: Option<String>,
    /// E4：按 assetId 集合取（相似结果临时集合 / 批量编辑回读），≤900 个
    pub ids: Option<Vec<i64>>,
    /// EXIF2：拍摄时间范围（unix 秒，含端点）
    pub takenFrom: Option<i64>,
    pub takenTo: Option<i64>,
    /// EXIF2：相机/镜头型号精确匹配（厂商拼写原样）
    pub cameraModel: Option<String>,
    pub lensModel: Option<String>,
    /// EXIF2：ISO 范围
    pub isoMin: Option<i64>,
    pub isoMax: Option<i64>,
    /// EXIF2：画幅过滤 landscape|portrait|square（走 aspectClass 派生列，禁全表表达式）
    pub aspect: Option<String>,
    /// 排序键：缺省 importedAt（新→旧）；"takenAt" = 拍摄时间（NULL 殿后）
    pub sort: Option<String>,
    /// D3 无法预览：None=不过滤；Some(1)=只看解码失败（无尺寸）的资产；Some(0)=只看可预览
    pub broken: Option<i64>,
    /// C6 合规状态维度（None = 不过滤；非法状态词 → 空集，HTTP 层已 400）
    pub compliance: Option<ComplianceStateFilter>,
    /// C7 关键词全库检索：文件名 / 标题（中英）/ 描述 / 路径 的大小写不敏感子串匹配。
    /// None 或空白串 = 不过滤（与前端「空串不发送」同口径）。
    ///
    /// 参数名口径：HTTP 形态叫 `q`（app-web `AssetQuery::q`），导出 payload 同源也用 `q`
    /// —— 这里 `alias` 让两条入口共用同一个字段，避免「图库搜了 2 张、导出却是整库」。
    #[serde(alias = "q")]
    pub query: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// E3 智能文件夹（保存搜索）：一组命名筛选条件 + 侧栏持久入口。
/// query 为 AssetQuery 形态 JSON（见 core-storage::smart_folders 白名单），结果动态现查。
#[derive(Debug, Clone, Serialize)]
pub struct SmartFolder {
    pub id: i64,
    pub name: String,
    pub query: String,
    pub createdAt: i64,
    pub updatedAt: i64,
}

/// 库统计（状态栏/侧栏）。
///
/// DESKTOP-MISSING-STAT-1：`missing` 是**上次现场核查的结果**，不是实时值 ——
/// 必须连同 `missingCheckedAt`/`missingStale` 一起展示，单看 `missing` 会撒谎
/// （源目录被外部清空后仍报 0）。UI 用新鲜度标记 + 「重新核查」显式重扫。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LibraryStats {
    pub assets: i64,
    pub missing: i64,
    pub tagged: i64,
    pub totalBytes: i64,
    pub byMediaType: Vec<MediaTypeCount>,
    /// 上次全库失联现场核查时间（unixepoch 秒）；None = 从未核查
    pub missingCheckedAt: Option<i64>,
    /// 统计是否已过期：从未核查 || 距今超过 MISSING_CHECK_STALE_SECS
    pub missingStale: bool,
    /// 上次核查里「存在但读不了」的数量（不折进 missing，但也不吞）
    pub missingUnreadable: i64,
    /// D3 无法预览：解码失败（无尺寸）的资产数 —— 与 `/api/library/assets?broken=1`
    /// 同一个判定口径（`svc_library::BROKEN_COND`），不是两套算法
    pub broken: i64,
    /// 新鲜度阈值（秒），随响应下发，前端提示文案用同一口径
    pub missingStaleSecs: i64,
}

/// settings 表里「上次失联核查」的 key（svc-import 写、svc-library 读）。
pub const MISSING_CHECK_KEY: &str = "missing_check";

/// 失联统计新鲜度阈值（秒）：超过它 stats.missingStale=true，UI 提示重新核查。
/// 取值依据见 reports/MISSING-STAT-1.md 的 10k 库实测（全量核查 ~O(资产数·stat)）。
pub const MISSING_CHECK_STALE_SECS: i64 = 600;

/// 上次全库失联现场核查的记录（落 settings，跨进程/重启保留）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MissingCheckRecord {
    pub at: i64,
    pub durationMs: i64,
    pub checked: i64,
    pub notFound: i64,
    pub unreadable: i64,
    pub missing: i64,
    pub recovered: i64,
    /// D3：本次核查发现「文件在但解不开」的数量（落库后可被 ?broken=1 筛出）
    #[serde(default)]
    pub broken: i64,
    /// D3：本次核查里解码成功、并已修复尺寸/缩略图的资产数（文件被换回过）
    #[serde(default)]
    pub repaired: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaTypeCount {
    pub mediaType: String,
    pub count: i64,
}

/// watcher 事件（事件 payload：app-tauri emit "watch:event"）。
#[derive(Debug, Clone, Serialize)]
pub struct WatchEvent {
    /// imported / missing / moved / error
    pub kind: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assetId: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------- D3 推理接入层读模型（前端设置页 / 诊断） ----------

/// 单模型文件的落盘状态（present/sha256 为实测值，expectedSha256 为 manifest 锁定值）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelFileStatus {
    pub name: String,
    pub urlPath: String,
    pub present: bool,
    pub bytes: u64,
    pub expectedBytes: u64,
    pub expectedSha256: String,
    /// 只在 verify 时回填
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaOk: Option<bool>,
}

/// 模型目录整体状态。
#[derive(Debug, Clone, Serialize)]
pub struct ModelStatus {
    pub name: String,
    pub dir: String,
    /// 文件齐全（不含 sha 校验）
    pub complete: bool,
    /// 全量 sha256 校验结果（None = 未校验）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    pub totalBytes: u64,
    pub presentBytes: u64,
    pub files: Vec<ModelFileStatus>,
}

/// embedding provider 运行时信息。
#[derive(Debug, Clone, Serialize)]
pub struct EmbedderInfo {
    pub name: String,
    pub provider: String,
    pub dims: u32,
    pub modalities: Vec<String>,
    pub normalized: bool,
    /// 模型已加载（可推理）
    pub loaded: bool,
    /// 未就绪原因（缺模型文件 / 未配置 endpoint 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 打标 provider 运行时信息（未配置 → enabled=false, configured=false）。
#[derive(Debug, Clone, Serialize)]
pub struct TaggerInfo {
    pub name: String,
    pub enabled: bool,
    pub configured: bool,
    /// provider 预设 id（D9）：dashscope / zhipu / ollama / custom
    pub provider: String,
    pub baseUrl: String,
    pub model: String,
}

/// 嵌入状态机计数。
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmbStats {
    pub none: i64,
    pub pending: i64,
    pub done: i64,
    pub failed: i64,
}

/// 设置页顶部总览（单次调用拿全）。
#[derive(Debug, Clone, Serialize)]
pub struct InferenceState {
    pub appDir: String,
    pub modelsRoot: String,
    pub activeEmbedder: String,
    pub modelBaseUrl: String,
    pub autoDownload: bool,
    pub embedder: EmbedderInfo,
    pub model: ModelStatus,
    pub tagger: TaggerInfo,
    pub embStats: EmbStats,
    pub downloadInProgress: bool,
}

/// 模型下载/校验进度（事件 payload：app-tauri emit "models:progress"）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelProgress {
    /// downloading / verifying / done / failed / skipped / cancelled
    pub status: String,
    pub model: String,
    pub file: String,
    pub doneBytes: u64,
    pub totalBytes: u64,
    pub completedFiles: usize,
    pub totalFiles: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 打标连通性测试结果。
#[derive(Debug, Clone, Serialize)]
pub struct TagTestResult {
    pub ok: bool,
    pub model: String,
    pub latencyMs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 单图打标落库结果（asset 标题/描述 + tag 行）。
#[derive(Debug, Clone, Serialize)]
pub struct TagApplyResult {
    pub assetId: i64,
    pub titleCh: String,
    pub titleEn: String,
    pub description: String,
    pub tagsInserted: usize,
    /// 输出被 max_tokens 截断、靠结构补齐救回（标签可能少于模板要求）
    #[serde(default)]
    pub repaired: bool,
}

// ---------- D4：自动打标队列读模型（svc-pipeline::tag_queue 的 serde 形状） ----------

/// 打标队列统计（UI 状态条）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct TagQueueStats {
    pub done: i64,
    pub failed: i64,
    pub pending: i64,
    /// 待入队（无 auto 标签的图片资产）
    pub untaggedAssets: i64,
}

/// 打标失败 job 明细（UI 失败列表）。
#[derive(Debug, Clone, Serialize)]
pub struct TagFailedItem {
    pub jobId: i64,
    pub assetId: i64,
    pub attempts: i64,
    pub lastError: String,
    /// 失败归类：`truncated`（max_tokens 截断）/ `other`（网络/上游/非法输出等）。
    /// UI 按类分桶重试，避免把网络错的 job 和「预算给小了」的 job 混在一起。
    #[serde(default)]
    pub errorClass: String,
}

/// EXIF2 回填队列运行报告（口径与 TagRunReport 一致）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExifRunReport {
    pub processed: usize,
    /// 本轮完成（含「确认无 EXIF」——那也是一次有效解析结论，记原因后 done）
    pub done: usize,
    /// 终态失败（重试 3 次后，如文件失联）
    pub failed: usize,
    pub retry: usize,
    /// 资产已删/非图片等跳过
    pub skipped: usize,
    pub cancelled: bool,
    pub errors: Vec<String>,
    pub jobDone: i64,
    pub jobFailed: i64,
    pub jobPending: i64,
}

/// 一轮打标队列的汇总（"tag:done" 事件 + 命令返回）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct TagRunReport {
    pub processed: usize,
    pub done: usize,
    pub failed: usize,
    /// 重试中（未终态）job 数
    pub retry: usize,
    /// 非图片/已删/失联等直接跳过
    pub skipped: usize,
    /// 输出被截断后靠结构补齐成功的张数（标签可能少于模板要求 → 提示调大 max_tokens）
    #[serde(default)]
    pub repaired: usize,
    pub cancelled: bool,
    /// 失败明细（最多 50 条）
    pub errors: Vec<String>,
    /// 跑完后的队列统计
    pub jobDone: i64,
    pub jobFailed: i64,
    pub jobPending: i64,
}

/// 逐 job 打标进度（"tag:progress" 事件）。
#[derive(Debug, Clone, Serialize)]
pub struct TagProgress {
    pub jobId: i64,
    pub assetId: i64,
    /// done / failed / retry / skipped
    pub status: String,
    pub index: usize,
    pub total: usize,
    pub doneCount: usize,
    pub failedCount: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------- OCR-E：文字识别队列读模型（svc-pipeline::ocr_queue 的 serde 形状） ----------

/// OCR 队列统计（UI 状态条）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct OcrQueueStats {
    pub done: i64,
    pub failed: i64,
    pub pending: i64,
    /// 待入队（无 ocr_text 的图片资产）
    pub noOcrAssets: i64,
}

/// OCR 失败 job 明细（UI 失败列表）。
#[derive(Debug, Clone, Serialize)]
pub struct OcrFailedItem {
    pub jobId: i64,
    pub assetId: i64,
    pub attempts: i64,
    pub lastError: String,
}

/// 一轮 OCR 队列的汇总（命令返回）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct OcrRunReport {
    pub processed: usize,
    pub done: usize,
    pub failed: usize,
    /// 重试中（未终态）job 数
    pub retry: usize,
    /// 非图片/已删/失联/模型缺文件等直接跳过
    pub skipped: usize,
    pub cancelled: bool,
    /// 失败明细（最多 50 条）
    pub errors: Vec<String>,
    /// 跑完后的队列统计
    pub jobDone: i64,
    pub jobFailed: i64,
    pub jobPending: i64,
}

/// 逐 job OCR 进度。
#[derive(Debug, Clone, Serialize)]
pub struct OcrProgress {
    pub jobId: i64,
    pub assetId: i64,
    /// done / failed / retry / skipped
    pub status: String,
    pub index: usize,
    pub total: usize,
    pub doneCount: usize,
    pub failedCount: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------- D10：自定义打标任务（core-storage::custom_tasks / svc-pipeline::custom_queue） ----------

/// 自定义打标任务（打标维度）：prompt 经 inline 通道逐图推理，结果写 tag(origin='custom')。
#[derive(Debug, Clone, Serialize)]
pub struct CustomTagTask {
    pub id: i64,
    /// 维度名（= tag_result.name，库内唯一；不是标签本身）
    pub name: String,
    /// 用户提示词（inline 通道原文，运行时再拼输出约束）
    pub prompt: String,
    pub enabled: bool,
    pub createdAt: i64,
    pub updatedAt: i64,
    /// 该任务 job_queue(kind='custom_tag') 计数：pending+running
    pub jobPending: i64,
    pub jobDone: i64,
    pub jobFailed: i64,
    /// 最近一次终态 job 的 updatedAt（unix 秒；None=从未跑过）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastRunAt: Option<i64>,
}

/// 自定义打标失败 job 明细（UI 失败列表，带任务名）。
#[derive(Debug, Clone, Serialize)]
pub struct CustomTagFailedItem {
    pub jobId: i64,
    pub taskId: i64,
    pub taskName: String,
    pub assetId: i64,
    pub attempts: i64,
    pub lastError: String,
}

// ---------- D5：嵌入队列读模型（svc-pipeline::embed_queue 的 serde 形状） ----------

/// 单侧（语义/去重）嵌入队列统计。
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmbedScopeStats {
    pub done: i64,
    pub failed: i64,
    pub pending: i64,
    /// 待入队（状态 none/failed 的合格图片资产）
    pub remaining: i64,
    /// 向量表行数（已落库向量）
    pub vectors: i64,
}

/// 双塔嵌入队列统计（UI 状态条）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmbedQueueStats {
    /// 语义塔（siglip，随 settings.active_embedder）
    pub semantic: EmbedScopeStats,
    /// 去重塔（dinov3 固定）
    pub dedup: EmbedScopeStats,
}

/// 嵌入失败 job 明细（UI 失败列表）。
#[derive(Debug, Clone, Serialize)]
pub struct EmbedFailedItem {
    pub jobId: i64,
    pub assetId: i64,
    pub model: String,
    pub attempts: i64,
    pub lastError: String,
}

/// 一轮嵌入队列汇总（"embed:done" 事件 + 命令返回）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct EmbedRunReport {
    pub scope: String,
    pub model: String,
    pub processed: usize,
    pub done: usize,
    pub failed: usize,
    pub retry: usize,
    pub skipped: usize,
    pub cancelled: bool,
    pub errors: Vec<String>,
    pub jobDone: i64,
    pub jobFailed: i64,
    pub jobPending: i64,
}

/// 逐 job 嵌入进度（"embed:progress" 事件）。
#[derive(Debug, Clone, Serialize)]
pub struct EmbedProgress {
    pub jobId: i64,
    pub assetId: i64,
    /// semantic / dedup
    pub scope: String,
    pub model: String,
    /// done / failed / retry / skipped
    pub status: String,
    pub index: usize,
    pub total: usize,
    pub doneCount: usize,
    pub failedCount: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------- D11：图片集导出（svc-library::export 的 IPC 形状，裁决 2026-10-13） ----------

/// 导出请求（前端 → Tauri command "export_start"）。
/// 来源二选一：显式 assetIds（批量选择集）或 filter（标签面板全量命中，走 svc-library 查询管道）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSpecParams {
    pub asset_ids: Option<Vec<i64>>,
    pub filter: Option<ImportFilter>,
    /// original / thumb256 / thumb1024
    pub content: String,
    /// 目录拷贝模式：目标目录（与 zipPath 二选一）
    pub dest_dir: Option<String>,
    /// zip 模式：目标 .zip 文件路径
    pub zip_path: Option<String>,
    pub manifest: bool,
}

/// 导出进度（"export:progress" 事件）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
    /// resolve（解析选择集）/ copy（逐文件写）/ manifest（写清单）/ finalize（zip 收尾）
    pub phase: String,
    pub done: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// 单条跳过记录（missing 清单）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSkipped {
    pub asset_id: i64,
    pub file_name: String,
    /// 原图缺失 / 缩略图无法生成 等
    pub reason: String,
}

/// 导出完成报告（"export:done" 事件）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportReport {
    /// 选择集命中的资产总数
    pub total: usize,
    /// 成功写出的文件数
    pub exported: usize,
    /// KI-15：选择集内 approvalStatus=rejected 被排除的张数（拒选 = 不交付，无开关）
    pub excluded_rejected: usize,
    /// missing 跳过清单
    pub skipped: Vec<ExportSkipped>,
    pub errors: Vec<String>,
    pub cancelled: bool,
    pub dest: String,
    pub manifest_written: bool,
}
