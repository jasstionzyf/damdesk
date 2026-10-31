//! 坏图入口收敛（CMP-JOB-01 / 卡 task）：**消费前先看魔数，不看扩展名**。
//!
//! 背景：BATCH-01 造的坏字节 `.jpg`（以及悬空符号链接）一路走到解码 → 每次都抛错 →
//! `job_queue.attempts` 耗尽 → 成为**永久 failed 任务**，于是「历史累计失败 30」永远挂在界面上。
//! 这类文件根本不是「判不出来的图」，而是「不是图」：正确归宿是 `skipped`（本次没有可判对象），
//! 不是 failed，也不该占用 VL 队列与重试预算。
//!
//! 判据（只做魔数，不做完整解码 —— 完整解码是管道里的活，重复做等于白烧 CPU）：
//! - 命中已知图片签名 → [`Probe::Image`]；
//! - 读得到但签名不认识/长度不足 → [`Probe::NotImage`]（垃圾字节、文本、改扩展名伪装）；
//! - 文件不存在（含悬空符号链接）→ [`Probe::Missing`]（资产已失联，与 `missing=1` 同语义）；
//! - 其它 IO 错误（权限不足等）→ [`Probe::Unreadable`]，调用方必须**响亮失败**，不许静默跳过：
//!   「读不出」是环境问题，掩盖它会让用户以为图判过了。
//!
//! 支持签名：JPEG / PNG / GIF / BMP / WebP / TIFF（含 BigTIFF）/ HEIF 系（heic/heix/mif1/avif…）。
//! 桌面版支持的格式（core-media 双后端：jpeg/png/webp + libvips 侧的 heif/tiff）都在列。

use std::io::Read;
use std::path::Path;

/// 探测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 真图片；值为格式名（jpeg/png/…），仅作日志/证据用
    Image(&'static str),
    /// 不是图片（签名不认/文件过短/空文件）
    NotImage(String),
    /// 文件不存在（含悬空符号链接）
    Missing(String),
    /// 其它 IO 错误（权限等）—— 调用方必须响亮报错，不许当跳过
    Unreadable(String),
}

impl Probe {
    /// 是否应跳过：**只有「不是图片」才跳过**。
    ///
    /// 「文件不在了」（含悬空符号链接）**不算跳过**：`assets.missing` 由 watcher 维护，DB 与磁盘
    /// 短暂不一致时元数据档规则其实还能判（C8 规则级隔离会兜住像素档规则的解码失败），把它
    /// 一律跳过等于把「数据不一致」这件事藏起来 —— D2 的教训是这类问题要可见。
    pub fn should_skip(&self) -> bool {
        matches!(self, Probe::NotImage(_))
    }

    /// 跳过原因（写进 task 的 lastError，界面上要能解释「为什么这张没结果」）。
    ///
    /// 前缀取自 [`crate::skip_reason`] 的唯一词表（与资产状态分支、界面、E2E 探针同口径）。
    pub fn skip_reason(&self) -> Option<String> {
        use crate::skip_reason::{note, PREFIX_FILE_MISSING, PREFIX_NOT_IMAGE, PREFIX_UNREADABLE};
        match self {
            Probe::Image(_) => None,
            Probe::NotImage(why) => Some(note(PREFIX_NOT_IMAGE, why)),
            Probe::Missing(why) => Some(note(PREFIX_FILE_MISSING, why)),
            Probe::Unreadable(why) => Some(note(PREFIX_UNREADABLE, why)),
        }
    }
}

/// 头部字节够不够判断（JPEG 最短 3 字节、ftyp 系要 12 字节）。
const HEAD_LEN: usize = 32;

