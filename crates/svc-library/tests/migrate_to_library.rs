//! DT6（卡 t_fcb18ceb）「一键迁入库」核心行为钉：目标路径纯函数 / skip 分类 / 续跑判定 /
//! 真复制（源默认保留）/ deleteSource / 失败不留半成品 / 审计账本。
//!
//! 全走真 SQLite（临时目录 dam.db）+ 真文件：迁移的正确性只体现在「盘上到底有几份文件、
//! absPath 指向哪、账本记了什么」。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 测试用内容指纹：真 SHA-256（本 crate 已依赖 core-hash），保证「迁移前比对磁盘」这条
/// 逻辑走的是生产同一条判定。
fn sha(bytes: &[u8]) -> String {
    core_hash::sha256_hex(bytes)
}

struct Fx {
    dir: PathBuf,
    conn: rusqlite::Connection,
    lib_root: PathBuf,
}

impl Fx {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dam-migrate-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = core_storage::open(&dir.join("dam.db")).unwrap();
        let lib_root = dir.join("library");
        Fx { dir, conn, lib_root }
    }

    /// 「参考目录」里造文件 + assets 行，返回 (assetId, 原路径, hash)。
    fn add_reference(&self, name: &str, bytes: &[u8]) -> (i64, PathBuf, String) {
        let d = self.dir.join("external");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, bytes).unwrap();
        let h = sha(bytes);
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.insert_row(&p, name, &ext, bytes.len() as i64, Some(&h), "reference");
        (self.conn.last_insert_rowid(), p, h)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row(
        &self,
        p: &Path,
        name: &str,
        ext: &str,
        sz: i64,
        hash: Option<&str>,
        store_mode: &str,
    ) {
        self.conn
            .execute(
                "INSERT INTO assets (mediaType, absPath, storeMode, fileName, fileExt, fileSz,
                                     mtime, fileHash, importedAt)
                 VALUES ('image', ?1, ?2, ?3, ?4, ?5, 0, ?6, unixepoch())",
                rusqlite::params![p.to_string_lossy(), store_mode, name, ext, sz, hash],
            )
            .unwrap();
    }

    fn abs_path(&self, id: i64) -> String {
        self.conn
            .query_row("SELECT absPath FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn store_mode(&self, id: i64) -> String {
        self.conn
            .query_row("SELECT storeMode FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn audit_count(&self, batch: &str, status: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM migrate_audit WHERE batchId=?1 AND status=?2",
                rusqlite::params![batch, status],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn files_in(&self, dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// 跑一批（默认选项）。
    fn run(&mut self, ids: &[i64], batch: &str) -> svc_library::MigrateReport {
        self.run_opts(ids, batch, svc_library::MigrateOptions::default(), None)
    }

    fn run_opts(
        &mut self,
        ids: &[i64],
        batch: &str,
        mut opts: svc_library::MigrateOptions,
        progress: Option<Arc<std::sync::Mutex<Vec<svc_library::MigrateProgress>>>>,
    ) -> svc_library::MigrateReport {
        opts.batch_id = Some(batch.to_string());
        if let Some(log) = progress {
            opts.progress = Some(Arc::new(move |p: &svc_library::MigrateProgress| {
                log.lock().unwrap().push(p.clone());
            }));
        }
        svc_library::migrate_to_library(&mut self.conn, ids, &self.lib_root, &opts).unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------- 纯函数 ----------------------------

#[test]
fn migration_target_matches_copyin_import_layout() {
    let root = Path::new("/lib");
    let hash = "0123456789abcdef0123456789abcdef";
    // 与 svc_import::process_file 的 copyin 落盘一致：<root>/<hash 前 16 位>/<原文件名>
    assert_eq!(
        svc_library::migration_target(root, hash, "photo.jpg", "jpg").unwrap(),
        PathBuf::from("/lib/0123456789abcdef/photo.jpg")
    );
    // 大写 hash 归一为小写（与导入侧一致）
    assert_eq!(
        svc_library::migration_target(root, &hash.to_uppercase(), "p.jpg", "jpg").unwrap(),
        PathBuf::from("/lib/0123456789abcdef/p.jpg")
    );
    // 缺文件名 → 退化 <hash>.<ext>；扩展名带点也容忍
    assert_eq!(
        svc_library::migration_target(root, hash, "  ", ".png").unwrap(),
        PathBuf::from(format!("/lib/0123456789abcdef/{hash}.png"))
    );
    // 连扩展名都没有 → 纯 hash 作文件名
    assert_eq!(
        svc_library::migration_target(root, hash, "", "").unwrap(),
        PathBuf::from(format!("/lib/0123456789abcdef/{hash}"))
    );
}

#[test]
fn migration_target_rejects_bad_hash() {
    let root = Path::new("/lib");
    for bad in ["", "   ", "abc", "zzzzzzzzzzzzzzzzzz", "0123456789abcde"] {
        let e = svc_library::migration_target(root, bad, "a.jpg", "jpg")
            .expect_err("非法 hash 必须报错");
        assert!(e.to_string().contains("fileHash"), "{bad} → {e}");
    }
}

#[test]
fn skip_reason_priority_and_all_branches() {
    use svc_library::skip_reason as s;
    // 资产不存在优先于一切
    assert_eq!(s(false, false, "", false, false), Some("not_found"));
    // 无指纹
    assert_eq!(s(true, false, "reference", true, true), Some("no_hash"));
    // 已在库内（copyin / 其它非 reference 形态）
    assert_eq!(s(true, true, "copyin", true, true), Some("already_copyin"));
    // 盘上读不到
    assert_eq!(s(true, true, "reference", false, true), Some("missing_file"));
    // 内容与指纹不符
    assert_eq!(
        s(true, true, "reference", true, false),
        Some("content_changed")
    );
    // 全绿 → 可迁移
    assert_eq!(s(true, true, "reference", true, true), None);
}

#[test]
fn skip_reason_text_covers_every_code() {
    for code in [
        "not_found",
        "no_hash",
        "already_copyin",
        "missing_file",
        "content_changed",
        "unknown_code",
    ] {
        let t = svc_library::skip_reason_text(code);
        assert!(!t.is_empty(), "{code} 必须有人话");
    }
    assert!(svc_library::skip_reason_text("content_changed").contains("重新导入"));
    assert_eq!(svc_library::skip_reason_text("unknown_code"), "跳过");
}

#[test]
fn resume_hit_is_size_based_and_ignores_dirs() {
    let fx = Fx::new("resume-pure");
    let p = fx.dir.join("file.bin");
    std::fs::write(&p, b"12345").unwrap();
    assert!(svc_library::is_resume_hit(&p, 5));
    assert!(!svc_library::is_resume_hit(&p, 4));
    assert!(!svc_library::is_resume_hit(&fx.dir, 0), "目录不算命中");
    assert!(!svc_library::is_resume_hit(&fx.dir.join("nope"), 0));
}

#[test]
fn batch_id_is_unique_and_prefixed() {
    let a = svc_library::new_migrate_batch_id();
    let b = svc_library::new_migrate_batch_id();
    assert!(a.starts_with("mig-"));
    assert_ne!(a, b);
}

// ---------------------------- 迁移主流程 ----------------------------

#[test]
fn copies_into_library_and_keeps_source_by_default() {
    let mut fx = Fx::new("copy");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"alpha-payload");
    let (b, pb, hb) = fx.add_reference("b.jpg", b"beta-payload-longer");

    let rep = fx.run(&[a, b], "b-copy");
    assert_eq!(rep.total, 2);
    assert_eq!(rep.migrated, 2, "{rep:?}");
    assert!(rep.skipped.is_empty() && rep.failed.is_empty() && rep.warnings.is_empty());
    assert_eq!(
        rep.bytes,
        (b"alpha-payload".len() + b"beta-payload-longer".len()) as i64
    );

    for (id, old, hash, name, content) in [
        (a, &pa, &ha, "a.jpg", &b"alpha-payload"[..]),
        (b, &pb, &hb, "b.jpg", &b"beta-payload-longer"[..]),
    ] {
        let expect = fx.lib_root.join(&hash[..16]).join(name);
        assert_eq!(PathBuf::from(fx.abs_path(id)), expect);
        assert_eq!(std::fs::read(&expect).unwrap(), content, "库内内容必须一致");
        assert!(old.is_file(), "源文件默认保留：{}", old.display());
        assert_eq!(fx.store_mode(id), "copyin");
        // 不留 .part 半成品
        let leftovers: Vec<String> = fx
            .files_in(expect.parent().unwrap())
            .into_iter()
            .filter(|n| n.contains(".part-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    let rows = svc_library::migrate_audit_rows(&fx.conn, Some("b-copy"), 100).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.status == "done" && r.kind == "copyin"));
    assert!(rows.iter().all(|r| r.operator == "local"), "默认 operator");
    assert!(rows.iter().all(|r| r.at > 0 && r.bytes > 0));
    assert_eq!(fx.audit_count("b-copy", "failed"), 0);
}

#[test]
fn delete_source_true_removes_origin_after_copy() {
    let mut fx = Fx::new("delsrc");
    let (a, pa, _ha) = fx.add_reference("a.jpg", b"payload");
    let rep = fx.run_opts(
        &[a],
        "b-del",
        {
            let mut o = svc_library::MigrateOptions::default();
            o.delete_source = true;
            o.operator = "alice".into();
            o
        },
        None,
    );
    assert_eq!(rep.migrated, 1, "{rep:?}");
    assert!(!pa.exists(), "deleteSource:true 时源应被删");
    assert!(PathBuf::from(fx.abs_path(a)).is_file());
    let rows = svc_library::migrate_audit_rows(&fx.conn, Some("b-del"), 10).unwrap();
    assert_eq!(rows[0].operator, "alice", "operator 应透传");
}

#[test]
fn operator_falls_back_to_local_when_blank() {
    let mut fx = Fx::new("opblank");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"p");
    let rep = fx.run_opts(
        &[a],
        "b-op",
        {
            let mut o = svc_library::MigrateOptions::default();
            o.operator = "   ".into();
            o
        },
        None,
    );
    assert_eq!(rep.migrated, 1);
    let rows = svc_library::migrate_audit_rows(&fx.conn, Some("b-op"), 10).unwrap();
    assert_eq!(rows[0].operator, "local");
}

#[test]
fn resume_hit_copies_zero_bytes_and_reuses_existing_file() {
    let mut fx = Fx::new("resume");
    let (a, _pa, ha) = fx.add_reference("a.jpg", b"same-bytes");
    // 上一次迁移已经把目标放好了（中断后重跑 / 导入已落过同一份）
    let target_dir = fx.lib_root.join(&ha[..16]);
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("a.jpg"), b"same-bytes").unwrap();

    let rep = fx.run(&[a], "b-resume");
    assert_eq!(rep.migrated, 1, "{rep:?}");
    assert_eq!(rep.bytes, 0, "续跑命中不该再写盘");
    assert_eq!(fx.files_in(&target_dir), vec!["a.jpg".to_string()]);
    assert_eq!(fx.store_mode(a), "copyin");
    let rows = svc_library::migrate_audit_rows(&fx.conn, Some("b-resume"), 10).unwrap();
    assert_eq!(rows[0].bytes, 0);
    assert_eq!(rows[0].status, "done");
}

#[test]
fn idempotent_rerun_on_copyin_asset_is_skipped() {
    let mut fx = Fx::new("idem");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"p");
    fx.run(&[a], "b-1");
    let rep = fx.run(&[a], "b-2");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.skipped.len(), 1);
    assert_eq!(rep.skipped[0].reason, "already_copyin");
    assert_eq!(rep.bytes, 0);
    assert_eq!(fx.audit_count("b-2", "done"), 0, "跳过不记账");
}

#[test]
fn skips_missing_content_changed_and_hashless_assets() {
    let mut fx = Fx::new("skips");
    // 1) 文件不在盘上
    let (missing, p_missing, _h1) = fx.add_reference("m.jpg", b"gone");
    std::fs::remove_file(&p_missing).unwrap();
    // 2) 内容被替换（hash 对不上）
    let (changed, p_changed, _h2) = fx.add_reference("c.jpg", b"original");
    std::fs::write(&p_changed, b"tampered-content").unwrap();
    // 3) 无 fileHash
    let p_nohash = fx.dir.join("external/n.jpg");
    std::fs::write(&p_nohash, b"no hash").unwrap();
    fx.insert_row(&p_nohash, "n.jpg", "jpg", 7, None, "reference");
    let nohash = fx.conn.last_insert_rowid();

    let rep = fx.run(&[missing, changed, nohash, 999_999], "b-skips");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.skipped.len(), 4);
    let reasons: Vec<&str> = rep.skipped.iter().map(|s| s.reason.as_str()).collect();
    assert_eq!(
        reasons,
        vec!["missing_file", "content_changed", "no_hash", "not_found"]
    );
    assert!(rep.skipped.iter().all(|s| !s.detail.is_empty()));
    // 失联资产被标 missing=1（与 move_assets 同口径），内容变了的资产不动
    let m: i64 = fx
        .conn
        .query_row("SELECT missing FROM assets WHERE assetId=?1", [missing], |r| r.get(0))
        .unwrap();
    assert_eq!(m, 1);
    assert_eq!(fx.store_mode(changed), "reference");
    // 跳过一律无账本行
    let total: i64 = fx
        .conn
        .query_row("SELECT COUNT(*) FROM migrate_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 0);
}

#[test]
fn duplicate_ids_and_progress_callback() {
    let mut fx = Fx::new("dups");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"p1");
    let (b, _pb, _hb) = fx.add_reference("b.jpg", b"p2");
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rep = fx.run_opts(&[a, b, a], "b-dups", Default::default(), Some(log.clone()));
    assert_eq!(rep.total, 3, "total 按请求计");
    assert_eq!(rep.migrated, 2, "重复 id 只处理一次：{rep:?}");
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2, "每个真正处理的资产回调一次");
    assert_eq!(log.last().unwrap().processed, 2);
    assert_eq!(log.last().unwrap().migrated, 2);
}

#[test]
fn batch_level_argument_validation() {
    let mut fx = Fx::new("validate");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"p");
    let opts = svc_library::MigrateOptions::default();
    // 空 ids
    let e = svc_library::migrate_to_library(&mut fx.conn, &[], &fx.lib_root, &opts).unwrap_err();
    assert!(e.to_string().contains("ids"));
    // 超过上限
    let many = vec![a; svc_library::MIGRATE_MAX_IDS + 1];
    let e = svc_library::migrate_to_library(&mut fx.conn, &many, &fx.lib_root, &opts).unwrap_err();
    assert!(e.to_string().contains("900"), "{e}");
    // 相对库根
    let e = svc_library::migrate_to_library(&mut fx.conn, &[a], Path::new("rel/lib"), &opts)
        .unwrap_err();
    assert!(e.to_string().contains("绝对路径"), "{e}");
    // 库根不可创建（父路径是个文件）→ 明确报错而不是静默
    let file_as_parent = fx.dir.join("blocked");
    std::fs::write(&file_as_parent, b"x").unwrap();
    let e = svc_library::migrate_to_library(
        &mut fx.conn,
        &[a],
        &file_as_parent.join("library"),
        &opts,
    )
    .unwrap_err();
    assert!(!e.to_string().is_empty());
}

