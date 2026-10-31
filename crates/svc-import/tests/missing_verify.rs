//! DESKTOP-MISSING-STAT-1 集成测试：失联现场核查（verify_missing / 分类 / settings 记录）。
//!
//! 覆盖卡面要求的三类失败：
//!   - 文件不存在（单文件被删、目录整体消失、路径中间段被换成普通文件 ENOTDIR）；
//!   - 权限拒绝（PermissionDenied，注入式 —— 容器内以 root 跑单测时 DAC 无法用 chmod 构造）；
//!   - 其他 IO 错误（真实符号链接环 ELOOP）。
//!
//! 断言口径：分类正确、**不崩不吞**（错误串进 samples）、missing 标记与恢复都落库、
//! 核查记录落 settings（stats 的新鲜度标记依赖它）。
//!
//! 本文件不解码图片（只 stat），故不需要 libvips / vips_lock。

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use svc_import::*;

fn setup_db(dir: &Path) -> Connection {
    core_storage::open(&dir.join("test.db")).unwrap()
}

/// 建一个真实文件（内容无关紧要：核查只看 metadata）。返回路径。
fn touch(p: &Path) -> PathBuf {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b"x").unwrap();
    p.to_path_buf()
}

/// 直接插 assets 行（核查只关心 absPath/assetId/missing）。
fn insert_asset(conn: &Connection, id: i64, abs: &str, missing: i64) {
    let name = Path::new(abs)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    conn.execute(
        "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
         VALUES (?1, ?2, ?3, ?4, 'png', 1, 1, 1)",
        rusqlite::params![id, abs, missing, name],
    )
    .unwrap();
}

fn missing_flags(conn: &Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT missing FROM assets ORDER BY assetId")
        .unwrap();
    let v = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<i64>>>()
        .unwrap();
    v
}

// ---------- 类 1：文件不存在 ----------

#[test]
fn deleted_file_is_marked_missing_then_recovered() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    let a = touch(&tmp.path().join("lib/a.png"));
    let b = touch(&tmp.path().join("lib/b.png"));
    let c = touch(&tmp.path().join("lib/c.png"));
    insert_asset(&conn, 1, &a.to_string_lossy(), 0);
    insert_asset(&conn, 2, &b.to_string_lossy(), 0);
    insert_asset(&conn, 3, &c.to_string_lossy(), 0);

    // 外部删除（不经 watcher：正是线上 /tmp 被清空的形态）
    std::fs::remove_file(&b).unwrap();
    std::fs::remove_file(&c).unwrap();

    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.checked, 3);
    assert_eq!(r.notFound, 2, "两文件不存在 → notFound=2");
    assert_eq!(r.newlyMissing, 2);
    assert_eq!(r.missing, 2, "核查后 missing 必须等于「删除数」");
    assert_eq!(
        r.missingBefore, 0,
        "核查前库里还是旧值 0（这就是原来的撒谎点）"
    );
    assert_eq!(r.recovered, 0);
    assert_eq!(r.unreadable, 0);
    assert_eq!(missing_flags(&conn), vec![0, 1, 1]);

    // 核查记录落 settings（stats 的新鲜度标记数据源）
    let rec = last_missing_check(&conn).expect("settings 里必须有核查记录");
    assert_eq!(rec.missing, 2);
    assert_eq!(rec.checked, 3);
    assert!(rec.at > 0, "记录必须带时间戳");
    assert_eq!(rec.durationMs, r.durationMs);

    // 文件回来 → 一次核查自动接回（不必等 watcher）
    touch(&c);
    let r2 = verify_missing(&conn).unwrap();
    assert_eq!(r2.notFound, 1);
    assert_eq!(r2.recovered, 1);
    assert_eq!(r2.missing, 1);
    assert_eq!(missing_flags(&conn), vec![0, 1, 0]);
}

#[test]
fn whole_directory_removed_marks_every_child() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    let d = tmp.path().join("photos");
    let p1 = touch(&d.join("1.png"));
    let p2 = touch(&d.join("2.png"));
    let p3 = touch(&d.join("sub/3.png"));
    insert_asset(&conn, 1, &p1.to_string_lossy(), 0);
    insert_asset(&conn, 2, &p2.to_string_lossy(), 0);
    insert_asset(&conn, 3, &p3.to_string_lossy(), 0);

    // 整个目录被外部删掉（源目录搬家/盘拔掉）
    std::fs::remove_dir_all(&d).unwrap();

    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.notFound, 3, "整目录消失 = 每个子文件都失联，一个不能漏");
    assert_eq!(r.missing, 3);
    assert_eq!(missing_flags(&conn), vec![1, 1, 1]);
    assert_eq!(r.samples.len(), 3, "样本不吞：3 条异常都要在报告里");
    assert!(r.samples.iter().all(|s| s.class == "notFound"));
    assert!(r.samples.iter().all(|s| s.error.is_some()), "错误串要留痕");
}