/// 读头部并判签名。
pub fn probe(path: &Path) -> Probe {
    let mut buf = [0u8; HEAD_LEN];
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    Probe::Missing(format!("{}", e))
                }
                _ => Probe::Unreadable(format!("{}", e)),
            }
        }
    };
    let mut read = 0usize;
    loop {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if read == HEAD_LEN {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Probe::Unreadable(format!("{e}")),
        }
    }
    if read == 0 {
        return Probe::NotImage("空文件（0 字节）".to_string());
    }
    match sniff(&buf[..read]) {
        Some(fmt) => Probe::Image(fmt),
        None => Probe::NotImage(format!(
            "魔数不是任何已知图片格式（前 {} 字节: {}）",
            read.min(8),
            hex(&buf[..read.min(8)])
        )),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// 纯函数签名判定（单测直接喂字节）。
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    // JPEG: FF D8 FF
    if b.len() >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF {
        return Some("jpeg");
    }
    // JPEG2000 (.jp2): 00 00 00 0C 6A 50 20 20 0D 0A 87 0A
    if b.len() >= 12 && b[..8] == [0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20] {
        return Some("jp2");
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if b.len() >= 8 && b[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png");
    }
    // GIF: "GIF87a" / "GIF89a"
    if b.len() >= 6 && &b[..4] == b"GIF8" {
        return Some("gif");
    }
    // BMP: "BM"
    if b.len() >= 2 && &b[..2] == b"BM" {
        return Some("bmp");
    }
    // WebP: "RIFF" .... "WEBP"
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("webp");
    }
    // TIFF: "II*\0" / "MM\0*"；BigTIFF: "II+\0" / "MM\0+"
    if b.len() >= 4 {
        let tiff = &b[..4];
        if tiff == [0x49, 0x49, 0x2A, 0x00]
            || tiff == [0x4D, 0x4D, 0x00, 0x2A]
            || tiff == [0x49, 0x49, 0x2B, 0x00]
            || tiff == [0x4D, 0x4D, 0x00, 0x2B]
        {
            return Some("tiff");
        }
    }
    // ISO-BMFF（HEIF 系）：第 4..8 字节 "ftyp"，brand 在 8..12
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        let brand = &b[8..12];
        for (sig, name) in [
            (b"heic", "heic"),
            (b"heix", "heic"),
            (b"heim", "heic"),
            (b"heis", "heic"),
            (b"hevc", "heic"),
            (b"mif1", "heif"),
            (b"msf1", "heif"),
            (b"avif", "avif"),
            (b"avis", "avif"),
        ] {
            if brand == sig {
                return Some(name);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_real_image_signatures() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("jpeg"));
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(sniff(&png), Some("png"));
        assert_eq!(sniff(b"GIF89a...."), Some("gif"));
        assert_eq!(sniff(b"BM12345678"), Some("bmp"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(sniff(&[0x49, 0x49, 0x2A, 0x00, 1, 2]), Some("tiff"));
        // HEIF 系：ftyp 在 4..8，brand 在 8..12
        assert_eq!(sniff(b"\0\0\0\x18ftypheic\0\0"), Some("heic"));
        assert_eq!(sniff(b"\0\0\0\x18ftypavif\0\0"), Some("avif"));
    }

    #[test]
    fn rejects_garbage_and_short_files() {
        // 坏字节 .jpg 的真实形态：随机文本
        assert_eq!(sniff(b"not an image at all, just text"), None);
        assert_eq!(sniff(b"\0\0\0\0"), None);
        assert_eq!(sniff(b"<html><body>404</body>"), None);
        // 只有 2 字节：连 JPEG 的 3 字节都凑不齐
        assert_eq!(sniff(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn probe_distinguishes_missing_notimage_and_unreadable() {
        let dir = std::env::temp_dir().join(format!("dam-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("ok.jpg");
        std::fs::write(&good, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();
        assert_eq!(probe(&good), Probe::Image("jpeg"));
        assert!(!probe(&good).should_skip());

        let bad = dir.join("garbage.jpg");
        std::fs::write(&bad, b"this is not a jpeg").unwrap();
        let p = probe(&bad);
        assert!(p.should_skip(), "{p:?}");
        assert!(p.skip_reason().unwrap().starts_with("not_image:"));

        let empty = dir.join("empty.jpg");
        std::fs::write(&empty, b"").unwrap();
        assert!(probe(&empty).should_skip());

        // 悬空符号链接 = 资产失联（DB 侧 missing 还没被 watcher 标上）→ 不跳过：
        // 元数据档规则还能判，像素档规则由 C8 规则级隔离兜住（不藏问题、也不炸 job）
        #[cfg(unix)]
        {
            let link = dir.join("dangling.jpg");
            std::os::unix::fs::symlink(dir.join("nope.jpg"), &link).unwrap();
            let p = probe(&link);
            assert!(matches!(p, Probe::Missing(_)), "{p:?}");
            assert!(!p.should_skip(), "失联不算「不是图片」，交给管道判: {p:?}");
            assert!(p.skip_reason().unwrap().starts_with("file_missing:"));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
