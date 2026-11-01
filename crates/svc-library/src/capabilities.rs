//! WEBCAP：运行形态能力开关（capability flags）的**单一事实源**。
//!
//! 为什么落在 L2：`/api/capabilities` 报给前端的 `writableRoots`、以及
//! `POST /api/library/move` 的前置拦截用的必须是**同一套判定**（可写根清单 / 逐根
//! 可写性 / 文件系统设备号）。写两处必然漂移，所以只在这里算一次。
//!
//! 事实纪律（对应 SKILL.md「web 形态能力边界」）：
//! - 可写性 = 真在目录里建一个临时文件、写一字节、删掉（不是看 `metadata().permissions()`）
//! - 跨设备 = 真比 `stat().st_dev`（Windows 用卷序列号），不是按路径猜
//! - 本模块**不产出用户文案**：文案按运行形态（桌面 / 网页容器）在 `app-web` 改写，
//!   口径见 `app_web::library_api::humanize_fs_error`（② 落地的保底文案）。
//!
//! 两层防护并存（不替代、不删除）：
//! - ① 本模块的 `precheck_move`：提交前拒绝（HTTP 4xx + `code`），不产生半途效果
//! - ② `move_assets` 的逐条 `classify_fs_error`：兜底（预检后磁盘状态仍可能变）

use rusqlite::Connection;
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

/// 一个库根（数据目录 / 导入批次根路径）的**实测**状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInfo {
    pub path: String,
    /// 实测可写（真建临时文件再删）
    pub writable: bool,
    /// 不可写时的分类 code（`read_only_fs` / `permission_denied` / `not_found` / `other`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 底层原始错误（排查/报告用，不当用户文案）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// 文件系统设备号（跨设备 rename 判定；拿不到 = None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<u64>,
}

/// 用户可见能力取值。**语义**：`false` = 本形态不对用户承诺该操作（UI 灰置/隐藏），
/// 不是「技术上绝对做不到」——网页形态服务端卷内技术上仍可 rename，但库根属于服务器
/// 磁盘，对用户没有意义（见 SKILL.md「web 形态能力边界」）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    /// 移动到目录 / 拖拽到目录树（磁盘 rename）
    pub move_asset: bool,
    /// 物理重命名源文件
    pub rename_asset: bool,
    /// 失联文件重新定位（relink）
    pub relink: bool,
    /// 在 Finder / Explorer 中打开（JSON key 保持 `openInOS`：camelCase 会把 OS 变成 Os，
    /// 前端契约按 `openInOS` 写，故显式指定）
    #[serde(rename = "openInOS")]
    pub open_in_os: bool,
    /// 物理删除磁盘文件（软删/回收站不算，那个两形态都有）
    pub physical_delete: bool,
    /// 复制入库（copy-in）：桌面=本机目录，网页=服务端可写卷内 —— 两形态都可用
    pub copy_in_import: bool,
    /// 能否读用户本机任意路径（库外文件夹扫描：只读、不入库）。
    ///
    /// KI-7：网页/容器形态的「本地路径」是**服务器**磁盘，用户给的库外路径服务端既
    /// 读不到也不该读（跨信任边界）→ false，UI 直接隐藏入口（不灰置）；
    /// 桌面形态与 `openInOS` 同源（都是 `isDesktop`）。
    pub scan_external_path: bool,
}

/// `GET /api/capabilities` 的完整契约（前端零形态分支：只读这里）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// 是否桌面形态（Tauri 壳 / `local-infer` 构建 / `DAM_WEB_FORM=desktop` 验收钩子）
    pub is_desktop: bool,
    /// 库文件归属口径：`local-disk`（用户本机磁盘）/ `server-volume`（服务器卷）
    pub store_mode: String,
    /// 可写库根（`roots` 里 writable=true 的 path，顺序同 `roots`）
    pub writable_roots: Vec<String>,
    /// 全部库根 + 实测状态（含只读根，前端据此逐目录灰置拖放目标）
    pub roots: Vec<RootInfo>,
    pub features: Features,
    /// 被禁用能力的**人话原因**（只含 false 的项；true 项不出现）
    pub feature_reasons: std::collections::BTreeMap<String, String>,
}

/// 前置拦截失败：`code` 是稳定分类，`message` 是底层/技术性说明（交给 app-web 按形态人化）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveReject {
    pub code: String,
    pub message: String,
}

