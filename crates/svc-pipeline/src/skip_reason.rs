//! 跳过原因词表（**唯一口径**，卡 task / CMP-JOB-FIX-1）。
//!
//! `job_queue` 里唯一可写的自由文本列是 `lastError`：跳过原因也写在这里，靠**前缀**与真失败
//! 原文区分（跳过与成功同终态 `done`，没有别的地方能区分）。所以前缀词表必须是**一份**，
//! 后端、界面、E2E 探针三处同口径：
//!
//! | 前缀 | 含义 | 谁写 |
//! |------|------|------|
//! | `deleted:` | 资产已被删除（`assets.deleted=1`） | `compliance_queue::process_job` 资产状态分支 |
//! | `file_missing:` | 资产失联（`assets.missing=1`，或磁盘上找不到） | 同上 / `image_probe::Probe::Missing` |
//! | `not_image_media:` | 媒体类型不是图片（`assets.mediaType != 'image'`） | `process_job` 资产状态分支 |
//! | `not_image:` | 文件魔数不是任何图片格式（坏字节 `.jpg`） | `image_probe::Probe::NotImage` |
//! | `unreadable:` | 读不出（权限等 IO 错）——**不是跳过**，必须响亮失败 | `image_probe::skip_reason` 只在日志用，不落库 |
//!
//! 同口径副本（改一处必须同步改另外两处，E2E 会抓不一致）：
//! - 界面：`web/src/lib/complianceJobs.ts` 的 `SKIP_PREFIXES`（翻译成人话）；
//! - 探针：`scripts/cmp_job_e2e.py` 的 `SKIP_PREFIXES` + `db_counts().skipnote`、
//!   `scripts/cmp_job_e2e_ui.mjs` 的 `isSkip()`。
//!
//! 铁律：**每条被计入 `run.skipped` 的 task 都必须带一条跳过错前缀原因**，
//! 即 `run.skipped == 明细里带跳过前缀的 task 数`（`scripts/cmp_job_e2e.py` P5 硬断言）。

/// 资产已删除（软删）。
pub const PREFIX_DELETED: &str = "deleted:";
/// 资产失联（DB 标了 `missing=1`，或磁盘上找不到）。
pub const PREFIX_FILE_MISSING: &str = "file_missing:";
/// 媒体类型不是图片（视频/文档等）。
pub const PREFIX_NOT_IMAGE_MEDIA: &str = "not_image_media:";
/// 文件魔数不是图片（坏字节、改扩展名伪装、空文件）。
pub const PREFIX_NOT_IMAGE: &str = "not_image:";
/// 读不出（权限等 IO 错）——**不落库当跳过**，只用于日志措辞。
pub const PREFIX_UNREADABLE: &str = "unreadable:";

/// 全部「跳过」前缀（`unreadable:` 刻意不在列表里：那是环境问题，必须响亮失败）。
pub const SKIP_PREFIXES: [&str; 4] = [
    PREFIX_DELETED,
    PREFIX_FILE_MISSING,
    PREFIX_NOT_IMAGE_MEDIA,
    PREFIX_NOT_IMAGE,
];

/// 拼一条跳过原因：`<prefix> <detail>`（前缀与说明之间一个空格，界面按前缀切分）。
pub fn note(prefix: &str, detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} {detail}")
    }
}

/// 这条 `lastError` 是不是「跳过原因」（而不是真失败原文）。
pub fn is_skip(last_error: &str) -> bool {
    let s = last_error.trim_start();
    SKIP_PREFIXES.iter().any(|p| s.starts_with(p))
}

/// 资产状态 → 跳过原因（`None` = 状态可判，继续走管道）。
///
/// 三个分支都**必须**返回原因：这是 CMP-JOB-FIX-1 的根因——原先这里直接
/// `return Ok(AssetOutcome::Skipped)`，run 侧照常 `note_skipped(+1)`，明细却没有任何原因，
/// 于是「跳过 3」与「带原因的 2 条」对不上，界面上那一条还会显示成「已完成 + 上一次失败原文」。
pub fn for_asset_state(deleted: i64, missing: i64, media_type: &str) -> Option<String> {
    if deleted != 0 {
        return Some(note(PREFIX_DELETED, "资产已删除"));
    }
    if missing != 0 {
        return Some(note(PREFIX_FILE_MISSING, "资产已失联（missing=1）"));
    }
    if !media_type.eq_ignore_ascii_case("image") {
        return Some(note(
            PREFIX_NOT_IMAGE_MEDIA,
            &format!("媒体类型 {media_type}"),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_state_always_explains_itself() {
        assert_eq!(for_asset_state(0, 0, "image"), None, "可判资产不该有原因");
        let d = for_asset_state(1, 0, "image").unwrap();
        assert!(d.starts_with(PREFIX_DELETED), "{d}");
        let m = for_asset_state(0, 1, "image").unwrap();
        assert!(m.starts_with(PREFIX_FILE_MISSING), "{m}");
        let v = for_asset_state(0, 0, "video").unwrap();
        assert!(v.starts_with(PREFIX_NOT_IMAGE_MEDIA), "{v}");
        assert!(v.contains("video"), "原因里要能看见真实媒体类型: {v}");
        // 删除优先于失联/媒体类型（三种都成立时给最直接的那条）
        assert!(for_asset_state(1, 1, "video")
            .unwrap()
            .starts_with(PREFIX_DELETED));
    }

    #[test]
    fn skip_detection_matches_vocabulary() {
        for p in SKIP_PREFIXES {
            assert!(is_skip(&format!("{p} 细节")), "{p}");
        }
        assert!(
            !is_skip(&format!("{PREFIX_UNREADABLE} permission denied")),
            "读不出是失败，不是跳过"
        );
        assert!(!is_skip(
            "compliance: rule R0 vl: io: No such file or directory"
        ));
        assert!(!is_skip(""));
    }

    #[test]
    fn not_image_media_does_not_shadow_not_image() {
        // 前缀是「带冒号」的：`not_image_media:` 不能被误判成 `not_image:`
        assert!(!PREFIX_NOT_IMAGE_MEDIA.starts_with(PREFIX_NOT_IMAGE));
    }
}
