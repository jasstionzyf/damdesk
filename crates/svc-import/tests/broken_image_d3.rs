//! D3：坏图（扩展名对、内容不是图片）的导入校验 + 核查识别/修复。
//!
//! 事故原形（真机）：21 字节的 `c-broken.jpg`（内容 "not an image at all"）
//! 被算成「导入成功 1 张」，落成 assetId 28 —— width=0、`/api/media/thumb/28` 永远 500，
//! 而「重新核查」报「全部文件都在磁盘上」，损坏资产不属于任何异常口径。
//!
//! 本文件的断言口径：
//!   1) 解码校验用**真解码**（不是读文件头）：junk → Err(BadImage)，真图 → Ok；
//!   2) 坏图导入 = failed（进失败清单）+ **不产生资产行**（顺序：先校验再 INSERT）；
//!   3) 核查期解码策略：记录有正尺寸且文件未动的资产**不解码**（成本上限）；
//!   4) 核查能把「存在但解不开」单独计数（broken），并把它写回成 width=0（可筛选）；
//!   5) 文件被换成真图后再核查 → 自动修复尺寸/缩略图，broken 归零。
//!
//! libvips 全局状态不允许并发 VipsInit（class 注册竞争 → SIGABRT），解码类测试串行化。

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use svc_import::*;

fn vips_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| {
        // 未 init 时多线程首次调用 libvips 会并发注册 GObject class → BlackError / SIGABRT，
        // 所以这里既做「显式初始化一次」又串行化用 vips 的用例。
        core_media::init_backend().expect("libvips 初始化失败");
        Mutex::new(())
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// 生成内容真正不同的 64×64 测试图。
///
/// 不能用纯色：纯色图去重口径下 pHash 近似常量，两张不同亮度的纯色会被判成同一张，
/// 于是「两张真图都入库」的断言在 pHash 的库版本/阈值差异下随机翻车（CI 上只有 1 张入库）。
/// 用带 seed 的高斯噪声：AC 系数各自随机 → pHash 不同，也不受 libvips 版本影响。
fn make_png(path: &Path, brighten: f64) {
    let opts = libvips::ops::GaussnoiseOptions {
        sigma: 40.0,
        mean: brighten,
        seed: brighten as i32,
    };
    let img = libvips::ops::gaussnoise_with_opts(64, 64, &opts).unwrap();
    libvips::ops::pngsave(&img, &path.to_string_lossy()).unwrap();
}

const JUNK: &[u8] = b"not an image at all";

fn setup(dir: &Path) -> (Connection, PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let conn = core_storage::open(&dir.join("test.db")).unwrap();
    let thumbs = dir.join("thumbs");
    std::fs::create_dir_all(&thumbs).unwrap();
    (conn, thumbs)
}

fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------- 1) 解码校验本体 ----------

#[test]
fn probe_image_accepts_real_png_and_rejects_junk() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("a.png");
    make_png(&real, 40.0);
    let (w, h, luma) = probe_image(&real).expect("真图必须解得出");
    assert_eq!((w, h), (64, 64));
    assert_eq!(luma.len(), 1024, "luma32 契约：32×32 = 1024 字节");

    let junk = tmp.path().join("c-broken.jpg");
    std::fs::write(&junk, JUNK).unwrap();
    match probe_image(&junk) {
        Err(ImportError::BadImage(msg)) => {
            assert!(msg.contains("c-broken.jpg"), "错误串要带路径：{msg}");
        }
        other => panic!("junk .jpg 必须判 BadImage，实际: {other:?}"),
    }

    // 空文件同样是坏图（不能因为「读元数据成功」就放过）
    let empty = tmp.path().join("empty.jpg");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(probe_image(&empty), Err(ImportError::BadImage(_))));
}

// ---------- 2) 导入：坏图 failed 且不落资产 ----------