#[test]
fn copy_failure_leaves_asset_untouched_and_audit_failed() {
    let mut fx = Fx::new("failcopy");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    // 让「库内分目录」位置被一个普通文件占住 → create_dir_all 必失败
    let blocker = fx.lib_root.join(&ha[..16]);
    std::fs::create_dir_all(&fx.lib_root).unwrap();
    std::fs::write(&blocker, b"not a dir").unwrap();

    let rep = fx.run(&[a], "b-fail");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert!(!rep.failed[0].code.is_empty());
    // 资产仍指向原路径、文件还在、形态没变；失败也记账（可定位重试）
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    assert!(pa.is_file());
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(fx.audit_count("b-fail", "failed"), 1);
    assert_eq!(fx.audit_count("b-fail", "done"), 0);
    // 失败不影响同批其它资产（不中止整批）
    let (b, _pb, _hb) = fx.add_reference("b.jpg", b"payload-b");
    let rep2 = fx.run(&[b], "b-fail2");
    assert_eq!(rep2.migrated, 1);
}

#[test]
fn audit_rows_filter_and_limit() {
    let mut fx = Fx::new("audit");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"p1");
    let (b, _pb, _hb) = fx.add_reference("b.jpg", b"p2");
    fx.run(&[a], "batch-x");
    fx.run(&[b], "batch-y");
    assert_eq!(svc_library::migrate_audit_rows(&fx.conn, Some("batch-x"), 10).unwrap().len(), 1);
    assert_eq!(svc_library::migrate_audit_rows(&fx.conn, None, 10).unwrap().len(), 2);
    // limit 下限被夹到 1，上限被夹到 1000（不 panic、不返回超量）
    assert_eq!(svc_library::migrate_audit_rows(&fx.conn, None, 0).unwrap().len(), 1);
    assert!(svc_library::migrate_audit_rows(&fx.conn, Some("nope"), 10)
        .unwrap()
        .is_empty());
}

