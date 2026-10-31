//! L2 `svc-import` — 导入编排：扫描 → 入库去重（fileHash）→ 缩略图 → pHash →
//! 批次断点续跑（PLAN.md M1）。
//!
//! v1 只走 image 白名单（IMAGE_EXTS）；storeMode=reference（原地索引，默认）或
//! copyin（拷贝入库目录）。每文件处理幂等：以 absPath+fileHash+mtime+size 判定
//! unchanged/duplicate/updated，批次崩溃后可重跑（幂等分类跳过已完成文件）。

use std::path::{Path, PathBuf};

use core_types::{BatchOutcome, ImportBatchRecord, ImportProgress};
use rusqlite::Connection;
use serde::Serialize;

pub mod path_tags;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("media: {0}")]
    Media(#[from] core_media::MediaError),
    #[error("path not absolute: {0}")]
    NotAbsolute(String),
    #[error("not a directory: {0}")]
    NotDir(String),
    #[error("copy-in target inside source tree: {0}")]
    RecursiveCopy(String),
    #[error("unsupported file: {0}")]
    Unsupported(String),
    #[error("path outside batch root: {0}")]
    OutsideBatchRoot(String),
    /// D3：文件扩展名在白名单里但内容解不出像素（不是图片/已损坏）。
    /// 必须在**入库前**响亮失败 —— 否则会落成一张 width=0、缩略图永远 500 的假资产。
    #[error("无法解码为图片（不是有效图片或已损坏）: {0}")]
    BadImage(String),
}

/// v1 图片扩展名白名单（小写，无点）。
pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif", "heic"];

/// mediaType 路由：白名单→image，其余→None（v1 跳过）。
pub fn media_type_of(ext: &str) -> Option<&'static str> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    if IMAGE_EXTS.contains(&e.as_str()) {
        Some("image")
    } else {
        None
    }
}

/// 递归扫描目录收集图片文件（跳过隐藏/系统目录）。
pub fn scan_dir(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().map(|e| e.to_string_lossy().to_string()) {
                if media_type_of(&ext) == Some("image") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 单文件处理结果分类（导入语义核心）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOutcome {
    /// 新资产入库（assetId）
    Imported(i64),
    /// 同 fileHash 已存在（重复，跳过；返回已存在的 assetId）
    Duplicate(i64),
    /// 同 absPath 且 fileHash+mtime+size 未变（重扫/续跑时跳过实际工作）
    Unchanged(i64),
    /// 同 absPath 但内容变了 → 更新记录 + 重生成缩略图/pHash
    Updated(i64),
    /// 处理失败（错误信息转 String，Copy 语义）
    Failed,
}

/// 缩略图路径：thumbs/<size>/<assetId%100 两位分片>/<assetId>_<size>.webp
/// （D11 上移 core-storage::thumb_path，此处转发保兼容）
pub fn thumb_path(thumbs_root: &Path, asset_id: i64, size: i32) -> PathBuf {
    core_storage::thumb_path(thumbs_root, asset_id, size)
}

/// 新建导入批次（status=running，totalFiles 由调用方先 scan 得出）。
pub fn create_batch(
    conn: &Connection,
    root: &Path,
    store_mode: &str,
    total_files: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO import_batches (rootPath, storeMode, status, totalFiles, doneFiles, startedAt)
         VALUES (?1, ?2, 'running', ?3, 0, unixepoch())",
        rusqlite::params![root.to_string_lossy(), store_mode, total_files],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_batch(conn: &Connection, batch_id: i64, done: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE import_batches SET doneFiles=?2 WHERE batchId=?1",
        rusqlite::params![batch_id, done],
    )?;
    Ok(())
}

pub fn finish_batch(
    conn: &Connection,
    batch_id: i64,
    status: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE import_batches SET status=?2, error=?3, endedAt=unixepoch() WHERE batchId=?1",
        rusqlite::params![batch_id, status, error],
    )?;
    Ok(())
}

/// W6 修复（task）：批次级 autoTag 标记（导入完成后自动入队打标）。
/// 只由 web 导入向导写入；桌面版不受影响（默认 0）。
pub fn set_batch_auto_tag(
    conn: &Connection,
    batch_id: i64,
    auto_tag: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE import_batches SET autoTag=?2 WHERE batchId=?1",
        rusqlite::params![batch_id, auto_tag as i64],
    )?;
    Ok(())
}

/// 读批次 autoTag（断点续跑恢复行为用）；老库/无行 → false。
pub fn get_batch_auto_tag(conn: &Connection, batch_id: i64) -> bool {
    conn.query_row(
        "SELECT COALESCE(autoTag, 0) FROM import_batches WHERE batchId=?1",
        [batch_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        != 0
}

pub fn get_batch(conn: &Connection, batch_id: i64) -> rusqlite::Result<ImportBatchRecord> {
    conn.query_row(
        "SELECT batchId, rootPath, storeMode, status, totalFiles, doneFiles, error, startedAt, endedAt
         FROM import_batches WHERE batchId=?1",
        rusqlite::params![batch_id],
        |r| {
            Ok(ImportBatchRecord {
                batchId: r.get(0)?,
                rootPath: r.get(1)?,
                storeMode: r.get(2)?,
                status: r.get(3)?,
                totalFiles: r.get(4)?,
                doneFiles: r.get(5)?,
                error: r.get(6)?,
                startedAt: r.get(7)?,
                endedAt: r.get(8)?,
            })
        },
    )
}

/// 找未完结批次（启动时续跑提示用）。
pub fn unfinished_batches(conn: &Connection) -> rusqlite::Result<Vec<ImportBatchRecord>> {
    let mut stmt = conn.prepare(
        "SELECT batchId, rootPath, storeMode, status, totalFiles, doneFiles, error, startedAt, endedAt
         FROM import_batches WHERE status IN ('running','failed') ORDER BY batchId",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ImportBatchRecord {
            batchId: r.get(0)?,
            rootPath: r.get(1)?,
            storeMode: r.get(2)?,
            status: r.get(3)?,
            totalFiles: r.get(4)?,
            doneFiles: r.get(5)?,
            error: r.get(6)?,
            startedAt: r.get(7)?,
            endedAt: r.get(8)?,
        })
    })?;
    rows.collect()
}