#[test]
fn path_component_replaced_by_file_counts_as_missing_enotdir() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    // 中间段是普通文件 → stat 返回 ENOTDIR（不是 NotFound），同样属于"读不到"
    let blocker = touch(&tmp.path().join("photos"));
    let phantom = blocker.join("child.png");
    insert_asset(&conn, 1, &phantom.to_string_lossy(), 0);

    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.notFound, 1);
    assert_eq!(r.missing, 1);
    assert_eq!(missing_flags(&conn), vec![1]);
}

// ---------- 类 2：权限拒绝（注入式，见文件头说明） ----------

#[test]
fn permission_denied_is_classified_and_never_marks_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    let ok = touch(&tmp.path().join("lib/ok.png"));
    let secret = tmp.path().join("lib/secret.png");
    insert_asset(&conn, 1, &ok.to_string_lossy(), 0);
    insert_asset(&conn, 2, &secret.to_string_lossy(), 0);

    let targets = collect_missing_targets(&conn).unwrap();
    let probes = probe_paths_with(&targets, |p| {
        if p.ends_with("secret.png") {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Permission denied (os error 13)",
            ))
        } else {
            Ok(())
        }
    });
    let pd = probes
        .iter()
        .find(|p| p.absPath.ends_with("secret.png"))
        .unwrap();
    assert_eq!(pd.class, ProbeClass::PermissionDenied);
    assert!(pd.error.as_deref().unwrap().contains("Permission denied"));

    let r = apply_missing_probes(&conn, &probes, std::time::Instant::now()).unwrap();
    assert_eq!(r.permissionDenied, 1);
    assert_eq!(
        r.unreadable, 1,
        "存在但读不了要单独计数，不能折进 missing 也不能丢"
    );
    assert_eq!(r.notFound, 0);
    assert_eq!(
        r.missing, 0,
        "文件在盘上就不是「失联」（不能用 exists() 判定）"
    );
    assert_eq!(missing_flags(&conn), vec![0, 0]);
    assert!(r.samples.iter().any(|s| s.class == "permissionDenied"));
    // 记录也带上 unreadable，UI/stats 才能如实提示
    let rec = last_missing_check(&conn).unwrap();
    assert_eq!(rec.unreadable, 1);
}

#[test]
fn other_io_error_is_reported_not_swallowed() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    // 真实符号链接环 → ELOOP（Linux 下 metadata 报错）
    let looped = tmp.path().join("lib/loop.png");
    std::fs::create_dir_all(looped.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&looped, &looped).unwrap();
    insert_asset(&conn, 1, &looped.to_string_lossy(), 0);

    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.other, 1, "符号链接环归 Other 类，如实上报");
    assert_eq!(r.notFound, 0);
    assert_eq!(r.unreadable, 1);
    assert_eq!(r.missing, 0);
    assert!(r
        .samples
        .iter()
        .any(|s| s.class == "other" && s.error.is_some()));
}

// ---------- 分类函数直测（无 FS 依赖） ----------

#[test]
fn classify_io_error_maps_kinds() {
    use std::io::{Error, ErrorKind};
    assert_eq!(
        classify_io_error(&Error::new(ErrorKind::NotFound, "nf")),
        ProbeClass::NotFound
    );
    assert_eq!(
        classify_io_error(&Error::new(ErrorKind::PermissionDenied, "pd")),
        ProbeClass::PermissionDenied
    );
    assert_eq!(
        classify_io_error(&Error::new(ErrorKind::TimedOut, "to")),
        ProbeClass::Other
    );
    // ENOTDIR（20）等价失联
    assert_eq!(
        classify_io_error(&Error::from_raw_os_error(20)),
        ProbeClass::NotFound
    );
    assert_eq!(
        classify_io_error(&Error::from_raw_os_error(5)),
        ProbeClass::Other
    );
}

/// 核查期间路径被 watcher/relink 改掉的行，不用过期路径的结果覆盖（条件 UPDATE）。
#[test]
fn stale_probe_does_not_clobber_relinked_row() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    let old = tmp.path().join("lib/old.png");
    insert_asset(&conn, 1, &old.to_string_lossy(), 0);
    let targets = collect_missing_targets(&conn).unwrap();

    // 核查读目标之后、写回之前：路径被改到新位置（文件在那儿）
    let new = touch(&tmp.path().join("lib/new.png"));
    conn.execute(
        "UPDATE assets SET absPath=?1 WHERE assetId=1",
        rusqlite::params![new.to_string_lossy()],
    )
    .unwrap();

    // old 路径的探测结果（NotFound）不允许把已接回的行标记成失联
    let probes = probe_paths(&targets);
    let r = apply_missing_probes(&conn, &probes, std::time::Instant::now()).unwrap();
    assert_eq!(r.missing, 0);
    assert_eq!(missing_flags(&conn), vec![0]);
}

/// 空库核查不炸（首启 / 新库）。
#[test]
fn empty_library_verifies_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let conn = setup_db(tmp.path());
    let r = verify_missing(&conn).unwrap();
    assert_eq!(r.checked, 0);
    assert_eq!(r.missing, 0);
    assert!(last_missing_check(&conn).is_some());
    assert_eq!(mark_missing(&conn).unwrap(), 0, "兼容入口同口径");
}