// ---------------------------- 边角：告警 / 读失败 / 进度回调 ----------------------------

/// 源的 stat 字节数与实际可读字节数不一致（/proc 伪文件：st_size=0，read 却有内容）
/// → 复制后的字节数校验必须拦住（否则会以 0 字节记账），并且不留任何半成品。
/// 注：这条同时覆盖「复制字节数不符」防线；「删不掉源」的告警路径见同 crate 的
/// `migrate_fault_tests`（那里用注入的删源动作造失败）。
#[test]
fn size_mismatch_after_copy_is_rejected() {
    let mut fx = Fx::new("sizemismatch");
    let src = Path::new("/proc/self/cmdline");
    let bytes = std::fs::read(src).unwrap();
    let hash = sha(&bytes);
    fx.insert_row(src, "cmdline", "", bytes.len() as i64, Some(&hash), "reference");
    let id = fx.conn.last_insert_rowid();

    let rep = fx.run_opts(
        &[id],
        "b-mismatch",
        {
            let mut o = svc_library::MigrateOptions::default();
            o.delete_source = true;
            o
        },
        None,
    );
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].code, "copy_failed");
    assert!(rep.warnings.is_empty());
    assert_eq!(fx.store_mode(id), "reference");
    // 源没被动（复制没通过校验，绝不删源）
    assert!(src.exists());
}