// ---------- 设备号 / 可写性实测 ----------

/// Windows：卷根身份（同卷 ⇒ 同号）。
///
/// 只在 `#[cfg(windows)]` 下编译。返回 `None` 的条件与 `device_id` 一致（路径不存在 /
/// 取不到卷根），调用方一律按「拿不到设备号」处理（不拦、不清洗）。
#[cfg(windows)]
fn volume_root_id(path: &Path) -> Option<u64> {
    use std::path::{Component, Prefix};
    // metadata 在上面已确保路径存在；canonicalize 只为把相对路径补成绝对，并统一成
    // `\\?\C:\...` 形态（Verbatim 前缀同样走 Prefix 分支）。
    let abs = std::fs::canonicalize(path).ok()?;
    let first = abs.components().next()?;
    let kind = match first {
        Component::Prefix(p) => p.kind(),
        // Windows 绝对路径必有盘符 / UNC；没有 Prefix ⇒ 拿不到卷
        _ => return None,
    };
    let key = match kind {
        // 盘符：直接用 ASCII 大写字母做身份（'C' = 67），可读、稳定、不碰撞。
        Prefix::Disk(d) | Prefix::VerbatimDisk(d) => (d as char).to_ascii_uppercase() as u64,
        // UNC / 设备命名空间 / Verbatim：对卷根字符串做 FNV-1a，再置高位置 1，
        // 与盘符值（恒 < 0x80）区分开。
        other => {
            let s = format!("{:?}", other).to_ascii_lowercase();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in s.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            h | 0x8000_0000_0000_0000
        }
    };
    Some(key)
}

/// 文件系统设备号（同号 = 可 `rename`，不同号 = 跨设备必然 EXDEV）。
pub fn device_id(path: &Path) -> Option<u64> {
    let md = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(md.dev())
    }
    #[cfg(windows)]
    {
        let _ = &md;
        // 稳定版 std 拿不到卷序列号（`MetadataExt::volume_serial_number` 属 unstable
        // `windows_by_handle`，需 nightly；见 rust-lang#63010），这里退一步用**卷根**
        // 做身份：同盘符 ⇒ 同卷 ⇒ rename 可行；不同盘符 / 不同 UNC 共享 ⇒ 跨卷 ⇒ 必然
        // EXDEV。目录联接 / 挂载点（同一卷挂到另一个目录）判不出来 —— 该情形真 rename
        // 仍会成功，属于保守方向（多放行、不误拦），不会误报跨设备。
        volume_root_id(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = md;
        None
    }
}

