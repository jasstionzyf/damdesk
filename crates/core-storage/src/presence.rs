//! 失联「失败即标」——`assets.missing` 的唯一写入口（G5-A 裁决 2026-10-19，卡 t_84604c05）。
//!
//! 为什么不做全库 stat 巡航：浏览/筛选/打标/合规全走 DB + 缓存缩略图，目录被改名/移动、
//! 外接盘拔掉、网盘掉线时**浏览阶段没有任何动作会读磁盘**。所以不设「核查」功能，而是让
//! 真正碰磁盘的几处（导出原图、读原图=分享链接、相似图/去重重算）在失败时顺手记账。
//!
//! 判据只有一种：`NotFound` / `ENOENT` / `ENOTDIR`（路径中间段被删或被替换成普通文件）→
//! `assets.missing=1`。`PermissionDenied` 与其他 IO 错误**不标**——那是 unreadable 语义，
//! 与「文件不在了」是两回事，混一起会让「失联」数字骗人。
//!
//! 反向也顺手：读成功且此前被标失联（文件回来了）→ 清回 `missing=0`，代价为零。

use std::io;
use std::path::Path;

use rusqlite::Connection;

/// 单条资产的现场探测结论（调用方据此决定「跳过 / 报错 / 正常返回」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// 文件在（可 stat）。若此前被标失联，本次已顺手清标。
    Present,
    /// 不在了（含路径中间段被删/被替换）。已写 `missing=1`。
    Gone,
    /// 在但读不了 / 其它 IO 错误。**不**碰 `missing`（unreadable 语义，不折进失联）。
    Unreadable,
}

/// `io::Error` 是否等价于「文件不在了」：ENOENT(2) / ENOTDIR(20)。
///
/// ENOTDIR 是路径中间段被删或被换成普通文件的典型形态（`/a/b.jpg` 里 `a` 没了，
/// stat 返回的是 ENOTDIR 而不是 ENOENT），语义上同样是「失联」，必须一起判。
pub fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20)
}

/// 读失败点记账：只在「不在了」时写 `missing=1`，返回本次是否标了。
///
/// 幂等（`missing=0` 才更新），与 watcher 的 `assets SET missing=1 WHERE absPath=?` 同口径，
/// 不引入新判定逻辑。
pub fn mark_missing_on_error(
    conn: &Connection,
    asset_id: i64,
    e: &io::Error,
) -> rusqlite::Result<bool> {
    if !is_gone(e) {
        return Ok(false);
    }
    let n = conn.execute(
        "UPDATE assets SET missing=1 WHERE assetId=?1 AND deleted=0 AND missing=0",
        rusqlite::params![asset_id],
    )?;
    Ok(n > 0)
}

/// 读成功点清标（`missing=1` → 0）：文件回来了就顺手接回，不需要额外核查动作。
pub fn clear_missing(conn: &Connection, asset_id: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE assets SET missing=0 WHERE assetId=?1 AND deleted=0 AND missing=1",
        rusqlite::params![asset_id],
    )
}

/// 现场 `stat` + 记账一步到位（导出原图 / 读原图 / 相似图重算共用）。
///
/// 只做一次 `metadata`（跟随符号链接；悬空链接 → NotFound），不遍历、不重试。
/// DB 错误一律忽略（记账失败不能把一次正常读/一次正常跳过变成失败）。
pub fn probe_and_note(conn: &Connection, asset_id: i64, path: &Path) -> Presence {
    match std::fs::metadata(path) {
        Ok(_) => {
            let _ = clear_missing(conn, asset_id);
            Presence::Present
        }
        Err(e) if is_gone(&e) => {
            let _ = mark_missing_on_error(conn, asset_id, &e);
            Presence::Gone
        }
        Err(_) => Presence::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO assets (assetId, absPath, missing, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (1, '/x/a.jpg', 1, 'a.jpg', 'jpg', 1, 1, 1),
                        (2, '/x/b.jpg', 0, 'b.jpg', 'jpg', 1, 1, 2);",
        )
        .unwrap();
        conn
    }

    fn missing_of(conn: &Connection, id: i64) -> i64 {
        conn.query_row(
            "SELECT missing FROM assets WHERE assetId=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn is_gone_covers_not_found_and_enotdir() {
        assert!(is_gone(&io::Error::from(io::ErrorKind::NotFound)));
        // ENOTDIR：路径中间段被删/被换成普通文件
        assert!(is_gone(&io::Error::from_raw_os_error(20)));
        // 权限 / 其它 IO 错误不折进失联（unreadable 语义）
        assert!(!is_gone(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_gone(&io::Error::from_raw_os_error(5)));
    }

    #[test]
    fn permission_denied_never_marks_missing() {
        let conn = setup();
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!mark_missing_on_error(&conn, 2, &e).unwrap());
        assert_eq!(missing_of(&conn, 2), 0, "读不了 ≠ 失联");
    }

    #[test]
    fn not_found_marks_and_clear_restores() {
        let conn = setup();
        let e = io::Error::from(io::ErrorKind::NotFound);
        assert!(mark_missing_on_error(&conn, 2, &e).unwrap());
        assert_eq!(missing_of(&conn, 2), 1);
        // 幂等
        assert!(!mark_missing_on_error(&conn, 2, &e).unwrap());
        assert_eq!(clear_missing(&conn, 2).unwrap(), 1);
        assert_eq!(missing_of(&conn, 2), 0);
    }

    #[test]
    fn probe_and_note_goes_both_ways_on_the_same_path() {
        let conn = setup();
        let dir = std::env::temp_dir().join(format!("presence-{}-{}", std::process::id(), 7));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.jpg");
        std::fs::write(&f, b"x").unwrap();
        conn.execute("UPDATE assets SET absPath=?1 WHERE assetId=1", [f.to_string_lossy()])
            .unwrap();
        // 之前被标失联（文件其实在）→ 读成功即清标
        assert_eq!(probe_and_note(&conn, 1, &f), Presence::Present);
        assert_eq!(missing_of(&conn, 1), 0);
        // 删掉 → 不在了 → 立标
        std::fs::remove_file(&f).unwrap();
        assert_eq!(probe_and_note(&conn, 1, &f), Presence::Gone);
        assert_eq!(missing_of(&conn, 1), 1);
        // 放回 → 再接回
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(probe_and_note(&conn, 1, &f), Presence::Present);
        assert_eq!(missing_of(&conn, 1), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_with_file_in_the_middle_is_gone() {
        let conn = setup();
        let dir = std::env::temp_dir().join(format!("presence-mid-{}-{}", std::process::id(), 8));
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("sub");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let f = blocker.join("a.jpg"); // 中间段是普通文件 → ENOTDIR
        conn.execute("UPDATE assets SET absPath=?1 WHERE assetId=2", [f.to_string_lossy()])
            .unwrap();
        assert_eq!(probe_and_note(&conn, 2, &f), Presence::Gone);
        assert_eq!(missing_of(&conn, 2), 1, "目录整体消失/被替换也算失联");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