/// 打开得开、读不出来（/proc/self/mem）→ 指纹核对拿不到实况，按「内容不可信」跳过，
/// 而不是硬搬一份来路不明的字节进库。
#[test]
fn unreadable_source_is_skipped_as_content_changed() {
    let mut fx = Fx::new("unreadable");
    let src = Path::new("/proc/self/mem");
    if !src.is_file() || std::fs::read(src).is_ok() {
        eprintln!("跳过：本机 /proc/self/mem 行为不同（不是可打开不可读）");
        return;
    }
    fx.insert_row(src, "mem", "", 0, Some(&"a".repeat(64)), "reference");
    let id = fx.conn.last_insert_rowid();
    let rep = fx.run(&[id], "b-unread");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.skipped.len(), 1);
    assert_eq!(rep.skipped[0].reason, "content_changed");
}

/// 进度回调在「跳过 / 失败 / 成功」三条路上都要被调用（前端进度条靠它，不许漏计）。
#[test]
fn progress_callback_fires_on_skip_failure_and_success() {
    let mut fx = Fx::new("progress");
    let (ok, _p1, _h1) = fx.add_reference("ok.jpg", b"ok-payload");
    let (gone, p_gone, _h2) = fx.add_reference("gone.jpg", b"gone-payload");
    std::fs::remove_file(&p_gone).unwrap();
    // 让某个资产的目标分目录被文件占住 → 该条失败
    let (bad, _p3, h3) = fx.add_reference("bad.jpg", b"bad-payload");
    std::fs::create_dir_all(&fx.lib_root).unwrap();
    std::fs::write(fx.lib_root.join(&h3[..16]), b"not a dir").unwrap();

    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rep = fx.run_opts(
        &[ok, gone, bad, 999_999],
        "b-progress",
        Default::default(),
        Some(log.clone()),
    );
    assert_eq!(rep.migrated, 1, "{rep:?}");
    assert_eq!(rep.skipped.len(), 2);
    assert_eq!(rep.failed.len(), 1);
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 4, "每个入参资产都应回调一次：{log:?}");
    let last = log.last().unwrap();
    assert_eq!(
        (last.processed, last.migrated, last.skipped, last.failed),
        (4, 1, 2, 1)
    );
    assert_eq!(last.total, 4);
    assert!(last.bytes > 0);
}

