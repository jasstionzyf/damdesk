//! W2 媒体静态服务：缩略图（按需生成）+ 原图（白名单根校验）。
//!
//! 路由一律按 assetId 索引（路径永远来自 DB，不接受请求携带路径）→ 天然无路径逃逸；
//! 原图额外要求 canonicalize 后落在白名单根（历史批次根 ∪ copyin 库根 ∪ DAM_WEB_MEDIA_ROOTS）
//! 之下（plan §3「absPath 白名单根校验」）。网格只渲染缩略图（桌面版同裁决）。

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn content_type_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        _ => "application/octet-stream",
    }
}

async fn serve_file(path: PathBuf) -> Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type_for(&path)),
                (header::CACHE_CONTROL, "private, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => err(
            StatusCode::NOT_FOUND,
            format!("read {}: {e}", path.display()),
        ),
    }
}

/// 读资产原图路径（deleted 排除）。None = 资产不存在。
async fn asset_abs_path(state: &AppState, asset_id: i64) -> Option<String> {
    let conn = state.db.lock().await;
    conn.query_row(
        "SELECT absPath FROM assets WHERE assetId = ?1 AND deleted = 0",
        [asset_id],
        |r| r.get(0),
    )
    .ok()
}

fn not_found(asset_id: i64) -> Response {
    err(StatusCode::NOT_FOUND, format!("asset {asset_id} not found"))
}

/// D3：缩略图降级占位图（64×64 深灰 + 断线符号，354 字节，随二进制内嵌）。
///
/// 为什么要占位图而不是继续 500：一张坏图 500 会被 `<img>` 当成加载失败 —— 浏览器
/// 只画个破图图标、`onLoad` 永不触发，网格里就是一格"空骨架"，用户既看不出是坏图
/// 也没有任何可点的原因。降级成 200 + 正常图片字节，卡片正常渲染（骨架收掉），
/// 角标「无法预览」由 `X-DAM-Degraded: 1` / 响应体语义在前端承载。
static THUMB_DEGRADED_PNG: &[u8] = include_bytes!("../assets/thumb-degraded.png");

/// 降级响应：200 + 占位图 + 显式降级头（不缓存 —— 修好文件后必须能立刻看到真图）。
/// 具体原因（解码报错串）只进 stderr/日志：header 里放非 ASCII 会破坏响应头解析。
fn degraded_placeholder(reason: &str) -> Response {
    eprintln!("media: thumb degraded → {reason}");
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
            (header::HeaderName::from_static("x-dam-degraded"), "1"),
            (
                header::HeaderName::from_static("x-dam-degraded-reason"),
                "decode_failed",
            ),
        ],
        THUMB_DEGRADED_PNG,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ThumbQuery {
    /// 256（网格）| 1024（大图预览），默认 256
    pub size: Option<i32>,
}

/// GET /api/media/thumb/{assetId}?size=256|1024 — 缩略图（不存在则按需生成 webp）。
///
/// D3：生成失败（源文件不是图片/已损坏/被删）时**不再 500**，改为 200 + 统一占位图
/// + `X-DAM-Degraded: 1`（响应头告诉前端"这是降级图，不是真缩略图"）。
/// 资产不存在（库里的行都没了）仍是 404 —— 那是"没有这个资产"，不是"这张图坏了"，
/// 两种语义不能混。
pub async fn thumb(
    State(state): State<AppState>,
    Path(asset_id): Path<i64>,
    Query(q): Query<ThumbQuery>,
) -> Response {
    let size = q.size.unwrap_or(256);
    if ![256, 1024].contains(&size) {
        return err(
            StatusCode::BAD_REQUEST,
            format!("size must be 256 or 1024, got {size}"),
        );
    }
    let abs = match asset_abs_path(&state, asset_id).await {
        Some(a) => a,
        None => return not_found(asset_id),
    };
    let tp = svc_import::thumb_path(&state.thumbs_root, asset_id, size);
    if !tp.is_file() {
        let src = PathBuf::from(&abs);
        let tp_gen = tp.clone();
        let gen = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(
                tp_gen
                    .parent()
                    .ok_or_else(|| std::io::Error::other("thumb dir missing parent"))?,
            )?;
            core_media::thumbnail_to(&src, &tp_gen, size)
                .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .await;
        match gen {
            Ok(Ok(())) => {}
            // D3：坏图/源文件不在 → 降级占位图（200），不打断整屏
            Ok(Err(e)) => return degraded_placeholder(&e.to_string()),
            Err(e) => return degraded_placeholder(&e.to_string()),
        }
    }
    if !tp.is_file() {
        // 生成"成功"但没落盘（磁盘满/权限）→ 同样降级，不 500
        return degraded_placeholder("thumb not written");
    }
    match tokio::fs::read(&tp).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "private, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => degraded_placeholder(&e.to_string()),
    }
}

/// 白名单根：历史批次根 ∪ copyin 库根 ∪ env DAM_WEB_MEDIA_ROOTS（冒号分隔，运维逃生口）。
fn whitelist_roots(state: &AppState, roots_from_batches: Vec<String>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = roots_from_batches.into_iter().map(PathBuf::from).collect();
    if let Ok(conn) = core_storage::open(&state.data_dir.join("dam.db")) {
        let s: core_config::LibrarySettings =
            core_storage::settings::get_or_default(&conn, core_config::LIBRARY_SETTINGS_KEY)
                .unwrap_or_default();
        roots.push(s.effective_root(&state.data_dir));
    }
    if let Ok(extra) = std::env::var("DAM_WEB_MEDIA_ROOTS") {
        for r in extra.split(':').filter(|s| !s.is_empty()) {
            roots.push(PathBuf::from(r));
        }
    }
    roots
}

/// GET /api/media/original/{assetId} — 原图（下载/查看；白名单根校验）。
pub async fn original(State(state): State<AppState>, Path(asset_id): Path<i64>) -> Response {
    let abs = match asset_abs_path(&state, asset_id).await {
        Some(a) => a,
        None => return not_found(asset_id),
    };
    let path = PathBuf::from(&abs);
    let canon = match std::fs::canonicalize(&path) {
        Ok(c) => c,
        Err(e) => {
            // G5-A：原图链接（分享出去的就是这条 URL）是真实读盘失败点 → 顺手记账。
            // 只标「不在了」（NotFound/ENOENT/ENOTDIR）；权限/其它 IO 错误不标（unreadable 语义）。
            let conn = state.db.lock().await;
            let _ = core_storage::presence::mark_missing_on_error(&conn, asset_id, &e);
            drop(conn);
            return err(StatusCode::NOT_FOUND, format!("file missing: {abs}"));
        }
    };
    let conn = state.db.lock().await;
    // 原图确实可读 → 若此前被标失联（文件回来了），顺手清标
    let _ = core_storage::presence::clear_missing(&conn, asset_id);
    let batch_roots: Vec<String> = conn
        .prepare("SELECT DISTINCT rootPath FROM import_batches")
        .and_then(|mut s| {
            s.query_map([], |r| r.get(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    drop(conn);
    for root in whitelist_roots(&state, batch_roots) {
        if let Ok(rc) = std::fs::canonicalize(&root) {
            if canon.starts_with(&rc) {
                return serve_file(canon).await;
            }
        }
    }
    err(
        StatusCode::FORBIDDEN,
        format!("asset {asset_id} path outside whitelisted roots: {abs}"),
    )
}