#[test]
fn junk_jpg_fails_import_and_leaves_no_asset_row() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("photos");
    std::fs::create_dir_all(&src).unwrap();
    make_png(&src.join("a.png"), 40.0);
    make_png(&src.join("b.png"), 90.0);
    std::fs::write(src.join("c-broken.jpg"), JUNK).unwrap();
    // 非图片扩展名：扫描期就该跳过（与「扩展名对内容坏」是两种语义）
    std::fs::write(src.join("note.txt"), JUNK).unwrap();

    let (conn, thumbs) = setup(&tmp.path().join("app"));
    let files = scan_dir(&src).unwrap();
    assert_eq!(files.len(), 3, "扫描只收图片扩展名（note.txt 不进）");
    let bid = create_batch(&conn, &src, "reference", files.len() as i64).unwrap();
    let out = run_batch(&conn, bid, None, &thumbs, |_| Ok(())).unwrap();

    assert_eq!(out.imported, 2, "只有两张真图入库");
    assert_eq!(out.failed, 1, "坏图必须计 failed（事故里是 failed:0）");
    assert_eq!(out.failedItems.len(), 1);
    assert_eq!(out.failedItems[0].fileName, "c-broken.jpg");
    assert!(
        out.failedItems[0].error.contains("无法解码"),
        "失败原因要能直读：{}",
        out.failedItems[0].error
    );

    // 关键：坏图**没有** assets 行，也没有 resource 行（不是「入库后标记坏」）
    let assets: i64 = conn
        .query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(assets, 2, "坏图不得产生资产：{assets}");
    let leaked: i64 = conn
        .query_row("SELECT COUNT(*) FROM resource WHERE width <= 0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(leaked, 0, "不得留下 width=0 的假资源");
}

// ---------- 3) 核查的解码成本策略（纯逻辑） ----------

#[test]
fn needs_decode_only_for_suspect_assets() {
    let base = MissingTarget {
        assetId: 1,
        absPath: "/x/a.jpg".into(),
        wasMissing: 0,
        width: Some(640),
        height: Some(480),
        mtime: 100,
        fileSz: 1000,
    };
    // 有正尺寸 + 文件没动 → 不解码（1 万张全解码不可接受）
    assert!(!base.needs_decode(1000, 100));
    // 尺寸是 0（历史坏资产）→ 必须复核
    assert!(MissingTarget {
        width: Some(0),
        ..base.clone()
    }
    .needs_decode(1000, 100));
    // 没有 resource 行 → 必须复核
    assert!(MissingTarget {
        width: None,
        height: None,
        ..base.clone()
    }
    .needs_decode(1000, 100));
    // 文件被换过（size/mtime 变了）→ 必须复核（可能是修好了，也可能是被写坏）
    assert!(base.needs_decode(2000, 100));
    assert!(base.needs_decode(1000, 101));
}

// ---------- 4) 核查：识别坏资产 + 修复可解码资产 ----------