/// 处理单个文件（幂等核心）。
///
/// copyin_root：storeMode=copyin 时文件拷贝目的地库目录；None=reference 原地。
/// thumbs_root：缩略图输出根目录。
/// `batch_id`：调用方当前的导入批次（`import_batches.batchId`）—— 写进 `assets.batchId`，
/// 去重 `range=batch:<id>` 靠它精确圈范围（DM-A4 / 迁移 0033）。口径（不许随意改）：
///
/// * `Imported`（新入库）/ `Updated`（同路径内容变了重写）→ 写**当次**批次 id（覆盖上一次）；
/// * `Unchanged`（同内容重扫）/ `Duplicate`（同 hash 命中别的资产）→ 一个资产行都没写，
///   **不动**原归属（把老资产搬到新批次 = 篡改来源事实）；这两条路径要在「本批 0 张」时
///   由前端/报告如实说「这批没新入库」，而不是稀里糊涂算进别批；
/// * watcher 捡回的文件（`handle_file_created`）不是导入事件 → `None`（batchId 留 NULL）。
///
/// 返回 (分类, 实际参与索引的路径)；失败返回 Err（调用方计数 failed 继续下一个文件）。
pub fn process_file(
    conn: &Connection,
    path: &Path,
    store_mode: &str,
    copyin_root: Option<&Path>,
    thumbs_root: &Path,
    batch_id: Option<i64>,
) -> Result<(FileOutcome, PathBuf), ImportError> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ImportError::Unsupported(
            path.to_string_lossy().into_owned(),
        ));
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if media_type_of(&ext).is_none() {
        return Err(ImportError::Unsupported(ext));
    }
    let mtime = mtime_of(path)?;
    let abs = path.to_string_lossy().into_owned();

    let data = std::fs::read(path)?;
    let hash = core_hash::sha256_hex(&data);

    // 同路径现存记录？（deleted=0）
    let existing: Option<(i64, Option<String>, i64, i64)> = conn
        .query_row(
            "SELECT assetId, fileHash, mtime, fileSz FROM assets WHERE absPath=?1 AND deleted=0",
            rusqlite::params![abs],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map(Some)
        .or_else(no_rows_none)?;

    // 快路径：同路径 + hash/mtime/size 全等 → Unchanged（补缺失缩略图）
    if let Some((aid, Some(prev_hash), prev_mtime, prev_sz)) = &existing {
        if prev_hash == &hash && *prev_mtime == mtime && *prev_sz == meta.len() as i64 {
            ensure_thumbs(path, thumbs_root, *aid)?;
            return Ok((FileOutcome::Unchanged(*aid), path.to_path_buf()));
        }
    }

    // 物理去重锚点：同 hash 是否已有资产
    let dup: Option<i64> = conn
        .query_row(
            "SELECT assetId FROM assets WHERE fileHash=?1 AND deleted=0 LIMIT 1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(no_rows_none)?;

    // copyin：拷进库目录（hash 前 16 分目录防冲突；源在库内则拒绝防递归）
    let effective_path: PathBuf = if store_mode == "copyin" {
        let root =
            copyin_root.ok_or_else(|| ImportError::Unsupported("copyin without root".into()))?;
        if path.starts_with(root) {
            return Err(ImportError::RecursiveCopy(
                path.to_string_lossy().into_owned(),
            ));
        }
        let dest_dir = root.join(&hash[..16]);
        std::fs::create_dir_all(&dest_dir)?;
        let dest = dest_dir.join(path.file_name().unwrap_or_default());
        if !dest.exists() {
            std::fs::copy(path, &dest)?;
        }
        dest
    } else {
        path.to_path_buf()
    };

    match (existing, dup) {
        (Some((_aid, _, _, _)), Some(dup_id)) => {
            Ok((FileOutcome::Duplicate(dup_id), effective_path))
        }
        (Some((aid, _, _, _)), None) => {
            // D3：内容变了 → 重新入库前先确认「内容真的能解码」。
            // 失败就整条判 failed（资产保持旧记录不动），不写 width=0 的坏资源。
            let (w, h, luma) = probe_image(&effective_path)?;
            update_asset(
                conn,
                aid,
                &effective_path,
                &hash,
                mtime,
                meta.len() as i64,
                batch_id,
            )?;
            refresh_resource(conn, aid, &effective_path, thumbs_root, w, h, Some(&luma))?;
            apply_exif_meta(conn, aid, &effective_path, &ext);
            Ok((FileOutcome::Updated(aid), effective_path))
        }
        (None, Some(dup_id)) => Ok((FileOutcome::Duplicate(dup_id), effective_path)),
        (None, None) => {
            // D3：**先解码校验，再入库** —— 顺序是重点：反过来就会先插一条 width=0 的
            // 假资产（真机实测 assetId 28：21 字节 "not an image at all" 被算成导入成功）。
            let (w, h, luma) = probe_image(&effective_path)?;
            let aid = insert_asset(
                conn,
                &effective_path,
                store_mode,
                &ext,
                &hash,
                mtime,
                meta.len() as i64,
                batch_id,
            )?;
            refresh_resource(conn, aid, &effective_path, thumbs_root, w, h, Some(&luma))?;
            apply_exif_meta(conn, aid, &effective_path, &ext);
            Ok((FileOutcome::Imported(aid), effective_path))
        }
    }
}

/// D3：解码校验 —— 一次真解码（不是只读文件头）。
///
/// 返回 `(宽, 高, 32×32 灰度网格)`；网格同时给 `refresh_resource` 算 pHash 复用，
/// 所以校验本身**不额外增加**解码次数（此前 refresh 里也要解一次）。
/// 宽高按 EXIF Orientation 摆正（与缩略图方向一致，口径同 `oriented_size`）。
/// 为什么用 `luma32` 做「能不能解码」的判据：它走的是 `thumbnail` 的真实解码路径
/// （vips / rustimg 两个后端都会解像素），而不是 vips 的 header-only `new_from_file`
/// —— 后者对垃圾字节可能成功并给出 0×0。
pub fn probe_image(path: &Path) -> Result<(i32, i32, Vec<u8>), ImportError> {
    let luma = core_media::luma32(path)
        .map_err(|e| ImportError::BadImage(format!("{}: {e}", path.display())))?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let (w, h) = oriented_size(path, &ext);
    if w <= 0 || h <= 0 {
        return Err(ImportError::BadImage(format!(
            "{}: 解码后尺寸为 {w}×{h}",
            path.display()
        )));
    }
    Ok((w, h, luma))
}

/// EXIF2：宽高按 EXIF Orientation 摆正（5..=8 互换），与缩略图方向一致；
/// 解码失败 → (0,0)（沿用原语义，落库侧 aspect_of 会把 0 视为缺失）。
fn oriented_size(path: &Path, ext: &str) -> (i32, i32) {
    let (w, h) = core_media::image_size(path).unwrap_or((0, 0));
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let o = core_media::orientation_of_file(path, ext);
    core_media::oriented_dims(w, h, o)
}

/// EXIF2：导入路径的元数据落库。设置从 settings 表读（exif key）；
/// 任何失败只记原因不炸导入（engine_exif 内部已兜底，这里再兜一层防御）。
fn apply_exif_meta(conn: &Connection, aid: i64, path: &Path, ext: &str) {
    let cfg: core_config::ExifSettings =
        core_storage::settings::get_or_default(conn, core_config::EXIF_SETTINGS_KEY)
            .unwrap_or_default();
    if !cfg.parse_on_import {
        return;
    }
    if let Err(e) = engine_exif::apply_for_asset(conn, aid, path, ext, &cfg) {
        eprintln!("exif: asset {aid} 落库失败（导入继续）: {e}");
    }
}

fn no_rows_none<T>(e: rusqlite::Error) -> rusqlite::Result<Option<T>> {
    match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    }
}

