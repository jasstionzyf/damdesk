//! VL 打标前的图像归一化（修 `DESKTOP-TAG-CTX-1`）。
//!
//! 现象：桌面版把**原图**直送 VL 打标，3072² 的图在 vLLM 侧是 10502 token，
//! 超过模型 8192 上下文 → `vLLM HTTP 400: Input length (10502) exceeds model's maximum
//! context length (8192)`，同一张图重试 3 次都失败（重试救不了输入超限）。
//!
//! 处置：送 VL 前统一走 [`load_for_vl`] —— 按**短边**归一到 [`VL_SHORT_SIDE`] 再编码成 JPEG。
//! 3072² → 512² 时像素降到 1/36（≈330 token @28px/patch），远低于 8192。
//!
//! 为什么以**短边**为准（而不是长边）：长边约束会惩罚竖图/长图——800×3000 按长边 768 归一会变成
//! 205×768（短边只剩 205 px，细节已被毁、标签质量下降）；按短边 512 归一则是 512×1920，
//! 比例不变、短边可用。极端长图再由长边上限 [`VL_MAX_LONG_SIDE`] 兜底防爆 token。
//!
//! 取舍说明：
//! - **短边 512 是双向的**：小于 512 的图也等比放大到 512（VL 模型在过小输入上判别力差）。
//! - **解码失败就原样送**（如 HEIC/RAW 等本 crate 解不了的格式）：不能因为"缩不了"就把打标变成失败，
//!   保持与修复前一致的宽进行为，真超限时仍由上游报错。
//! - **一律重编码成 JPEG**：格式归一（不再把 30MB TIFF/PNG 原样 base64），
//!   也是各 VL provider（dam-ai /v1/tagging、OpenAI 兼容）都能吃的最大公约数。

use std::path::Path;

use crate::http::mime_for;

/// VL 打标输入图的**短边目标**（像素）：等比缩放使短边 = 该值（小图也放大）。
pub const VL_SHORT_SIDE: i32 = 512;

/// 极端长图保护：按短边归一后若长边仍超过此值，再按长边等比压回来（防 panoramas 爆 token）。
pub const VL_MAX_LONG_SIDE: i32 = 4096;

/// 计算归一化后的目标尺寸（等比、不裁切）：短边 → [`VL_SHORT_SIDE`]，长边不超 [`VL_MAX_LONG_SIDE`]。
pub fn target_dims(w: u32, h: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }
    let short = w.min(h) as f64;
    let scale = VL_SHORT_SIDE as f64 / short;
    let mut nw = ((w as f64) * scale).round().max(1.0);
    let mut nh = ((h as f64) * scale).round().max(1.0);
    let long = nw.max(nh);
    if long > VL_MAX_LONG_SIDE as f64 {
        let back = VL_MAX_LONG_SIDE as f64 / long;
        nw = (nw * back).round().max(1.0);
        nh = (nh * back).round().max(1.0);
    }
    (nw as u32, nh as u32)
}

/// 读图 → `(字节, mime)`，供拼接 base64 data URL。
///
/// 返回的字节**保证**是短边 = [`VL_SHORT_SIDE`] 的 JPEG（解码失败时回退为原文件字节 + 猜测 mime）。
pub fn load_for_vl(path: &Path) -> crate::Result<(Vec<u8>, String)> {
    let bytes = (|| -> core_media::Result<Vec<u8>> {
        let (w, h) = core_media::image_size(path)?;
        let (nw, nh) = target_dims(w.max(0) as u32, h.max(0) as u32);
        core_media::vl_jpeg(path, nw.max(nh) as i32)
    })();
    match bytes {
        Ok(bytes) => Ok((bytes, "image/jpeg".to_string())),
        Err(_) => Ok((std::fs::read(path)?, mime_for(path).to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_uses_short_side_rule() {
        assert_eq!(target_dims(3072, 3072), (512, 512));
    }

    #[test]
    fn landscape_and_portrait_keep_aspect() {
        assert_eq!(target_dims(3000, 800), (1920, 512));
        assert_eq!(target_dims(800, 3000), (512, 1920));
    }

    #[test]
    fn small_image_is_upscaled_to_short_side() {
        assert_eq!(target_dims(200, 150), (683, 512));
    }

    #[test]
    fn extreme_pano_is_capped_by_long_side() {
        // 512×20000：短边规则会得到 512×20000 → 长边上限 4096 压回来
        let (w, h) = target_dims(512, 20000);
        assert_eq!(h, 4096);
        assert!(w > 100 && w < 110, "w={w}");
    }
}