/// 最近存在的祖先（目标目录可能还不存在 —— `move_assets` 会 `create_dir_all`）。
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut p = path.to_path_buf();
    loop {
        if p.exists() {
            return Some(p);
        }
        match p.parent() {
            Some(parent) if parent != p => p = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// 目录可写性**实测**：在 `dir`（或最近存在的祖先）里建临时文件 → 写 → 删。
/// 返回 `Err((code, raw))`，code 走 `classify_fs_error` 同一套分类。
pub fn probe_writable(dir: &Path) -> Result<(), (String, String)> {
    let Some(base) = nearest_existing(dir) else {
        return Err((
            "not_found".to_string(),
            format!("路径不存在且无存在的祖先目录：{}", dir.display()),
        ));
    };
    if !base.is_dir() {
        return Err((
            "other".to_string(),
            format!("不是目录：{}", base.display()),
        ));
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = base.join(format!(
        ".dam-write-probe-{}-{}",
        std::process::id(),
        seq
    ));
    let mut f = match std::fs::File::create(&probe) {
        Ok(f) => f,
        Err(e) => {
            return Err((
                classify_code(&e).to_string(),
                format!("写探针创建失败 @ {}：{e}", base.display()),
            ))
        }
    };
    if let Err(e) = std::io::Write::write_all(&mut f, b"1") {
        drop(f);
        let _ = std::fs::remove_file(&probe);
        return Err((
            classify_code(&e).to_string(),
            format!("写探针写入失败 @ {}：{e}", base.display()),
        ));
    }
    drop(f);
    if let Err(e) = std::fs::remove_file(&probe) {
        return Err((
            classify_code(&e).to_string(),
            format!("写探针清理失败 @ {}：{e}", base.display()),
        ));
    }
    Ok(())
}

/// `classify_fs_error` 的间接调用（保持分类口径唯一，不在本模块另写一套映射）。
fn classify_code(e: &std::io::Error) -> &'static str {
    crate::classify_fs_error(e)
}

// ---------- 库根清单 ----------

/// 库根清单：数据目录（恒在首位）+ 全部导入批次根路径（去重、去空）。
/// 顺序稳定 → `writableRoots` 顺序稳定（报告/断言可比）。
pub fn library_roots(conn: &Connection, data_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![data_dir.to_path_buf()];
    let batches = crate::import_batches(conn).unwrap_or_default();
    for b in batches {
        let p = b.rootPath.trim().trim_end_matches('/').to_string();
        if p.is_empty() {
            continue;
        }
        let pb = PathBuf::from(&p);
        if !out.iter().any(|x| x == &pb) {
            out.push(pb);
        }
    }
    out
}

/// 逐根实测可写性 + 设备号（`roots` 与入参同序）。
pub fn probe_roots(roots: &[PathBuf]) -> Vec<RootInfo> {
    roots
        .iter()
        .map(|r| {
            let path = r.to_string_lossy().to_string();
            let device = device_id(r);
            match probe_writable(r) {
                Ok(()) => RootInfo {
                    path,
                    writable: true,
                    code: None,
                    raw: None,
                    device,
                },
                Err((code, raw)) => RootInfo {
                    path,
                    writable: false,
                    code: Some(code),
                    raw: Some(raw),
                    device,
                },
            }
        })
        .collect()
}

/// 可写库根路径列表（单一事实源，端点与前置拦截共用）。
pub fn writable_roots(roots: &[RootInfo]) -> Vec<String> {
    roots
        .iter()
        .filter(|r| r.writable)
        .map(|r| r.path.clone())
        .collect()
}

/// `target` 是否落在 `root` 内（按路径段，边界安全：`/data` 不含 `/database`）。
pub fn path_inside(target: &Path, root: &str) -> bool {
    let root_path = Path::new(root.trim_end_matches('/'));
    if root == "/" || root.trim_end_matches('/').is_empty() {
        return target.is_absolute();
    }
    target.starts_with(root_path)
}

/// 能力取值（真值来自实测 roots + 形态，不硬编码）。
pub fn evaluate_features(is_desktop: bool, roots: &[RootInfo]) -> Features {
    let any_writable = roots.iter().any(|r| r.writable);
    Features {
        move_asset: is_desktop && any_writable,
        rename_asset: is_desktop && any_writable,
        relink: is_desktop,
        open_in_os: is_desktop,
        physical_delete: is_desktop && any_writable,
        copy_in_import: true,
        // 库外扫描 = 读用户本机磁盘：只有桌面形态成立（见 Features 字段注释）
        scan_external_path: is_desktop,
    }
}

/// 组装完整能力契约（端点唯一入口）。
pub fn capabilities(is_desktop: bool, roots: Vec<RootInfo>) -> Capabilities {
    let features = evaluate_features(is_desktop, &roots);
    Capabilities {
        is_desktop,
        store_mode: if is_desktop { "local-disk" } else { "server-volume" }.to_string(),
        writable_roots: writable_roots(&roots),
        roots,
        features,
        // 原因文案按形态在 app-web 填（本层只出 code/raw）
        feature_reasons: Default::default(),
    }
}

// ---------- ① 移动前置拦截 ----------

/// `POST /api/library/move` 的前置校验（在**任何磁盘副作用之前**跑）。
///
/// 规则（与 `/api/capabilities` 同源）：
/// 1. 目标必须是绝对路径、不含 `..` → 否则 `bad_path`
/// 2. 目标（最近存在的祖先）必须实测可写 → 否则 `read_only_fs` / `permission_denied`
/// 3. 目标必须落在**可写库根**内 → 否则 `not_writable_root`
/// 4. 源文件与目标必须同一文件系统 → 否则 `cross_device`（整批拒绝，不留半途效果）
///
/// 通过则返回实测 roots（调用方可复用，不重复探针）。
pub fn precheck_move(
    conn: &Connection,
    asset_ids: &[i64],
    target_dir: &str,
    data_dir: &Path,
) -> Result<Vec<RootInfo>, MoveReject> {
    let target = Path::new(target_dir);
    if !target.is_absolute() || target.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(MoveReject {
            code: "bad_path".to_string(),
            message: format!("目标目录必须是绝对路径且不含 `..`：{target_dir}"),
        });
    }

    let roots = probe_roots(&library_roots(conn, data_dir));
    let writable = writable_roots(&roots);
    if writable.is_empty() {
        return Err(MoveReject {
            code: "read_only_fs".to_string(),
            message: format!(
                "没有可写库根（{}）",
                roots
                    .iter()
                    .map(|r| format!(
                        "{}={}",
                        r.path,
                        r.code.as_deref().unwrap_or("unknown")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    // 2) 目标可写性实测（先于库根归属：/corpus 这类只读挂载给具体 fs 归因）
    if let Err((code, raw)) = probe_writable(target) {
        return Err(MoveReject {
            code,
            message: raw,
        });
    }

    // 3) 必须落在可写库根内
    if !writable
        .iter()
        .any(|root| path_inside(target, root.as_str()))
    {
        return Err(MoveReject {
            code: "not_writable_root".to_string(),
            message: format!(
                "目标目录不在可写库根内：{target_dir}（可写根：{}）",
                writable.join(", ")
            ),
        });
    }

    // 4) 跨设备：源文件设备号必须与目标目录一致（整批拒绝）
    if let Some(target_dev) = nearest_existing(target).as_deref().and_then(device_id) {
        let mut offenders: Vec<String> = Vec::new();
        for &id in asset_ids {
            let abs: Option<String> = conn
                .query_row(
                    "SELECT absPath FROM assets WHERE assetId=?1 AND deleted=0",
                    [id],
                    |r| r.get(0),
                )
                .ok();
            if let Some(abs) = abs {
                if let Some(dev) = device_id(Path::new(&abs)) {
                    if dev != target_dev {
                        offenders.push(abs);
                    }
                }
            }
        }
        if !offenders.is_empty() {
            let sample = offenders
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(MoveReject {
                code: "cross_device".to_string(),
                message: format!(
                    "{} 个源文件与目标目录不在同一文件系统（设备号 {} ≠ 目标 {}）：{}",
                    offenders.len(),
                    device_id(Path::new(&offenders[0])).unwrap_or(0),
                    target_dev,
                    sample
                ),
            });
        }
    }

    Ok(roots)
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "dam-cap-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn probe_writable_true_on_writable_dir_and_false_for_missing_root() {
        let d = tmpdir("probe");
        assert!(probe_writable(&d).is_ok(), "普通目录应实测可写");
        // 探针文件必须被清掉（不留垃圾）
        let leftovers = fs::read_dir(&d).unwrap().count();
        assert_eq!(leftovers, 0, "探针文件未清理");
        // 不存在的目标目录：最近存在的祖先天可写 → 允许（move_assets 会 create_dir_all）
        assert!(probe_writable(&d.join("sub/deeper")).is_ok());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn path_inside_is_segment_boundary_safe() {
        assert!(path_inside(Path::new("/p/x"), "/p"));
        assert!(path_inside(Path::new("/p"), "/p"));
        assert!(path_inside(Path::new("/p/x"), "/p/"));
        assert!(!path_inside(Path::new("/database/x"), "/p"));
        assert!(!path_inside(Path::new("/corpus"), "/p"));
        assert!(path_inside(Path::new("/anything"), "/"));
    }

    #[test]
    fn features_are_evaluated_from_probe_not_hardcoded() {
        let rw = RootInfo {
            path: "/p".into(),
            writable: true,
            code: None,
            raw: None,
            device: Some(1),
        };
        let ro = RootInfo {
            path: "/corpus".into(),
            writable: false,
            code: Some("read_only_fs".into()),
            raw: Some("ro".into()),
            device: Some(2),
        };
        // 网页形态：磁盘类能力一律不对用户承诺（即使卷内可写）
        let web = evaluate_features(false, &[rw.clone(), ro.clone()]);
        assert!(!web.move_asset && !web.rename_asset && !web.relink && !web.open_in_os);
        assert!(!web.physical_delete);
        assert!(web.copy_in_import);
        // KI-7：库外扫描 = 读用户本机磁盘 → 网页形态必须 false（UI 隐藏入口，不灰置）
        assert!(!web.scan_external_path, "容器/网页形态不得承诺读本机库外路径");
        // 桌面形态 + 有可写根：磁盘类能力全部可用
        let desk = evaluate_features(true, &[rw.clone(), ro.clone()]);
        assert!(desk.move_asset && desk.rename_asset && desk.relink && desk.open_in_os);
        assert!(desk.physical_delete);
        // 桌面形态但库根全只读：磁盘写操作不可用（真值来自实测）
        let desk_ro = evaluate_features(true, &[ro]);
        assert!(!desk_ro.move_asset && !desk_ro.physical_delete);
        assert!(desk_ro.relink && desk_ro.open_in_os, "只读不影响读类能力");
        // 契约字段：可写根从 roots 推导，storeMode 跟形态
        let caps = capabilities(false, vec![rw]);
        assert_eq!(caps.store_mode, "server-volume");
        assert_eq!(caps.writable_roots, vec!["/p".to_string()]);
    }

    #[test]
    fn json_contract_keys_are_exact() {
        // 前端按这组 key 读（web/src/lib/api.ts::Capabilities）—— key 名变了前端就静默失效，
        // 所以在单测里钉死（`openInOS` 曾被 camelCase 磨成 openInOs，实战踩过）。
        let caps = capabilities(
            false,
            vec![RootInfo {
                path: "/p".into(),
                writable: true,
                code: None,
                raw: None,
                device: Some(1),
            }],
        );
        let j = serde_json::to_value(&caps).unwrap();
        for k in [
            "isDesktop",
            "storeMode",
            "writableRoots",
            "roots",
            "features",
            "featureReasons",
        ] {
            assert!(j.get(k).is_some(), "Capabilities 缺字段 {k}: {j}");
        }
        for k in [
            "moveAsset",
            "renameAsset",
            "relink",
            "openInOS",
            "physicalDelete",
            "copyInImport",
            "scanExternalPath",
        ] {
            assert!(j["features"].get(k).is_some(), "features 缺 {k}: {}", j["features"]);
        }
        assert!(j["roots"][0].get("writable").is_some(), "RootInfo 缺 writable: {j}");
        assert!(j["roots"][0].get("device").is_some(), "RootInfo 缺 device: {j}");
    }

    #[test]
    fn precheck_rejects_bad_path_and_outside_root_and_cross_device() {
        let data = tmpdir("pre");
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        // 目标相对路径 → bad_path
        let e = precheck_move(&conn, &[], "rel/dir", &data).unwrap_err();
        assert_eq!(e.code, "bad_path", "{}", e.message);
        // 目标可写但不在库根内 → not_writable_root
        let outside = tmpdir("outside");
        let e = precheck_move(&conn, &[], outside.to_str().unwrap(), &data).unwrap_err();
        assert_eq!(e.code, "not_writable_root", "{}", e.message);
        // 库根内 + 同设备 → 通过
        let inside = data.join("lib");
        fs::create_dir_all(&inside).unwrap();
        assert!(precheck_move(&conn, &[], inside.to_str().unwrap(), &data).is_ok());

        // 跨设备：找一个与 data 不同 st_dev 且**可写**的现存目录（容器里 /corpus 是只读挂载，
        // /dev/shm 是 tmpfs —— 有就断言 cross_device，没有就明确跳过，不假装通过）
        let data_dev = device_id(&data).unwrap();
        let mut tried: Vec<String> = Vec::new();
        let mut done = false;
        for cand in ["/dev/shm", "/corpus", "/data1", "/tmp"] {
            let dir = Path::new(cand);
            let differs = dir.is_dir() && device_id(dir).map(|d| d != data_dev).unwrap_or(false);
            if !differs {
                continue;
            }
            let src = dir.join(format!("dam-cap-xdev-{}.jpg", std::process::id()));
            if let Err(e) = fs::write(&src, b"x") {
                tried.push(format!("{cand}={e}"));
                continue;
            }
            conn.execute(
                "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES ('image', ?1, 'x.jpg', 'jpg', 1, unixepoch(), unixepoch())",
                rusqlite::params![src.to_str().unwrap()],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            let e = precheck_move(&conn, &[id], inside.to_str().unwrap(), &data).unwrap_err();
            assert_eq!(e.code, "cross_device", "{}", e.message);
            fs::remove_file(&src).ok();
            done = true;
            break;
        }
        if !done {
            eprintln!("跳过 cross_device 断言：没有可写的跨设备目录可试（{tried:?}）");
        }
        fs::remove_dir_all(&data).ok();
        fs::remove_dir_all(&outside).ok();
    }
}
