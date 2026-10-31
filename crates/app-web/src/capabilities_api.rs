//! WEBCAP：`GET /api/capabilities` —— 运行形态能力开关（flag 由服务端算，前端零形态分支）。
//!
//! 分层位置：L3 进程壳（薄壳）。判定全在 `svc_library::capabilities`（L2，单一事实源），
//! 这里只做三件事：
//! 1. 把形态信号（桌面构建 / `DAM_WEB_FORM=desktop` 验收钩子）传下去
//! 2. 用 `humanize_fs_error` 的同一口径补 `featureReasons`（人话原因，只填 false 的项）
//! 3. 序列化
//!
//! 契约（与前端 `web/src/lib/caps.ts` 对齐）：
//! `{ isDesktop, storeMode, writableRoots[], roots[{path,writable,code?,raw?,device?}],
//!    features{moveAsset,renameAsset,relink,openInOS,physicalDelete,copyInImport,scanExternalPath},
//!    featureReasons{moveAsset?...} }`

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::library_api::humanize_fs_error;
use crate::{is_desktop_form, AppState};

/// 被禁用能力的人话原因（复用 ② 的文案口径：指向桌面版 / copy-in，不说黑话）。
fn reasons(is_desktop: bool, roots: &[svc_library::capabilities::RootInfo]) -> BTreeMap<String, String> {
    let feats = svc_library::capabilities::evaluate_features(is_desktop, roots);
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: String| {
        m.insert(k.to_string(), v);
    };
    // 桌面形态但库根全只读 → 说清是「根不可写」，不是「网页版不支持」
    let ro_detail = roots
        .iter()
        .filter(|r| !r.writable)
        .map(|r| format!("{}（{}）", r.path, r.code.as_deref().unwrap_or("other")))
        .collect::<Vec<_>>()
        .join("、");
    if !feats.move_asset {
        put(
            "moveAsset",
            if is_desktop {
                format!("无法移动：当前库根不可写 {ro_detail}，请改选可写磁盘内的目录。")
            } else {
                // 与 ② 完全同一句（网页形态兜底文案），一处口径两处复用
                humanize_fs_error(false, "cross_device", "")
            },
        );
    }
    if !feats.rename_asset {
        put(
            "renameAsset",
            if is_desktop {
                format!("无法重命名：当前库根不可写 {ro_detail}。")
            } else {
                "网页版不支持在磁盘上重命名文件（库根属于服务器磁盘）。请在桌面版操作。".to_string()
            },
        );
    }
    if !feats.relink {
        put(
            "relink",
            "网页版无法重新定位失联文件（服务端路径由导入批次决定）。请在桌面版操作，或重新导入该目录。"
                .to_string(),
        );
    }
    if !feats.open_in_os {
        put("openInOS", open_in_os_reason());
    }
    if !feats.physical_delete {
        put(
            "physicalDelete",
            if is_desktop {
                format!("无法物理删除：当前库根不可写 {ro_detail}。可改用「移入回收站」（软删可恢复）。")
            } else {
                "网页版不支持物理删除磁盘文件；请用「移入回收站」（软删可恢复）。".to_string()
            },
        );
    }
    if !feats.scan_external_path {
        put(
            "scanExternalPath",
            "网页版无法读取你电脑上的文件夹（文件在服务器上）。请在桌面版扫描库外文件夹查重。"
                .to_string(),
        );
    }
    m
}

/// `features.openInOS=false` 的人话原因（`reasons()` 与 `/api/os/reveal` 的 400 文案同一句）：
/// 一处口径两处复用，避免前端 hover 提示与接口报错说两套话。
pub fn open_in_os_reason() -> String {
    "网页版无法在系统文件管理器（Finder/Explorer）中打开——文件在服务器上。".to_string()
}

/// GET /api/capabilities — 当前形态的能力开关 + 库根实测状态。
pub async fn capabilities(State(state): State<AppState>) -> impl IntoResponse {
    let is_desktop = is_desktop_form();
    let roots = {
        let conn = state.db.lock().await;
        svc_library::capabilities::probe_roots(&svc_library::capabilities::library_roots(
            &conn,
            &state.data_dir,
        ))
    };
    let mut caps = svc_library::capabilities::capabilities(is_desktop, roots);
    caps.feature_reasons = reasons(is_desktop, &caps.roots);
    // KI-28：插件运行时（内置 python / 启动器）的真实落位也要能被界面与支持读到——
    // 「装了但行业包起不来」以前只能靠翻安装目录猜，现在一个字段说清缺没缺。
    let mut body = json!(caps);
    body["packRuntime"] = state.pack_runtime.describe();
    (StatusCode::OK, Json(body)).into_response()
}
