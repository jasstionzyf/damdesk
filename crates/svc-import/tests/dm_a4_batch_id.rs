//! DM-A4（卡 task，KI-2 残项）：`assets.batchId` 的写入口径。
//!
//! 迁移 0033 加了这列，`GET /api/dedup/groups?range=batch:<id>` 靠它精确圈范围。
//! 本文件钉住「谁写、什么时候覆盖」这三条（口径写进 svc_import::process_file 文档）：
//!
//! 1. 新入库（Imported）写当次批次；同路径内容变了重写（Updated）**覆盖**成最近一次；
//! 2. 同内容重扫（Unchanged）与同 hash 命中别的资产（Duplicate）**一个资产行都没写** →
//!    不改归属（否则「再导一遍」就把整个老库搬到新批次，批次溯源彻底失真）；
//! 3. watcher 捡回的文件不是导入事件 → `batchId` 保持 NULL。
//!
//! 反向验证：把 `update_asset` 的 `COALESCE` 去掉（即 Unchanged/relink 也覆盖）或把
//! Duplicate 分支也写库，本文件的 `batch_id_stays_with_original_import` 会红。

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use svc_import::*;

/// libvips 是进程级全局状态：先显式初始化一次，再串行化本文件用 vips 的用例
/// （同 crates/svc-import/tests/pipeline.rs 的说明）。
fn vips_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

fn make_png(path: &Path, brighten: f64) {
    // 用带 seed 的高斯噪声而不是纯色：纯色图（哪怕亮度不同）在 pHash 口径下近似同一张，
    // 入库断言会因 libvips 版本/阈值差异翻车（CI 上两张真图塌成 1 张）。
    let opts = libvips::ops::GaussnoiseOptions {
        sigma: 40.0,
        mean: brighten,
        seed: brighten as i32,
    };
    let img = libvips::ops::gaussnoise_with_opts(64, 64, &opts).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

fn setup(dir: &Path) -> (Connection, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let thumbs = dir.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = core_storage::open(&dir.join("dam.db")).unwrap();
    (conn, thumbs)
}

fn batch_id_of(conn: &Connection, path: &Path) -> Option<i64> {
    conn.query_row(
        "SELECT batchId FROM assets WHERE absPath = ?1 AND deleted = 0",
        rusqlite::params![path.to_string_lossy()],
        |r| r.get::<_, Option<i64>>(0),
    )
    .unwrap()
}

fn count_in_batch(conn: &Connection, batch: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE batchId = ?1",
        [batch],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn import_writes_batch_id_and_rewrite_takes_over() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let a = root.join("a.png");
    let b = root.join("b.png");
    make_png(&a, 40.0);
    make_png(&b, 90.0);
    let (conn, thumbs) = setup(&tmp.path().join("app"));

    // 第一次导入：两张都归批次 1
    let b1 = create_batch(&conn, &root, "reference", 2).unwrap();
    let out1 = run_batch(&conn, b1, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!((out1.imported, out1.updated, out1.unchanged), (2, 0, 0));
    assert_eq!(batch_id_of(&conn, &a), Some(b1), "新入库必须写当次批次");
    assert_eq!(batch_id_of(&conn, &b), Some(b1), "新入库必须写当次批次");
    assert_eq!(count_in_batch(&conn, b1), 2);

    // 第二次导入同一目录：a 内容变了（Updated → 覆盖到批次 2），b 没变（Unchanged → 不动）
    make_png(&a, 200.0);
    let b2 = create_batch(&conn, &root, "reference", 2).unwrap();
    let out2 = run_batch(&conn, b2, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(
        (out2.imported, out2.updated, out2.unchanged),
        (0, 1, 1),
        "a 改了内容→Updated，b 没变→Unchanged"
    );
    assert_eq!(
        batch_id_of(&conn, &a),
        Some(b2),
        "同路径内容变了重写 → 批次归属跟着最近一次导入事件走"
    );
    assert_eq!(
        batch_id_of(&conn, &b),
        Some(b1),
        "Unchanged 没有写库 → 不许把老资产搬到新批次"
    );
    assert_eq!(count_in_batch(&conn, b1), 1, "批次 1 只剩没被重写的那张");
    assert_eq!(count_in_batch(&conn, b2), 1, "批次 2 只有被重写的那张");
}

#[test]
fn batch_id_stays_with_original_import() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let a = root.join("a.png");
    make_png(&a, 40.0);
    let (conn, thumbs) = setup(&tmp.path().join("app"));
    let b1 = create_batch(&conn, &root, "reference", 1).unwrap();
    run_batch(&conn, b1, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(batch_id_of(&conn, &a), Some(b1));

    // 另一目录放一张字节完全相同的图 → 第二次导入判 Duplicate（同 hash 命中已有资产）
    let other = tmp.path().join("copy");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(&a, other.join("a-copy.png")).unwrap();
    let b2 = create_batch(&conn, &other, "reference", 1).unwrap();
    let out2 = run_batch(&conn, b2, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(
        out2.duplicates, 1,
        "同内容复制品判 Duplicate（不新增资产行）"
    );
    let assets: i64 = conn
        .query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(assets, 1, "Duplicate 不新增资产行");
    assert_eq!(
        batch_id_of(&conn, &a),
        Some(b1),
        "原资产的批次归属不许被后来的重复导入改写"
    );
    assert_eq!(
        count_in_batch(&conn, b2),
        0,
        "这批一张都没入库 → 本批 0 张（诚实呈现，别把别批的图算进来）"
    );

    // 再导一遍同一批（Unchanged 快路径）同样不改归属
    let out3 = run_batch(&conn, b2, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(out3.duplicates + out3.unchanged, 1);
    assert_eq!(batch_id_of(&conn, &a), Some(b1));
}

#[test]
fn watcher_import_leaves_batch_null() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("watched");
    std::fs::create_dir_all(&root).unwrap();
    let (conn, thumbs) = setup(&tmp.path().join("app"));
    let f = root.join("dropped.png");
    make_png(&f, 40.0);

    let ev = handle_file_created(&conn, &f, "reference", None, &thumbs).unwrap();
    assert_eq!(ev.kind, "imported", "{ev:?}");
    assert_eq!(
        batch_id_of(&conn, &f),
        None,
        "watcher 捡回来的文件不是导入事件 → batchId 留 NULL（不编造归属）"
    );
}