fn mtime_of(p: &Path) -> Result<i64, ImportError> {
    Ok(std::fs::metadata(p)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

#[allow(clippy::too_many_arguments)] // INSERT 全列固有宽度
fn insert_asset(
    conn: &Connection,
    path: &Path,
    store_mode: &str,
    ext: &str,
    hash: &str,
    mtime: i64,
    sz: i64,
    batch_id: Option<i64>,
) -> rusqlite::Result<i64> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz, mtime, fileHash, source, importedAt, batchId)
         VALUES ('image', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 'userUpload', unixepoch(), ?8)",
        rusqlite::params![
            path.to_string_lossy(),
            store_mode,
            name,
            ext,
            sz,
            mtime,
            hash,
            batch_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 同路径内容变了 → 重写行。`batch_id = Some(n)` 时把归属改到当次导入事件
/// （`None` 保持原归属：watcher 等非导入来源不许篡改批次）。迁移 0033 / DM-A4。
fn update_asset(
    conn: &Connection,
    aid: i64,
    path: &Path,
    hash: &str,
    mtime: i64,
    sz: i64,
    batch_id: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE assets SET absPath=?2, fileHash=?3, mtime=?4, fileSz=?5, missing=0,
                           batchId=COALESCE(?6, batchId) WHERE assetId=?1",
        rusqlite::params![aid, path.to_string_lossy(), hash, mtime, sz, batch_id],
    )?;
    Ok(())
}

/// KI-2c：pHash 双列产物 —— `pHash` = identity 变体（老列，读路径不变），
/// `pHashes` = D4 八变体 JSON 文本（旋转/镜像比对用）。
/// 网格非法/算不出来 → 两列都留空（不写「无变体」的假数据）。
fn phash_columns(luma: &[u8]) -> (Option<String>, Option<String>) {
    match engine_dedup::d4_phashes(luma) {
        Ok(hs) => (
            Some(engine_dedup::phash_hex_of(hs[0])),
            Some(engine_dedup::phashes_to_text(&hs)),
        ),
        Err(_) => (None, None),
    }
}

/// 重建 asset 的 primary resource（缩略图两档 + pHash + 尺寸）。
///
/// `luma`：调用方已经解码过一次的 32×32 灰度网格（D3 解码校验顺带产出）→ 直接算 pHash，
/// 不再重复解码；None = 自己去解（老调用点）。
fn refresh_resource(
    conn: &Connection,
    aid: i64,
    path: &Path,
    thumbs_root: &Path,
    w: i32,
    h: i32,
    luma: Option<&[u8]>,
) -> rusqlite::Result<()> {
    for size in core_media::THUMB_SIZES {
        let tp = thumb_path(thumbs_root, aid, *size);
        if let Some(parent) = tp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = core_media::thumbnail_to(path, &tp, *size);
    }
    let thumb = thumb_path(thumbs_root, aid, 256)
        .to_string_lossy()
        .into_owned();
    // KI-2c：一次解码出网格 → 同时算 identity（老列）+ D4 八变体（新列），
    // 不额外解码、不额外读盘（8× DCT 是毫秒级）。
    let (ph, phs) = match luma {
        Some(l) => phash_columns(l),
        None => core_media::luma32(path)
            .map(|l| phash_columns(&l))
            .unwrap_or((None, None)),
    };
    conn.execute(
        "DELETE FROM resource WHERE assetId=?1",
        rusqlite::params![aid],
    )?;
    conn.execute(
        "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height, pHash, pHashes)
         VALUES (?1, 'primary', 0, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![aid, thumb, w as i64, h as i64, ph, phs],
    )?;
    // EXIF2：画幅派生列（aspectRatio/aspectClass）**导入即落**——宽高就是本次 primary resource
    // 写进去的值，与 exif_queue 回填时的补列口径同源（core_storage::exif_meta::aspect_of）。
    // 不这样做的话：导入后两列为 NULL，第一次回填才补上 → 「同批重跑回填」在 SQL 层不是零变化
    // （验收判据 D 幂等要求导入已完成全部派生列）。解码失败 w/h=0 → aspect_of 给 (None,None)，
    // 不编造比例。任何失败只记原因，不炸导入。
    if let Err(e) = core_storage::exif_meta::set_aspect(conn, aid, Some(w as i64), Some(h as i64)) {
        eprintln!("exif: asset {aid} 画幅列落库失败（导入继续）: {e}");
    }
    Ok(())
}

/// Unchanged 快路径：缩略图缺失时补生成（不重算 pHash）。
fn ensure_thumbs(path: &Path, thumbs_root: &Path, aid: i64) -> Result<(), ImportError> {
    for size in core_media::THUMB_SIZES {
        let tp = thumb_path(thumbs_root, aid, *size);
        if !tp.exists() {
            if let Some(parent) = tp.parent() {
                std::fs::create_dir_all(parent)?;
            }
            core_media::thumbnail_to(path, &tp, *size)?;
        }
    }
    Ok(())
}

fn progress_of(
    batch_id: i64,
    out: &BatchOutcome,
    done: i64,
    current: Option<String>,
    err: Option<String>,
    pending: Vec<String>,
) -> ImportProgress {
    ImportProgress {
        batchId: batch_id,
        status: out.status.clone(),
        done,
        total: out.total,
        imported: out.imported,
        duplicates: out.duplicates,
        failed: out.failed,
        lastError: err,
        currentPath: current,
        failedItems: out.failedItems.clone(),
        pendingNames: pending,
    }
}

/// 剩余未处理文件名（files[i+1..] 的文件名，保持处理序）。
fn pending_names(files: &[PathBuf], next: usize) -> Vec<String> {
    files[next..]
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect()
}

/// 跑一个批次：逐文件 process_file，单文件失败计数后继续（不让一张坏图卡死整批）。
/// on_progress 每 16 个文件/收尾时回调；回调返回 Err → 取消（status=cancelled）。
pub fn run_batch(
    conn: &Connection,
    batch_id: i64,
    copyin_root: Option<&Path>,
    thumbs_root: &Path,
    mut on_progress: impl FnMut(&ImportProgress) -> Result<(), String>,
) -> Result<BatchOutcome, ImportError> {
    let batch = get_batch(conn, batch_id)?;
    let root = PathBuf::from(&batch.rootPath);
    // D8：导入时一次性解析 path 标签（开关全关零成本跳过；从 source 路径解析，
    // copyin 模式下目录段相对批次根仍正确）
    let path_cfg = path_tags::settings(conn);
    let files = scan_dir(&root)?;
    let total = files.len() as i64;
    let mut out = BatchOutcome {
        batchId: batch_id,
        total,
        ..Default::default()
    };

    for (i, f) in files.iter().enumerate() {
        // E5 导入即图库：小批次（≤64）逐文件 emit（网格占位卡需要细粒度点亮）；
        // 大批次仍每 16 个 + 收尾，避免高频回调开销。
        let emit_every = if files.len() <= 64 { 1 } else { 16 };
        match process_file(
            conn,
            f,
            &batch.storeMode,
            copyin_root,
            thumbs_root,
            Some(batch_id),
        ) {
            Ok((FileOutcome::Imported(aid), _)) => {
                out.imported += 1;
                if !path_cfg.disabled() {
                    let tags = path_tags::derive_tags(&path_cfg, Some(&root), f);
                    let _ = path_tags::apply_tags(conn, aid, &tags);
                }
                // D10：新导入自动入队 enabled 自定义打标任务（无任务时零写入）
                let _ = core_storage::custom_tasks::enqueue_for_asset(conn, aid);
            }
            Ok((FileOutcome::Duplicate(_), _)) => out.duplicates += 1,
            Ok((FileOutcome::Unchanged(_), _)) => out.unchanged += 1,
            Ok((FileOutcome::Updated(_), _)) => out.updated += 1,
            Ok((FileOutcome::Failed, _)) => unreachable!(),
            Err(e) => {
                out.failed += 1;
                // 错误文本带文件路径：任务中心/日志能直接定位是哪张失败
                let msg = format!("{}: {e}", f.to_string_lossy());
                // 结构化失败明细（任务中心「失败明细」列表 + 「重试失败项」回传）。
                // 计数 failed 是全量，明细封顶 MAX_FAILED_ITEMS（大批次不撑爆 payload）。
                if out.failedItems.len() < core_types::MAX_FAILED_ITEMS {
                    out.failedItems.push(core_types::ImportFailure {
                        fileName: f
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| f.to_string_lossy().into_owned()),
                        path: f.to_string_lossy().into_owned(),
                        error: e.to_string(),
                    });
                }
                if let Err(se) = update_batch(conn, batch_id, i as i64 + 1) {
                    return Err(ImportError::from(se));
                }
                let p = progress_of(
                    batch_id,
                    &out,
                    i as i64 + 1,
                    Some(f.to_string_lossy().into_owned()),
                    Some(msg),
                    pending_names(&files, i + 1),
                );
                let _ = on_progress(&p);
            }
        }
        if i % emit_every == emit_every - 1 || i == files.len().saturating_sub(1) {
            update_batch(conn, batch_id, i as i64 + 1)?;
            let p = progress_of(
                batch_id,
                &out,
                i as i64 + 1,
                Some(f.to_string_lossy().into_owned()),
                None,
                pending_names(&files, i + 1),
            );
            if let Err(cancel) = on_progress(&p) {
                out.status = "cancelled".into();
                finish_batch(conn, batch_id, "cancelled", Some(&cancel))?;
                return Ok(out);
            }
        }
    }
    // failed 只影响批次报告计数，批次状态恒为 done
    out.status = "done".into();
    finish_batch(conn, batch_id, "done", None)?;
    let _ = on_progress(&progress_of(batch_id, &out, total, None, None, Vec::new()));
    Ok(out)
}

/// 从断点续跑：批次置回 running 后幂等重扫（Unchanged 快路径跳过已完成文件）。
pub fn resume_batch(
    conn: &Connection,
    batch_id: i64,
    copyin_root: Option<&Path>,
    thumbs_root: &Path,
    on_progress: impl FnMut(&ImportProgress) -> Result<(), String>,
) -> Result<BatchOutcome, ImportError> {
    conn.execute(
        "UPDATE import_batches SET status='running', endedAt=NULL, error=NULL WHERE batchId=?1",
        rusqlite::params![batch_id],
    )?;
    run_batch(conn, batch_id, copyin_root, thumbs_root, on_progress)
}

/// 只重跑指定文件（任务中心「重试失败项」）：不重扫整个目录，进度 total = 待重试文件数。
///
/// 场景：一批 200 张里 3 张因权限/占用/解码失败，用户修好环境后只想重跑这 3 张。
/// 走 resume_batch 会把整目录重扫一遍（Unchanged 快路径虽快，但数万张时仍是分钟级），
/// 所以这里按前端回传的失败项路径精确重跑。
///
/// 安全：paths 必须全部落在批次根目录内（防止把任意路径塞进库），越界整批拒绝
/// ——不做「部分成功但不知道漏了哪些」的静默降级。批次历史记录不改写
/// （重试结果只体现在本次 outcome/progress 里）。
pub fn retry_failed(
    conn: &Connection,
    batch_id: i64,
    paths: &[PathBuf],
    copyin_root: Option<&Path>,
    thumbs_root: &Path,
    mut on_progress: impl FnMut(&ImportProgress) -> Result<(), String>,
) -> Result<BatchOutcome, ImportError> {
    let batch = get_batch(conn, batch_id)?;
    let root = PathBuf::from(&batch.rootPath);
    for f in paths {
        if !f.is_absolute() {
            return Err(ImportError::NotAbsolute(f.to_string_lossy().into_owned()));
        }
        if !f.starts_with(&root) {
            return Err(ImportError::OutsideBatchRoot(format!(
                "{} (batch root {})",
                f.display(),
                root.display()
            )));
        }
    }
    let path_cfg = path_tags::settings(conn);
    let total = paths.len() as i64;
    let mut out = BatchOutcome {
        batchId: batch_id,
        total,
        ..Default::default()
    };
    for (i, f) in paths.iter().enumerate() {
        match process_file(
            conn,
            f,
            &batch.storeMode,
            copyin_root,
            thumbs_root,
            Some(batch_id),
        ) {
            Ok((FileOutcome::Imported(aid), _)) => {
                out.imported += 1;
                if !path_cfg.disabled() {
                    let tags = path_tags::derive_tags(&path_cfg, Some(&root), f);
                    let _ = path_tags::apply_tags(conn, aid, &tags);
                }
                let _ = core_storage::custom_tasks::enqueue_for_asset(conn, aid);
            }
            Ok((FileOutcome::Duplicate(_), _)) => out.duplicates += 1,
            Ok((FileOutcome::Unchanged(_), _)) => out.unchanged += 1,
            Ok((FileOutcome::Updated(_), _)) => out.updated += 1,
            Ok((FileOutcome::Failed, _)) => unreachable!(),
            Err(e) => {
                out.failed += 1;
                if out.failedItems.len() < core_types::MAX_FAILED_ITEMS {
                    out.failedItems.push(core_types::ImportFailure {
                        fileName: f
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| f.to_string_lossy().into_owned()),
                        path: f.to_string_lossy().into_owned(),
                        error: e.to_string(),
                    });
                }
            }
        }
        let p = progress_of(
            batch_id,
            &out,
            i as i64 + 1,
            Some(f.to_string_lossy().into_owned()),
            None,
            Vec::new(),
        );
        if on_progress(&p).is_err() {
            out.status = "cancelled".into();
            return Ok(out);
        }
    }
    out.status = "done".into();
    let _ = on_progress(&progress_of(batch_id, &out, total, None, None, Vec::new()));
    Ok(out)
}

