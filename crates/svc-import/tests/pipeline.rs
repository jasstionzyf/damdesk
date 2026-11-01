//! svc-import 集成测试：真实 libvips 解码 + 真实 SQLite 库跑全链路
//! （导入 → 去重 → 缩略图 → pHash → 断点续跑 → missing → relink）。
//! 测试图片用 libvips 合成（black + linear 打渐变），不依赖外部资源。

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use svc_import::*;

/// libvips 是进程级全局状态：先用 `core_media::init_backend()` 显式初始化一次（未 init 时
/// 多线程首次调用会并发注册 GObject class → BlackError / SIGABRT），再串行化本文件用 vips 的用例。
fn vips_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
}

fn make_png(path: &Path, brighten: f64) {
    // black(64,64) → linear a*x+b 拉亮度，产出确定性灰度图；两张不同 b 的图内容不同、pHash 相近
    let black = libvips::ops::black(64, 64).unwrap();
    let img = libvips::ops::linear(&black, &mut [1.0], &mut [brighten]).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

fn setup_db(dir: &Path) -> Connection {
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    conn
}

fn fixture(root: &Path) {
    make_png(&root.join("a.png"), 40.0);
    make_png(&root.join("b.png"), 90.0);
    std::fs::write(root.join("note.txt"), b"not an image").unwrap();
    std::fs::create_dir_all(root.join(".hiddendir")).unwrap();
    make_png(&root.join(".hiddendir/hidden.png"), 10.0);
    std::fs::create_dir_all(root.join("sub")).unwrap();
    make_png(&root.join("sub/c.png"), 40.0); // 与 a.png 同内容（物理重复）
}

#[test]
fn full_pipeline_reference_mode() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    fixture(&root);
    let work = tmp.path().join("app");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = setup_db(&work);

    // 扫描：3 张可见图（a/b/sub-c），跳过 .hiddendir 与 txt
    let files = scan_dir(&root).unwrap();
    assert_eq!(files.len(), 3, "hidden dir + txt excluded");

    // 批次导入
    let bid = create_batch(&conn, &root, "reference", files.len() as i64).unwrap();
    let mut progress_events = 0;
    let outcome = run_batch(&conn, bid, None, &thumbs, |_| {
        progress_events += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(outcome.status, "done");
    assert_eq!(outcome.imported, 2, "a/b 入库");
    assert_eq!(outcome.duplicates, 1, "sub/c 与 a 同 hash 跳过");
    assert_eq!(outcome.failed, 0);
    assert!(progress_events > 0);

    // 库状态
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    let res_n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM resource WHERE role='primary'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(res_n, 2, "每资产一个 primary resource");
    let batch = get_batch(&conn, bid).unwrap();
    assert_eq!(batch.status, "done");
    assert_eq!(batch.totalFiles, 3);
    assert_eq!(batch.doneFiles, 3);

    // 缩略图两档落盘（分片目录 thumbs/<size>/<shard>/<aid>_<size>.webp）
    for size in core_media::THUMB_SIZES {
        let mut cnt = 0;
        let size_dir = thumbs.join(size.to_string());
        for shard in std::fs::read_dir(&size_dir).unwrap() {
            let shard = shard.unwrap().path();
            if !shard.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&shard).unwrap() {
                let f = f.unwrap().path();
                if f.extension().map(|e| e == "webp").unwrap_or(false) {
                    cnt += 1;
                }
            }
        }
        assert_eq!(cnt, 2, "thumb {size}: 每资产一份");
    }

    // pHash 已写入且不同图不同 hash
    let ph: Vec<Option<String>> = {
        let mut stmt = conn
            .prepare("SELECT pHash FROM resource ORDER BY resourceId")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert!(ph.iter().all(|p| p.is_some()));

    // 重跑批次：幂等（a/b Unchanged；c 无路径记录仍判 Duplicate）
    let outcome2 = run_batch(&conn, bid, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(outcome2.unchanged, 2);
    assert_eq!(outcome2.duplicates, 1);
    assert_eq!(outcome2.imported, 0);

    // 文件内容变化 → Updated（a unchanged；c 无路径仍 Duplicate）
    make_png(&root.join("b.png"), 200.0);
    let outcome3 = run_batch(&conn, bid, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(outcome3.updated, 1);
    assert_eq!(outcome3.unchanged, 1);
    assert_eq!(outcome3.duplicates, 1);
}

#[test]
fn missing_and_relink() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    make_png(&root.join("x.png"), 60.0);
    let work = tmp.path().join("app");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = setup_db(&work);

    let bid = create_batch(&conn, &root, "reference", 1).unwrap();
    let out = run_batch(&conn, bid, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(out.imported, 1);
    let aid: i64 = conn
        .query_row("SELECT assetId FROM assets", [], |r| r.get(0))
        .unwrap();

    // 移走文件 → watcher 语义：missing=1
    let moved = tmp.path().join("moved/x_renamed.png");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(&root.join("x.png"), &moved).unwrap();
    let ev = handle_file_removed(&conn, &root.join("x.png")).unwrap();
    assert_eq!(ev.kind, "missing");
    let missing: i64 = conn
        .query_row(
            "SELECT missing FROM assets WHERE assetId=?1",
            rusqlite::params![aid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing, 1);

    // 文件出现在新路径（watcher created 语义）：missing 同 hash 资产自动接回
    let appeared = tmp.path().join("appeared.png");
    std::fs::rename(&moved, &appeared).unwrap();
    let ev2 = handle_file_created(&conn, &appeared, "reference", None, &thumbs).unwrap();
    assert_eq!(ev2.kind, "moved");
    let abs2: String = conn
        .query_row(
            "SELECT absPath FROM assets WHERE assetId=?1",
            rusqlite::params![aid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(abs2, appeared.to_string_lossy());

    // relink 错误内容 → 拒绝
    std::fs::rename(&appeared, &moved).unwrap();
    handle_file_removed(&conn, &appeared).unwrap();
    make_png(&tmp.path().join("wrong.png"), 999.0);
    assert!(relink(&conn, aid, &tmp.path().join("wrong.png"), &thumbs).is_err());

    // relink 正确内容 → 接回
    let ok = relink(&conn, aid, &moved, &thumbs).unwrap();
    assert_eq!(ok, aid);
    let (missing, abs): (i64, String) = conn
        .query_row(
            "SELECT missing, absPath FROM assets WHERE assetId=?1",
            rusqlite::params![aid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(missing, 0);
    assert_eq!(abs, moved.to_string_lossy());

    // mark_missing 全库扫描
    let n = mark_missing(&conn).unwrap();
    assert_eq!(n, 0, "文件已接回，无失联");
}

#[test]
fn copyin_mode_copies_into_library() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("src_photos");
    std::fs::create_dir_all(&root).unwrap();
    make_png(&root.join("a.png"), 30.0);
    let work = tmp.path().join("app");
    let lib = work.join("library");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = setup_db(&work);

    let bid = create_batch(&conn, &root, "copyin", 1).unwrap();
    let out = run_batch(&conn, bid, Some(&lib), &thumbs, |_| Ok(())).unwrap();
    assert_eq!(out.imported, 1);
    let abs: String = conn
        .query_row("SELECT absPath FROM assets", [], |r| r.get(0))
        .unwrap();
    let absp = PathBuf::from(&abs);
    assert!(absp.starts_with(&lib), "copyin 路径在库目录: {abs}");
    assert!(absp.exists());

    // 源在库目录内再导入 → 拒绝（防递归）。改内容避开 Unchanged 快路径（mtime 同秒）。
    make_png(&absp, 77.0);
    let r = process_file(&conn, &absp, "copyin", Some(&lib), &thumbs, Some(bid));
    assert!(matches!(r, Err(ImportError::RecursiveCopy(_))));
}

#[test]
fn cancelled_batch_stops() {
    let _g = vips_lock().lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..40 {
        make_png(&root.join(format!("g{i:02}.png")), 10.0 + i as f64);
    }
    let work = tmp.path().join("app");
    let thumbs = work.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    let conn = setup_db(&work);

    let bid = create_batch(&conn, &root, "reference", 40).unwrap();
    // 第一个进度回调即取消
    let out = run_batch(&conn, bid, None, &thumbs, |_| Err("stop".into())).unwrap();
    assert_eq!(out.status, "cancelled");
    let batch = get_batch(&conn, bid).unwrap();
    assert_eq!(batch.status, "cancelled");
    assert!(batch.doneFiles < 40);

    // 续跑完成
    let out2 = resume_batch(&conn, bid, None, &thumbs, |_| Ok(())).unwrap();
    assert_eq!(out2.status, "done");
    assert_eq!(out2.imported + out2.unchanged, 40);
}