// ---------------------------- 防线：DB / 路径异常时不留半成品 ----------------------------
// 这些用例刻意把「库写不了 / 账本写不了 / 事务开不了 / 更新命中 0 行 / 文件名超长 /
// 目标被目录占住」造出来：迁移的承诺是「失败时资产仍指向原路径、库内不留半成品」，
// 只有真把失败造出来才算验证。

/// DB 变成只读 → UPDATE 失败 → 本次复制的那份必须被清掉，资产保持 reference。
#[test]
fn db_update_failure_cleans_copied_file() {
    let mut fx = Fx::new("ro");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    fx.conn.execute_batch("PRAGMA query_only=1").unwrap();
    let rep = fx.run(&[a], "b-ro");
    fx.conn.execute_batch("PRAGMA query_only=0").unwrap();

    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].code, "sqlite");
    // 资产完全没变
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    // 库内不留半成品（连分目录里也没有文件）
    let dir = fx.lib_root.join(&ha[..16]);
    assert!(fx.files_in(&dir).is_empty(), "{:?}", fx.files_in(&dir));
}

/// 账本写不进去（表被改名）→ 事务回滚 + 复制出来的文件被清掉。
#[test]
fn db_audit_insert_failure_rolls_everything_back() {
    let mut fx = Fx::new("noaudit");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    fx.conn
        .execute_batch("ALTER TABLE migrate_audit RENAME TO migrate_audit_gone")
        .unwrap();
    let rep = fx.run(&[a], "b-noaudit");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].code, "sqlite");
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    let dir = fx.lib_root.join(&ha[..16]);
    assert!(fx.files_in(&dir).is_empty(), "{:?}", fx.files_in(&dir));
}