/// relink：missing 资产指向新路径（hash 一致才接受，防止指错文件）。
pub fn relink(
    conn: &Connection,
    asset_id: i64,
    new_path: &Path,
    thumbs_root: &Path,
) -> Result<i64, ImportError> {
    let data = std::fs::read(new_path)?;
    let hash = core_hash::sha256_hex(&data);
    let recorded: Option<String> = conn
        .query_row(
            "SELECT fileHash FROM assets WHERE assetId=?1",
            rusqlite::params![asset_id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(no_rows_none)?;
    match recorded {
        Some(h) if h == hash => {
            update_asset(
                conn,
                asset_id,
                new_path,
                &hash,
                mtime_of(new_path)?,
                data.len() as i64,
                // relink = 文件搬家，不是导入事件 → 不改批次归属
                None,
            )?;
            let ext = new_path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let (w, hh) = oriented_size(new_path, &ext);
            refresh_resource(conn, asset_id, new_path, thumbs_root, w, hh, None)?;
            apply_exif_meta(conn, asset_id, new_path, &ext);
            // D8：手动 relink = 显式移动 → 重解析 path 标签（关态 no-op）
            let _ = path_tags::apply_for_path(conn, asset_id, new_path);
            Ok(asset_id)
        }
        _ => Err(ImportError::Unsupported(format!(
            "hash mismatch: asset {asset_id} expects different content"
        ))),
    }
}

/// 全库失联标记（启动/显式核查调用）：missing 标记/恢复，返回当前失联数。
///
/// DESKTOP-MISSING-STAT-1：语义等价于 `verify_missing(conn)?.missing`，保留本函数
/// 作为旧调用点的兼容入口（同一份实现，不允许两套判定逻辑漂移）。
pub fn mark_missing(conn: &Connection) -> core_storage::Result<i64> {
    Ok(verify_missing(conn)?.missing)
}

// ---------- DESKTOP-MISSING-STAT-1：失联现场核查 ----------
//
// 为什么要有这一节：`assets.missing` 只由 watcher 事件维护，进程没在跑、整个目录被
// 外部清空（/tmp 清理、外接盘拔掉）时**永远不会被更新**，于是 `/api/library/stats`
// 的 missing 一直报旧值（线上实例：源目录被清后 3 张图读不到，stats 仍 missing:0）。
// 缺失的不是"计数 bug"而是"核查动作"：任何失联数字都必须来自一次现场 stat。

/// 单条资产的文件系统探测分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ProbeClass {
    /// 文件在，可 stat
    Present,
    /// 不存在（含路径中间段被删/被替换成普通文件）→ 这就是"失联"
    NotFound,
    /// 存在但没权限 stat（DAC 拒绝）→ 不折进 missing，但如实上报
    PermissionDenied,
    /// 其他 IO 错误（符号链接环、IO 故障…）→ 如实上报，绝不吞
    Other,
}

/// 待核查目标（读一次拿全，避免逐条查库）。
// 字段名即 JSON 线上格式（camelCase），非 snake_case 是对外契约，勿改。
#[allow(non_snake_case)]
#[derive(Debug, Clone)]
pub struct MissingTarget {
    pub assetId: i64,
    pub absPath: String,
    /// 核查前的标记值（用于统计 newlyMissing/recovered 转移，免二次查询）
    pub wasMissing: i64,
    /// D3：记录里的尺寸（null/非正 = 历史上就没解出过像素 → 必须复核解码）
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// D3：记录里的 mtime/size —— 与盘上不一致说明文件被换过，必须复核解码
    pub mtime: i64,
    pub fileSz: i64,
}

impl MissingTarget {
    /// D3：这条资产要不要走解码复核。
    ///
    /// 判据 = 记录里没有正尺寸（从来没解出来 / 以前就坏）**或** 文件内容变了
    /// （mtime/size 与入库记录不同 → 可能是「修好后换回来」也可能是「被写坏」）。
    /// 记录有正尺寸且文件没动 → 跳过解码：1 万张全解码是不可接受的核查成本
    /// （DESKTOP-MISSING-STAT-1 的成本模型是每资产一次 stat，见 reports/MISSING-STAT-1.md）。
    pub fn needs_decode(&self, size_now: u64, mtime_now: i64) -> bool {
        let dims_ok = self.width.unwrap_or(0) > 0 && self.height.unwrap_or(0) > 0;
        !dims_ok || size_now as i64 != self.fileSz || mtime_now != self.mtime
    }
}

/// D3：核查期的解码结论。`Skipped` = 没复核（不是「好」）—— 报告里要能区分，
/// 不能把「跳过」说成「正常」。
// 变体字段名 = JSON 线上 camelCase 契约（pHash/pHashes 列名同源），勿改。
#[allow(non_snake_case)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum DecodeOutcome {
    /// 没复核（记录有正尺寸且文件未动 / 该入口不做解码）
    Skipped,
    /// 解得出像素；width/height 已按 EXIF 摆正（与缩略图同向）
    Ok {
        width: i32,
        height: i32,
        pHash: Option<String>,
        /// KI-2c：D4 八变体（JSON 文本，`pHashes` 列）
        pHashes: Option<String>,
    },
    /// 文件在盘上但解不开 = 「无法预览」（D3 的坏资产口径）
    Failed { error: String },
}

/// 单条探测结果。
// 字段名即 JSON 线上格式（camelCase），非 snake_case 是对外契约，勿改。
#[allow(non_snake_case)]
#[derive(Debug, Clone)]
pub struct MissingProbe {
    pub assetId: i64,
    pub absPath: String,
    pub wasMissing: i64,
    pub class: ProbeClass,
    /// 非 Present 时的原始错误串（分类之外的细节，不吞）
    pub error: Option<String>,
    /// D3：解码复核结论（Present 且需要复核时才有内容）
    pub decode: DecodeOutcome,
}

/// 核查报告（HTTP 直返 + 落 settings）。
#[derive(Debug, Clone, Serialize)]
// 字段名即 JSON 线上格式（camelCase），非 snake_case 是对外契约，勿改。
#[allow(non_snake_case)]
pub struct MissingVerifyReport {
    /// 本次实际 stat 的资产数
    pub checked: i64,
    pub notFound: i64,
    pub permissionDenied: i64,
    pub other: i64,
    /// 存在但读不了 = permissionDenied + other
    pub unreadable: i64,
    /// 核查前库里的 missing 总数
    pub missingBefore: i64,
    /// 核查后库里的 missing 总数（stats 用的是它）
    pub missing: i64,
    /// 本次新标记为失联
    pub newlyMissing: i64,
    /// 本次从失联接回
    pub recovered: i64,
    /// D3：本次复核里「文件在但解不开」的数量（= 无法预览，可被 ?broken=1 筛出）
    pub broken: i64,
    /// D3：本次复核里解码成功并已修复尺寸/缩略图的资产数
    pub repaired: i64,
    pub durationMs: i64,
    pub checkedAt: i64,
    /// 异常样本（NotFound/不可读/无法解码各留前若干条，便于定位；不吞细节也不撑爆响应）
    pub samples: Vec<MissingSample>,
}

#[derive(Debug, Clone, Serialize)]
// 字段名即 JSON 线上格式（camelCase），非 snake_case 是对外契约，勿改。
#[allow(non_snake_case)]
pub struct MissingSample {
    /// notFound / permissionDenied / other / unparseable
    pub class: String,
    pub absPath: String,
    pub error: Option<String>,
}

/// 报告里每类最多留几条样本。
pub const MISSING_REPORT_SAMPLE_CAP: usize = 20;