#[test]
fn verify_flags_unparseable_and_repairs_fixed_files() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let (conn, thumbs) = setup(&tmp.path().join("app"));

    // ① 历史坏资产（旧版本静默入库的形态）：文件在、resource width=0
    let junk = lib.join("c-broken.jpg");
    std::fs::write(&junk, JUNK).unwrap();
    // ② 记录坏但文件其实是好图（修复前的半截状态）：核查应自动修复
    let repaired = lib.join("b-good.png");
    make_png(&repaired, 50.0);
    // ③ 正常资产：有正尺寸 + 文件未动 → 本次不解码（pHash 保持原值证明没动过）
    let trusted = lib.join("a.png");
    make_png(&trusted, 10.0);

    let ins = |path: &Path, name: &str, sz: i64, mt: i64| -> i64 {
        conn.execute(
            "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt, missing)
             VALUES ('image', ?1, ?2, 'jpg', ?3, ?4, unixepoch(), 0)",
            rusqlite::params![path.to_string_lossy(), name, sz, mt],
        )
        .unwrap();
        conn.last_insert_rowid()
    };
    let a_broken = ins(&junk, "c-broken.jpg", JUNK.len() as i64, mtime_secs(&junk));
    let a_repaired = ins(&repaired, "b-good.png", 1, 1);
    // 可信资产：size/mtime 与盘上一致 → 本次核查不解码
    let a_trusted = ins(
        &trusted,
        "a.png",
        std::fs::metadata(&trusted).unwrap().len() as i64,
        mtime_secs(&trusted),
    );
    let res = |aid: i64, w: i64, h: i64, ph: Option<&str>| {
        conn.execute(
            "INSERT INTO resource (assetId, role, ordinal, thumbPath, width, height, pHash)
             VALUES (?1, 'primary', 0, '', ?2, ?3, ?4)",
            rusqlite::params![aid, w, h, ph],
        )
        .unwrap();
    };
    res(a_broken, 0, 0, None);
    res(a_repaired, 0, 0, None);
    res(a_trusted, 64, 64, Some("deadbeefdeadbeef"));

    let r = verify_missing_with_thumbs(&conn, &thumbs).unwrap();
    assert_eq!(r.checked, 3);
    assert_eq!(r.broken, 1, "只有 c-broken.jpg 是「存在但解不开」：{r:?}");
    assert_eq!(r.repaired, 1, "b-good.png 记录坏但能解 → 修复一张：{r:?}");
    assert_eq!(r.notFound, 0);
    assert_eq!(r.unreadable, 0);

    // 坏资产：尺寸写回 0（= /api/library/assets?broken=1 的口径）；真资产：尺寸修回
    let dims = |aid: i64| -> (i64, i64) {
        conn.query_row(
            "SELECT width, height FROM resource WHERE assetId=?1 AND role='primary'",
            rusqlite::params![aid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(dims(a_broken), (0, 0));
    assert_eq!(dims(a_repaired), (64, 64), "修好的资产要落上新尺寸");
    // 可信资产完全没动（pHash 原值 → 证明本次没解码/没重写）
    let ph: Option<String> = conn
        .query_row(
            "SELECT pHash FROM resource WHERE assetId=?1 AND role='primary'",
            rusqlite::params![a_trusted],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ph.as_deref(), Some("deadbeefdeadbeef"));

    // 缩略图：修复的资产两档都生成了；坏资产没有缩略图（接口层发占位图）
    for (aid, should_exist) in [(a_repaired, true), (a_broken, false)] {
        for size in core_media::THUMB_SIZES {
            let tp = thumb_path(&thumbs, aid, *size);
            assert_eq!(
                tp.is_file(),
                should_exist,
                "asset {aid} size {size}: {tp:?}"
            );
        }
    }

    // ---------- 文件被换成真图 → 再核查：broken 归零 ----------
    make_png(&junk, 70.0);
    let r2 = verify_missing_with_thumbs(&conn, &thumbs).unwrap();
    assert_eq!(r2.broken, 0, "换回真图后不该再有坏资产：{r2:?}");
    assert_eq!(r2.repaired, 1, "坏→好 也是修复：{r2:?}");
    assert_eq!(dims(a_broken), (64, 64));
    assert!(
        thumb_path(&thumbs, a_broken, 256).is_file(),
        "修复后缩略图要落盘"
    );

    // 记录落 settings（UI 新鲜度/计数依赖它）
    let rec = last_missing_check(&conn).unwrap();
    assert_eq!(rec.broken, 0);
    assert_eq!(rec.repaired, 1);
}

/// 兼容入口（启动期 mark_missing）不做解码：坏资产不会被它写成 width=0，
/// 但也不会把好的判坏 —— 「无法预览」的现场判定只发生在用户显式「重新核查」。
#[test]
fn stat_only_entry_does_not_decode() {
    let _g = vips_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (conn, _thumbs) = setup(&tmp.path().join("app"));
    let junk = tmp.path().join("lib/c-broken.jpg");
    std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
    std::fs::write(&junk, JUNK).unwrap();
    conn.execute(
        "INSERT INTO assets (mediaType, absPath, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES ('image', ?1, 'c-broken.jpg', 'jpg', 21, 1, 1)",
        rusqlite::params![junk.to_string_lossy()],
    )
    .unwrap();
    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.checked, 1);
    assert_eq!(r.broken, 0, "只 stat 的入口不解码：{r:?}");
    assert_eq!(r.missing, 0, "文件在盘上就不是失联");
}