/// 连接上已有外层事务 → 开不了新事务 → 明确失败，且复制出来的文件被清掉。
#[test]
fn transaction_creation_failure_cleans_copied_file() {
    let mut fx = Fx::new("intx");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    fx.conn.execute_batch("BEGIN").unwrap();
    let rep = svc_library::migrate_to_library(
        &mut fx.conn,
        &[a],
        &fx.lib_root,
        &{
            let mut o = svc_library::MigrateOptions::default();
            o.batch_id = Some("b-intx".into());
            o
        },
    )
    .unwrap();
    fx.conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed[0].code, "sqlite");
    assert!(rep.failed[0].detail.contains("事务"), "{:?}", rep.failed);
    assert_eq!(fx.store_mode(a), "reference");
    let dir = fx.lib_root.join(&ha[..16]);
    assert!(fx.files_in(&dir).is_empty(), "{:?}", fx.files_in(&dir));
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
}

/// UPDATE 命中 0 行（BEFORE UPDATE 触发器 RAISE(IGNORE) 模拟「资产刚被删」）
/// → 报 not_found，且不写账本、不留文件。
#[test]
fn zero_row_update_reports_not_found() {
    let mut fx = Fx::new("norow");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    fx.conn
        .execute_batch(
            "CREATE TRIGGER dam_skip_update BEFORE UPDATE ON assets
             FOR EACH ROW BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    let rep = fx.run(&[a], "b-norow");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed[0].code, "not_found");
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    let dir = fx.lib_root.join(&ha[..16]);
    assert!(fx.files_in(&dir).is_empty(), "{:?}", fx.files_in(&dir));
}

/// 目标文件名超长（> 单段上限）→ 复制失败要**明确报错**并清掉临时文件，不静默丢资产。
#[test]
fn over_long_target_name_fails_cleanly() {
    let mut fx = Fx::new("longname");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"payload");
    let long = format!("{}.jpg", "x".repeat(300));
    fx.conn
        .execute("UPDATE assets SET fileName=?1 WHERE assetId=?2", rusqlite::params![long, a])
        .unwrap();
    let rep = fx.run(&[a], "b-long");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert!(!rep.failed[0].detail.is_empty());
    assert_eq!(fx.store_mode(a), "reference");
}

/// 目标路径被一个**目录**占住 → rename 失败，临时文件被清掉，资产不动。
#[test]
fn target_occupied_by_directory_fails_cleanly() {
    let mut fx = Fx::new("targetdir");
    let (a, pa, ha) = fx.add_reference("a.jpg", b"payload");
    let target_dir = fx.lib_root.join(&ha[..16]);
    std::fs::create_dir_all(target_dir.join("a.jpg")).unwrap();
    let rep = fx.run(&[a], "b-dirtarget");
    assert_eq!(rep.migrated, 0, "{rep:?}");
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(fx.store_mode(a), "reference");
    assert_eq!(PathBuf::from(fx.abs_path(a)), pa);
    // 目录还在（我们没敢乱删），也没有 .part 残留
    assert!(target_dir.join("a.jpg").is_dir());
    let leftovers: Vec<String> = fx
        .files_in(&target_dir)
        .into_iter()
        .filter(|n| n.contains(".part-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// 账本表被破坏时，查询必须**明确报错**（前端「迁移清单」不能静默显示空列表）。
#[test]
fn audit_query_fails_loudly_when_ledger_is_broken() {
    let mut fx = Fx::new("brokeraudit");
    let (a, _pa, _ha) = fx.add_reference("a.jpg", b"payload");
    fx.run(&[a], "b-brk");
    fx.conn
        .execute_batch("ALTER TABLE migrate_audit RENAME TO migrate_audit_gone")
        .unwrap();
    assert!(svc_library::migrate_audit_rows(&fx.conn, Some("b-brk"), 10).is_err());
    assert!(svc_library::migrate_audit_rows(&fx.conn, None, 10).is_err());
}