/// 读全库待核查目标（deleted=0 的全部资产）。
pub fn collect_missing_targets(conn: &Connection) -> core_storage::Result<Vec<MissingTarget>> {
    let mut stmt = conn.prepare(
        "SELECT a.assetId, a.absPath, a.missing, r.width, r.height, a.mtime, a.fileSz
         FROM assets a
         LEFT JOIN resource r ON r.assetId = a.assetId AND r.role = 'primary'
         WHERE a.deleted = 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(MissingTarget {
            assetId: r.get(0)?,
            absPath: r.get(1)?,
            wasMissing: r.get(2)?,
            width: r.get(3)?,
            height: r.get(4)?,
            mtime: r.get(5)?,
            fileSz: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// io::Error → 分类。单独抽出来是为了可测：容器内以 root 跑单测时 DAC 权限拒绝
/// 无法用 chmod 构造（root 绕过权限），只能注入错误来覆盖该分类。
pub fn classify_io_error(e: &std::io::Error) -> ProbeClass {
    match e.kind() {
        std::io::ErrorKind::NotFound => ProbeClass::NotFound,
        std::io::ErrorKind::PermissionDenied => ProbeClass::PermissionDenied,
        // ENOTDIR：路径中间段不是目录（目录被删除/替换）→ 等价失联
        _ if e.raw_os_error() == Some(20) => ProbeClass::NotFound,
        _ => ProbeClass::Other,
    }
}

/// 注入式探测（单测用）：`stat` 只做一件事 —— 对路径返回 Ok/Err。
/// 不做解码复核（`decode = Skipped`）：解码路径由 `probe_paths_checked` /
/// `probes_with_decoder` 覆盖。
pub fn probe_paths_with<F>(targets: &[MissingTarget], stat: F) -> Vec<MissingProbe>
where
    F: Fn(&Path) -> std::io::Result<()>,
{
    targets
        .iter()
        .map(|t| match stat(Path::new(&t.absPath)) {
            Ok(()) => MissingProbe {
                assetId: t.assetId,
                absPath: t.absPath.clone(),
                wasMissing: t.wasMissing,
                class: ProbeClass::Present,
                error: None,
                decode: DecodeOutcome::Skipped,
            },
            Err(e) => MissingProbe {
                assetId: t.assetId,
                absPath: t.absPath.clone(),
                wasMissing: t.wasMissing,
                class: classify_io_error(&e),
                error: Some(e.to_string()),
                decode: DecodeOutcome::Skipped,
            },
        })
        .collect()
}

/// 注入式探测 + 可注入解码（单测用）：把「要不要复核」交给 `decode` 决定。
pub fn probes_with_decoder<F, D>(targets: &[MissingTarget], stat: F, decode: D) -> Vec<MissingProbe>
where
    F: Fn(&Path) -> std::io::Result<()>,
    D: Fn(&MissingTarget) -> DecodeOutcome,
{
    targets
        .iter()
        .map(|t| match stat(Path::new(&t.absPath)) {
            Ok(()) => MissingProbe {
                assetId: t.assetId,
                absPath: t.absPath.clone(),
                wasMissing: t.wasMissing,
                class: ProbeClass::Present,
                error: None,
                decode: decode(t),
            },
            Err(e) => MissingProbe {
                assetId: t.assetId,
                absPath: t.absPath.clone(),
                wasMissing: t.wasMissing,
                class: classify_io_error(&e),
                error: Some(e.to_string()),
                decode: DecodeOutcome::Skipped,
            },
        })
        .collect()
}

/// 真实文件系统探测（元数据 stat，不读内容；D3 起对「可疑」资产再做一次解码复核）。
///
/// `thumbs_root`：
///   - Some(..) → 对需要复核的资产解码校验，解得出就顺手重生成两档缩略图（修复）。
///     HTTP「重新核查」走这条（用户显式动作，值得多花这点 IO）。
///   - None → 只 stat（启动期静默核查走这条：不在启动路径上批量解码）。
pub fn probe_paths_checked(
    targets: &[MissingTarget],
    thumbs_root: Option<&Path>,
) -> Vec<MissingProbe> {
    targets
        .iter()
        .map(|t| {
            let path = Path::new(&t.absPath);
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(e) => {
                    return MissingProbe {
                        assetId: t.assetId,
                        absPath: t.absPath.clone(),
                        wasMissing: t.wasMissing,
                        class: classify_io_error(&e),
                        error: Some(e.to_string()),
                        decode: DecodeOutcome::Skipped,
                    }
                }
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let decode = match thumbs_root {
                Some(root) if t.needs_decode(meta.len(), mtime) => decode_and_repair(t, path, root),
                _ => DecodeOutcome::Skipped,
            };
            MissingProbe {
                assetId: t.assetId,
                absPath: t.absPath.clone(),
                wasMissing: t.wasMissing,
                class: ProbeClass::Present,
                error: None,
                decode,
            }
        })
        .collect()
}

/// 真实文件系统探测（仅 stat，不解码）。兼容旧调用点。
pub fn probe_paths(targets: &[MissingTarget]) -> Vec<MissingProbe> {
    probe_paths_checked(targets, None)
}

/// D3：解码复核 + 修复缩略图（核查阶段不持库锁）。
/// 解得出 → `DecodeOutcome::Ok`（apply 阶段落库新尺寸/pHash）；
/// 解不出 → `Failed`（apply 阶段把该资产的尺寸写成 0 → 进入「无法预览」口径）。
fn decode_and_repair(t: &MissingTarget, path: &Path, thumbs_root: &Path) -> DecodeOutcome {
    match probe_image(path) {
        Ok((w, h, luma)) => {
            for size in core_media::THUMB_SIZES {
                let tp = thumb_path(thumbs_root, t.assetId, *size);
                if let Some(parent) = tp.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = core_media::thumbnail_to(path, &tp, *size);
            }
            let (ph, phs) = phash_columns(&luma);
            DecodeOutcome::Ok {
                width: w,
                height: h,
                pHash: ph,
                pHashes: phs,
            }
        }
        Err(e) => DecodeOutcome::Failed {
            error: e.to_string(),
        },
    }
}

/// D3：把复核结论写回 primary resource（行不存在则补建）。
fn write_resource_health(
    conn: &Connection,
    asset_id: i64,
    thumbs_root: Option<&Path>,
    dims: (i32, i32),
    phash: Option<&str>,
    phashes: Option<&str>,
) -> core_storage::Result<()> {
    let (w, h) = (dims.0 as i64, dims.1 as i64);
    let n = conn.execute(
        "UPDATE resource SET width=?2, height=?3, pHash=?4, pHashes=?5
          WHERE assetId=?1 AND role='primary'",
        rusqlite::params![asset_id, w, h, phash, phashes],
    )?;
    if n == 0 {
        let thumb = thumbs_root
            .map(|r| thumb_path(r, asset_id, 256).to_string_lossy().into_owned())
            .unwrap_or_default();
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height, pHash, pHashes)
             VALUES (?1, 'primary', 0, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![asset_id, thumb, w, h, phash, phashes],
        )?;
    }
    // 画幅派生列与尺寸同源（坏资产写 0 → aspect_of 给 (None,None)，不编造比例）
    let _ = core_storage::exif_meta::set_aspect(conn, asset_id, Some(w), Some(h));
    Ok(())
}

/// 把探测结果写回 assets.missing（只动变化行），并落 settings 记录 + 组装报告。
///
/// 并发安全：UPDATE 带 `absPath=?` 条件 —— 核查期间 watcher/relink 改了路径的行
/// 不被本次结果覆盖（宁可留下次核查修，也不用过期路径判定结果）。
/// D3：`decode != Skipped` 的条目同时把「能不能预览」写回 primary resource
/// （Ok → 新尺寸/pHash；Failed → 0×0 = 无法预览），所以 `?broken=1` 与
/// `stats.broken` 的判定口径在核查后是最新的。
pub fn apply_missing_probes(
    conn: &Connection,
    probes: &[MissingProbe],
    started: std::time::Instant,
) -> core_storage::Result<MissingVerifyReport> {
    apply_missing_probes_repairing(conn, probes, started, None)
}

/// `apply_missing_probes` + 修复落库时需要 thumbs_root 的版本（HTTP 核查用）。
pub fn apply_missing_probes_repairing(
    conn: &Connection,
    probes: &[MissingProbe],
    started: std::time::Instant,
    thumbs_root: Option<&Path>,
) -> core_storage::Result<MissingVerifyReport> {
    let missing_before: i64 = conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE deleted=0 AND missing=1",
        [],
        |r| r.get(0),
    )?;

    let mut not_found = 0i64;
    let mut perm = 0i64;
    let mut other = 0i64;
    let mut newly = 0i64;
    let mut recovered = 0i64;
    let mut broken = 0i64;
    let mut repaired = 0i64;
    let mut samples: Vec<MissingSample> = Vec::new();

    for p in probes {
        let mut sample_of = |class: &str, err: Option<String>| {
            if samples.len() < MISSING_REPORT_SAMPLE_CAP {
                samples.push(MissingSample {
                    class: class.to_string(),
                    absPath: p.absPath.clone(),
                    error: err,
                });
            }
        };
        let want = match p.class {
            ProbeClass::Present => {
                if p.wasMissing == 1 {
                    recovered += 1;
                }
                0i64
            }
            ProbeClass::NotFound => {
                not_found += 1;
                if p.wasMissing == 0 {
                    newly += 1;
                }
                sample_of("notFound", p.error.clone());
                1i64
            }
            ProbeClass::PermissionDenied => {
                perm += 1;
                sample_of("permissionDenied", p.error.clone());
                0i64
            }
            ProbeClass::Other => {
                other += 1;
                sample_of("other", p.error.clone());
                0i64
            }
        };
        conn.execute(
            "UPDATE assets SET missing=?3 WHERE assetId=?1 AND absPath=?2 AND missing<>?3",
            rusqlite::params![p.assetId, p.absPath, want],
        )?;
        // D3：解码复核结论写回（Present 才可能有结论）
        match &p.decode {
            DecodeOutcome::Skipped => {}
            DecodeOutcome::Ok {
                width,
                height,
                pHash,
                pHashes,
            } => {
                // 「修复」= 记录真的变了。尺寸/pHash 本来就对（只是文件 mtime 变了）
                // 就不再重写、也不计数 —— 否则每点一次「重新核查」都报一次 repaired，
                // 报告会变成"每次都在修"的假象。
                let now: Option<(i64, i64, Option<String>, Option<String>)> = conn
                    .query_row(
                        "SELECT width, height, pHash, pHashes FROM resource
                         WHERE assetId=?1 AND role='primary'",
                        rusqlite::params![p.assetId],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                let same = now.as_ref().is_some_and(|(w, h, ph, phs)| {
                    *w == *width as i64
                        && *h == *height as i64
                        && ph.as_deref() == pHash.as_deref()
                        && phs.as_deref() == pHashes.as_deref()
                });
                if !same {
                    write_resource_health(
                        conn,
                        p.assetId,
                        thumbs_root,
                        (*width, *height),
                        pHash.as_deref(),
                        pHashes.as_deref(),
                    )?;
                    repaired += 1;
                }
            }
            DecodeOutcome::Failed { error } => {
                broken += 1;
                write_resource_health(conn, p.assetId, thumbs_root, (0, 0), None, None)?;
                sample_of("unparseable", Some(error.clone()));
            }
        }
    }

    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE deleted=0 AND missing=1",
        [],
        |r| r.get(0),
    )?;
    let checked_at = now_secs();
    let report = MissingVerifyReport {
        checked: probes.len() as i64,
        notFound: not_found,
        permissionDenied: perm,
        other,
        unreadable: perm + other,
        missingBefore: missing_before,
        missing,
        newlyMissing: newly,
        recovered,
        broken,
        repaired,
        durationMs: started.elapsed().as_millis() as i64,
        checkedAt: checked_at,
        samples,
    };
    let rec = core_types::MissingCheckRecord {
        at: checked_at,
        durationMs: report.durationMs,
        checked: report.checked,
        notFound: report.notFound,
        unreadable: report.unreadable,
        missing: report.missing,
        recovered: report.recovered,
        broken: report.broken,
        repaired: report.repaired,
    };
    core_storage::settings::set_json(conn, core_types::MISSING_CHECK_KEY, &rec)?;
    Ok(report)
}

/// 全量失联现场核查（读目标 → stat → 写回 → 记录）。同步、持连接。
/// HTTP 路径请用 collect/probe/apply 三段（避免长持锁，见 app-web library_api::verify）。
///
/// 注意：这是**不解码**的兼容入口（启动期用）。要连「无法预览」一起现场核，
/// 用 `verify_missing_with_thumbs`（HTTP「重新核查」走它）。
pub fn verify_missing(conn: &Connection) -> core_storage::Result<MissingVerifyReport> {
    let started = std::time::Instant::now();
    let targets = collect_missing_targets(conn)?;
    let probes = probe_paths_checked(&targets, None);
    apply_missing_probes_repairing(conn, &probes, started, None)
}

/// 全量核查 + 解码复核（文件在但解不开 = 无法预览）+ 修复缩略图。
pub fn verify_missing_with_thumbs(
    conn: &Connection,
    thumbs_root: &Path,
) -> core_storage::Result<MissingVerifyReport> {
    let started = std::time::Instant::now();
    let targets = collect_missing_targets(conn)?;
    let probes = probe_paths_checked(&targets, Some(thumbs_root));
    apply_missing_probes_repairing(conn, &probes, started, Some(thumbs_root))
}

/// 上次核查记录（settings 缺失/损坏 → None，绝不让读取方炸掉）。
pub fn last_missing_check(conn: &Connection) -> Option<core_types::MissingCheckRecord> {
    core_storage::settings::get_json::<core_types::MissingCheckRecord>(
        conn,
        core_types::MISSING_CHECK_KEY,
    )
    .ok()
    .flatten()
}

/// 当前 unix 秒。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------- watcher 事件落库（L3 只转发 notify 事件，语义在此） ----------

/// 文件出现（Create/Modify）：非图片忽略；是 missing 资产同内容 → 自动接回（moved 回来）；
/// 否则走 process_file 幂等入库。返回给 UI 的 WatchEvent（ignore 时 kind="ignored"）。
pub fn handle_file_created(
    conn: &Connection,
    path: &Path,
    store_mode: &str,
    copyin_root: Option<&Path>,
    thumbs_root: &Path,
) -> rusqlite::Result<core_types::WatchEvent> {
    let p_str = path.to_string_lossy().into_owned();
    let ext_ok = path
        .extension()
        .map(|e| media_type_of(&e.to_string_lossy()).is_some())
        .unwrap_or(false);
    if !ext_ok || !path.is_file() {
        return Ok(core_types::WatchEvent {
            kind: "ignored".into(),
            path: p_str,
            to: None,
            assetId: None,
            error: None,
        });
    }
    // 同 hash 的 missing 资产 → 接回（文件被移回/改名回来）
    if let Ok(data) = std::fs::read(path) {
        let hash = core_hash::sha256_hex(&data);
        let missing_same: Option<i64> = conn
            .query_row(
                "SELECT assetId FROM assets WHERE fileHash=?1 AND missing=1 AND deleted=0 LIMIT 1",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(no_rows_none)?;
        if let Some(aid) = missing_same {
            let mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            update_asset(conn, aid, path, &hash, mtime, data.len() as i64, None)?;
            // D8：移动/改名接回 → 仅重解析 origin='path' 标签（auto/manual 不动）
            let _ = path_tags::apply_for_path(conn, aid, path);
            return Ok(core_types::WatchEvent {
                kind: "moved".into(),
                path: p_str,
                to: None,
                assetId: Some(aid),
                error: None,
            });
        }
    }
    let outcome = process_file(conn, path, store_mode, copyin_root, thumbs_root, None);
    match outcome {
        Ok((FileOutcome::Imported(aid), _)) | Ok((FileOutcome::Updated(aid), _)) => {
            // D8：watcher 新导入也产 path 标签（开关全关时 apply_for_path 内部 no-op）
            let _ = path_tags::apply_for_path(conn, aid, path);
            // D10：watcher 新导入/文件变更 → 自动入队 enabled 自定义打标任务
            //（幂等：未终态 job 不重复；内容变更重跑由 tag 层 upsert 承接；无任务零写入）
            let _ = core_storage::custom_tasks::enqueue_for_asset(conn, aid);
            Ok(core_types::WatchEvent {
                kind: "imported".into(),
                path: p_str,
                to: None,
                assetId: Some(aid),
                error: None,
            })
        }
        Ok(_) => Ok(core_types::WatchEvent {
            kind: "ignored".into(),
            path: p_str,
            to: None,
            assetId: None,
            error: None,
        }),
        Err(e) => Ok(core_types::WatchEvent {
            kind: "error".into(),
            path: p_str,
            to: None,
            assetId: None,
            error: Some(e.to_string()),
        }),
    }
}

/// 文件消失（Remove/Rename-From）：absPath 命中 → missing=1。
pub fn handle_file_removed(
    conn: &Connection,
    path: &Path,
) -> rusqlite::Result<core_types::WatchEvent> {
    let p_str = path.to_string_lossy().into_owned();
    let n = conn.execute(
        "UPDATE assets SET missing=1 WHERE absPath=?1 AND deleted=0 AND missing=0",
        rusqlite::params![p_str],
    )?;
    let kind = if n > 0 { "missing" } else { "ignored" };
    Ok(core_types::WatchEvent {
        kind: kind.into(),
        path: p_str,
        to: None,
        assetId: None,
        error: None,
    })
}

/// 文件移动（Rename To 配对 From）：新位置存在 + 旧 absPath 有 missing 记录同 hash → 改 absPath。
/// v1 简化：To 事件按 handle_file_created 处理已可接回（hash 匹配 missing 资产）。
pub fn watch_roots(conn: &Connection) -> rusqlite::Result<Vec<PathBuf>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT rootPath FROM import_batches WHERE status IN ('done','running','cancelled','failed')",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for p in rows.collect::<rusqlite::Result<Vec<String>>>()? {
        let pb = PathBuf::from(&p);
        if pb.is_dir() {
            out.push(pb);
        }
    }
    Ok(out)
}

/// 单测专用：libvips 全局状态不允许并发 VipsInit（class 注册竞争 → SIGBUS/SIGABRT）。
/// D3 起导入会真解码，所以同一个测试二进制里凡是走 process_file/run_batch/
/// handle_file_created 的用例都必须先拿这把锁串行化。
#[cfg(test)]
pub(crate) fn vips_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3：夹具必须是**真图片** —— 导入会做解码校验，随便塞 b"xxx" 的 .jpg 现在会判 failed。
    /// 最小合法 PNG（1×1 RGBA，A/B 两种颜色；哈希不同 → 去重逻辑仍可验）。
    /// 故意不用 libvips 造图：单测二进制里别为夹具去抢 vips 全局锁。
    const PNG_A: [u8; 70] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 5, 0, 1, 0xFF, 0x89, 0x99,
        0x3D, 0x1D, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    const PNG_B: [u8; 70] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44,
        0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0xF8, 0xCF, 0xF0, 0x1F, 0, 4, 1, 1, 0xFF, 0x71, 0xEB,
        0x47, 0xE5, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn media_type_routing() {
        assert_eq!(media_type_of("jpg"), Some("image"));
        assert_eq!(media_type_of(".JPEG"), Some("image"));
        assert_eq!(media_type_of("mp4"), None);
    }

    #[test]
    fn scan_skips_hidden_and_non_image() {
        let dir = std::env::temp_dir().join(format!("d1_scan_{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(sub.join(".hidden")).unwrap();
        std::fs::write(dir.join(".DS_Store"), b"x").unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        std::fs::write(sub.join("c.PNG"), b"x").unwrap();
        let found = scan_dir(&dir).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"a.jpg".to_string()));
        assert!(names.contains(&"c.PNG".to_string()));
        assert!(!names.iter().any(|n| n.ends_with(".txt")));
        assert!(!names.iter().any(|n| n.starts_with('.')));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---------- D8：导入批次产 path 标签（开关 on/off，PLAN 验收） ----------

    /// 批次根之外的缩略图目录（`<root>_thumbs`）—— 缩略图落在批次根内会被下一轮扫描
    /// 当成新图片导入（D3 起缩略图真会生成，这个夹具坑才暴露出来）。
    fn sibling_thumbs(root: &std::path::Path) -> std::path::PathBuf {
        let mut p = root.to_path_buf().into_os_string();
        p.push("_thumbs");
        std::path::PathBuf::from(p)
    }

    fn path_tags_of(conn: &Connection, aid: i64) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT value FROM tag WHERE assetId=?1 AND origin='path' ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![aid], |r| r.get::<_, String>(0))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    #[test]
    fn run_batch_path_tags_switch_on_and_off() {
        let _g = crate::vips_lock();
        let root = std::env::temp_dir().join(format!("d8_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let trip = root.join("旅行");
        std::fs::create_dir_all(&trip).unwrap();
        std::fs::write(trip.join("海边 度假.jpg"), PNG_A).unwrap();
        std::fs::write(trip.join("IMG_1234.jpg"), PNG_B).unwrap();
        // D3：缩略图真会生成 → 必须放在**批次根之外**，否则下一轮 scan_dir 会把
        // `_thumbs/256/**/*.webp` 当成新图片再次导入（测试夹具自污染）。
        let thumbs = sibling_thumbs(&root);
        std::fs::create_dir_all(&thumbs).unwrap();

        // 开关全关导入 → 不产任何 path 标签
        {
            let conn = Connection::open_in_memory().unwrap();
            core_storage::migrations::migrate(&conn).unwrap();
            let batch = create_batch(&conn, &root, "reference", 2).unwrap();
            run_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM tag WHERE origin='path'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(n, 0, "默认关：导入不产标签");
        }
        // 开启后导入 → 目录段+文件名词成为 origin=path 标签（噪声文件名不产）
        {
            let conn = Connection::open_in_memory().unwrap();
            core_storage::migrations::migrate(&conn).unwrap();
            core_storage::settings::set_json(
                &conn,
                core_config::PATH_TAG_SETTINGS_KEY,
                &core_config::PathTagSettings {
                    use_dirs: true,
                    use_filenames: true,
                },
            )
            .unwrap();
            let batch = create_batch(&conn, &root, "reference", 2).unwrap();
            run_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();
            let rows: Vec<(i64, String)> = {
                let mut stmt = conn
                    .prepare("SELECT a.assetId, a.fileName FROM assets a")
                    .unwrap();
                let it = stmt
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                    .unwrap();
                it.collect::<Result<Vec<_>, _>>().unwrap()
            };
            assert_eq!(rows.len(), 2);
            for (aid, name) in rows {
                let tags = path_tags_of(&conn, aid);
                if name == "海边 度假.jpg" {
                    assert_eq!(tags, ["旅行", "海边", "度假"], "目录段+文件名词");
                } else {
                    // IMG_1234 文件名全噪声：只剩目录段标签
                    assert_eq!(tags, ["旅行"], "文件名词全噪声，仅目录段，got {tags:?}");
                }
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ---------- D10：新导入自动入队 enabled 自定义打标任务 ----------

    #[test]
    fn run_batch_auto_enqueues_enabled_custom_tag_tasks() {
        let _g = crate::vips_lock();
        let root = std::env::temp_dir().join(format!("d10_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.jpg"), PNG_A).unwrap();
        std::fs::write(root.join("b.jpg"), PNG_B).unwrap();
        // D3：缩略图放批次根外（同上，防 scan_dir 把 thumbs 当新图导入）
        let thumbs = sibling_thumbs(&root);
        std::fs::create_dir_all(&thumbs).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let t_on = core_storage::custom_tasks::create_task(&conn, "拍摄地点", "地点是什么？", true)
            .unwrap();
        let t_off = core_storage::custom_tasks::create_task(&conn, "色调氛围", "色调如何？", false)
            .unwrap();

        let batch = create_batch(&conn, &root, "reference", 2).unwrap();
        run_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();

        let jobs: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("SELECT kind, COALESCE(payload,'') FROM job_queue ORDER BY id")
                .unwrap();
            let it = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            it.collect::<Result<Vec<_>, _>>().unwrap()
        };
        // 仅 enabled 任务 × 2 张新图；disabled 任务不入队
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        let want = core_storage::custom_tasks::custom_payload(t_on);
        assert!(jobs.iter().all(|(k, p)| k == "custom_tag" && *p == want));
        assert_ne!(want, core_storage::custom_tasks::custom_payload(t_off));

        // 批次重扫（Unchanged 快路径）→ 不重复入队
        let batch2 = create_batch(&conn, &root, "reference", 2).unwrap();
        run_batch(&conn, batch2, None, &thumbs, |_| Ok(())).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM job_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "批次重扫不得重复入队");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn watcher_move_reparses_path_tags_only() {
        let _g = crate::vips_lock();
        let root = std::env::temp_dir().join(format!("d8_move_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir_a = root.join("旅行");
        let dir_b = root.join("工作");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        // D3：缩略图放批次根外（防 scan_dir 把 _thumbs/**/*.webp 当新图导入）
        let thumbs = sibling_thumbs(&root);
        std::fs::create_dir_all(&thumbs).unwrap();
        let f1 = dir_a.join("海边.jpg");
        std::fs::write(&f1, PNG_A).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        core_storage::settings::set_json(
            &conn,
            core_config::PATH_TAG_SETTINGS_KEY,
            &core_config::PathTagSettings {
                use_dirs: true,
                use_filenames: true,
            },
        )
        .unwrap();
        let batch = create_batch(&conn, &root, "reference", 1).unwrap();
        run_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();
        let aid: i64 = conn
            .query_row("SELECT assetId FROM assets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(path_tags_of(&conn, aid), ["旅行", "海边"]);
        // 手动/auto 标签在场，验证不被移动重解析触碰
        conn.execute(
            "INSERT INTO tag (assetId, value, origin, createdAt) VALUES (?1, '手动', 'manual', 1), (?1, '狗', 'auto', 1)",
            rusqlite::params![aid],
        )
        .unwrap();

        // 文件被移走（missing）再以新路径出现 → watcher 接回 + 仅重解析 path 标签
        let f2 = dir_b.join("礁石.jpg");
        std::fs::rename(&f1, &f2).unwrap();
        conn.execute(
            "UPDATE assets SET missing=1 WHERE assetId=?1",
            rusqlite::params![aid],
        )
        .unwrap();
        let ev = handle_file_created(&conn, &f2, "reference", None, &thumbs).unwrap();
        assert_eq!(ev.kind, "moved");
        assert_eq!(path_tags_of(&conn, aid), ["工作", "礁石"], "按新路径重解析");
        let manual: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tag WHERE assetId=?1 AND origin IN ('manual','auto')",
                rusqlite::params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(manual, 2, "auto/manual 标签不动");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ---------- D2：失败明细（哪张、为什么）+ 只重试失败项 ----------

    /// 一张坏图不能再被「done」吞掉：outcome/progress 要带失败明细，
    /// 且「重试失败项」只处理那几张（不重扫目录）。
    #[cfg(unix)]
    #[test]
    fn failed_items_carry_reason_and_retry_only_those_paths() {
        let _g = crate::vips_lock();
        let root = std::env::temp_dir().join(format!("d2fail_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // D3：真图（坏内容现在会判 failed，正是 D3 的行为）
        std::fs::write(root.join("ok.jpg"), PNG_A).unwrap();
        // 悬空符号链接：扫描期扩展名合法算图片，处理期 metadata 失败
        // （等价于真机上 icacls deny / 文件被占用时 process_file 报错）
        std::os::unix::fs::symlink(
            root.join("missing-target.jpg"),
            root.join("denied-read.jpg"),
        )
        .unwrap();
        let thumbs = sibling_thumbs(&root);
        std::fs::create_dir_all(&thumbs).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let batch = create_batch(&conn, &root, "reference", 2).unwrap();

        let mut seen: Vec<ImportProgress> = Vec::new();
        let out = run_batch(&conn, batch, None, &thumbs, |p| {
            seen.push(p.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(out.imported, 1, "{out:?}");
        assert_eq!(out.failed, 1, "{out:?}");
        assert_eq!(out.failedItems.len(), 1, "失败明细必须带出失败项");
        let f = out.failedItems[0].clone();
        assert_eq!(f.fileName, "denied-read.jpg", "明细给文件名，不是整串路径");
        assert!(
            f.path.ends_with("denied-read.jpg"),
            "明细带绝对路径（重试要用）：{}",
            f.path
        );
        assert!(!f.error.is_empty(), "失败原因不得为空");
        // 进度分支也要带（前端轮询靠 progress.failedItems 实时展示）
        let last = seen.last().unwrap();
        assert_eq!(last.failedItems.len(), 1, "progress 也要带失败明细");

        // 修好源文件 → 只重试这一张：total 恒为 1（不重扫整个目录）
        std::fs::write(root.join("missing-target.jpg"), PNG_B).unwrap();
        let mut totals: Vec<i64> = Vec::new();
        let out2 = retry_failed(
            &conn,
            batch,
            &[std::path::PathBuf::from(&f.path)],
            None,
            &thumbs,
            |p| {
                totals.push(p.total);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out2.failed, 0, "{out2:?}");
        assert_eq!(out2.imported, 1, "{out2:?}");
        assert!(out2.failedItems.is_empty());
        assert!(
            totals.iter().all(|t| *t == 1),
            "重试口径 total=待重试张数，不该是整目录：{totals:?}"
        );

        // 越界路径整批拒绝（不能把任意路径塞进来导入）
        let err = retry_failed(
            &conn,
            batch,
            &[std::path::PathBuf::from("/etc/passwd")],
            None,
            &thumbs,
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(
            matches!(err, ImportError::OutsideBatchRoot(_)),
            "越界应报 OutsideBatchRoot，got {err:?}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ================= KI-2c：D4 八变体 pHash 入库 + 旋转/镜像判重 =================

    /// 非对称源图的一个像素（同一图在 8 个 D4 朝向下互不相同）。
    fn asym_px(x: u32, y: u32) -> [u8; 3] {
        let v = (x * 37 + y * 91 + (x * y) % 61 + (x * x) % 23) % 256;
        [
            v as u8,
            ((v * 3 + 17) % 256) as u8,
            ((v * 7 + 5) % 256) as u8,
        ]
    }

    /// 无关图（强条纹 + 亮块）：与上面的纹理 pHash 相距很远。
    fn unrelated_px(x: u32, y: u32) -> [u8; 3] {
        let striped = ((x / 3) + (y / 3)).is_multiple_of(2);
        let in_block = (2..10).contains(&x) && (18..30).contains(&y);
        let v: u8 = if in_block {
            250
        } else if striped {
            200
        } else {
            15
        };
        [v, v, v]
    }

    fn write_bmp_rgb(path: &Path, w: u32, h: u32, px: impl Fn(u32, u32) -> [u8; 3]) {
        // BMP 24 位未压缩：IMAGE_EXTS 白名单里有 bmp，且 vips / image 都能解 →
        // 夹具用真图片文件进导入管线（不额外拉图片编码依赖）。
        let row_pad = (4 - (w as usize * 3) % 4) % 4;
        let data_sz = (w as usize * 3 + row_pad) * h as usize;
        let mut out = Vec::with_capacity(54 + data_sz);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&((54 + data_sz) as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes()); // 正数 = 底向上
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data_sz as u32).to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&2835i32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..h).rev() {
            for x in 0..w {
                let [r, g, b] = px(x, y);
                out.extend_from_slice(&[b, g, r]); // BMP 是 BGR
            }
            out.resize(out.len() + row_pad, 0u8);
        }
        std::fs::write(path, out).unwrap();
    }

    /// 把 w×h 源图按 D4 变换写成另一张真图片文件（验收要的是「旋转过的文件」进导入
    /// 管线，而不是复用内存里的网格）。
    fn write_bmp_transformed(path: &Path, w: u32, h: u32, t: engine_dedup::D4) {
        use engine_dedup::D4;
        // 顺时针 90°：新 (x,y) = 源 (y, h-1-x)，尺寸 h×w
        let rot90 = |x: u32, y: u32| asym_px(y, h - 1 - x);
        let rot270 = |x: u32, y: u32| asym_px(w - 1 - y, x);
        match t {
            D4::Rot90 => write_bmp_rgb(path, h, w, rot90),
            D4::Rot180 => write_bmp_rgb(path, w, h, |x, y| asym_px(w - 1 - x, h - 1 - y)),
            D4::Rot270 => write_bmp_rgb(path, h, w, rot270),
            D4::MirrorH => write_bmp_rgb(path, w, h, |x, y| asym_px(w - 1 - x, y)),
            other => panic!("本夹具只需 4 旋转 + 水平镜像，收到 {other:?}"),
        }
    }

    fn phash_cols_of(conn: &Connection, aid: i64) -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT pHash, pHashes FROM resource WHERE assetId=?1 AND role='primary'",
            rusqlite::params![aid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// 验收主线：同一张图旋转 90/180/270 与水平镜像后**真文件入库**，
    /// ①每条 primary resource 都落 8 变体（`pHashes` 与 `pHash` identity 一致）；
    /// ②按 8×8 最小汉明距离比对 → 全部判 near，且证据标出正确变换；
    /// ③无关图既不与它们成组、也不被误判为近邻。
    #[test]
    fn import_writes_d4_variants_and_rotation_mirror_judged_near() {
        let _g = crate::vips_lock();
        let root = std::env::temp_dir().join(format!("ki2c_d4_import_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let thumbs = sibling_thumbs(&root);
        std::fs::create_dir_all(&thumbs).unwrap();

        let (w, h) = (64u32, 40u32);
        write_bmp_rgb(&root.join("orig.bmp"), w, h, asym_px);
        write_bmp_transformed(&root.join("rot90.bmp"), w, h, engine_dedup::D4::Rot90);
        write_bmp_transformed(&root.join("rot180.bmp"), w, h, engine_dedup::D4::Rot180);
        write_bmp_transformed(&root.join("rot270.bmp"), w, h, engine_dedup::D4::Rot270);
        write_bmp_transformed(&root.join("mirror.bmp"), w, h, engine_dedup::D4::MirrorH);
        write_bmp_rgb(&root.join("unrelated.bmp"), w, h, unrelated_px);

        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        let batch = create_batch(&conn, &root, "reference", 0).unwrap();
        let out = run_batch(&conn, batch, None, &thumbs, |_| Ok(())).unwrap();
        assert_eq!(out.imported, 6, "六张真图都要入库: {out:?}");
        assert_eq!(out.failed, 0, "{out:?}");

        let rows: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT assetId, fileName FROM assets ORDER BY assetId")
                .unwrap();
            let it = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .unwrap();
            it.collect::<Result<Vec<_>, _>>().unwrap()
        };
        let id_of = |name: &str| -> i64 {
            rows.iter()
                .find(|(_, n)| n == name)
                .unwrap_or_else(|| panic!("{name} 没入库: {rows:?}"))
                .0
        };

        // ① 八变体落库：identity 与老列同值，文本可解析出 8 个 hex
        for (aid, name) in &rows {
            let (phash, phashes) = phash_cols_of(&conn, *aid);
            let phash = phash.unwrap_or_else(|| panic!("{name} 缺 identity pHash"));
            let phashes = phashes.unwrap_or_else(|| panic!("{name} 缺 pHashes 八变体列"));
            let variants = engine_dedup::parse_phashes(&phashes);
            assert_eq!(variants.len(), 8, "{name} 应有 8 个变体: {phashes}");
            assert_eq!(
                engine_dedup::phash_hex_of(variants[0]),
                phash,
                "{name} 的 0 号变体必须等于老 pHash 列"
            );
        }

        // ② 8×8 比对：旋转/镜像都对原件判 near，且变换标注正确
        let variants_of = |aid: i64| -> Vec<u64> {
            let (phash, phashes) = phash_cols_of(&conn, aid);
            engine_dedup::phashes_for_compare(phashes.as_deref(), phash.as_deref())
        };
        let orig = variants_of(id_of("orig.bmp"));
        for (file, t) in [
            ("rot90.bmp", engine_dedup::D4::Rot90),
            ("rot180.bmp", engine_dedup::D4::Rot180),
            ("rot270.bmp", engine_dedup::D4::Rot270),
            ("mirror.bmp", engine_dedup::D4::MirrorH),
        ] {
            let m = engine_dedup::best_variant_match(&orig, &variants_of(id_of(file)))
                .expect("两侧都有 pHash");
            println!(
                "[KI-2c] {file}: 最小汉明距离 {} → {}（a 变体 #{} / b 变体 #{}）",
                m.distance,
                m.transform.label(),
                m.a_variant,
                m.b_variant
            );
            assert!(
                m.distance <= engine_dedup::PHASH_THRESHOLD,
                "{file} 应判近似重复（最小汉明距离 {}）",
                m.distance
            );
            assert_eq!(
                m.transform, t,
                "{file} 的变换标注错（{:?}），距离 {}",
                m.transform, m.distance
            );
        }
        // 无关图：最小距离仍 > 阈值（八变体不引入误判）
        let m =
            engine_dedup::best_variant_match(&orig, &variants_of(id_of("unrelated.bmp"))).unwrap();
        assert!(
            m.distance > engine_dedup::PHASH_THRESHOLD,
            "无关图不该判近邻，最小距离 {}",
            m.distance
        );

        // ③ 分组层：原件 + 4 个变体成 1 组，无关图不进组
        let members: Vec<engine_dedup::DupMemberD4> = rows
            .iter()
            .map(|(aid, _)| {
                let (phash, phashes) = phash_cols_of(&conn, *aid);
                let file_hash = conn
                    .query_row(
                        "SELECT fileHash FROM assets WHERE assetId=?1",
                        rusqlite::params![aid],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .unwrap();
                engine_dedup::DupMemberD4::from_columns(
                    *aid,
                    file_hash,
                    phashes.as_deref(),
                    phash.as_deref(),
                )
            })
            .collect();
        let groups = engine_dedup::build_dup_groups_d4(&members, engine_dedup::PHASH_THRESHOLD);
        assert_eq!(groups.len(), 1, "只该有一组（旋转/镜像全家）: {groups:?}");
        let mut got: Vec<String> = groups[0]
            .members
            .iter()
            .map(|&i| rows[i].1.clone())
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                "mirror.bmp",
                "orig.bmp",
                "rot180.bmp",
                "rot270.bmp",
                "rot90.bmp"
            ],
            "组内应恰好是原件 + 4 个变体"
        );
        let labels: Vec<String> = groups[0].edges.iter().map(|e| e.evidence_label()).collect();
        for t in [
            engine_dedup::D4::Rot90,
            engine_dedup::D4::Rot180,
            engine_dedup::D4::Rot270,
            engine_dedup::D4::MirrorH,
        ] {
            assert!(
                labels.iter().any(|l| l.contains(t.label())),
                "组证据里缺「{}」: {labels:?}",
                t.label()
            );
        }

        // 老数据兼容：把某条的 pHashes 抹成 NULL（模拟老库），只留 pHash 也要能参与比对
        conn.execute(
            "UPDATE resource SET pHashes = NULL WHERE assetId = ?1",
            rusqlite::params![id_of("rot90.bmp")],
        )
        .unwrap();
        let legacy = variants_of(id_of("rot90.bmp"));
        assert_eq!(legacy.len(), 1, "老库回落到单值 = identity 变体");
        assert!(engine_dedup::best_variant_match(&orig, &legacy).is_some());

        std::fs::remove_dir_all(&root).unwrap();
    }
}
