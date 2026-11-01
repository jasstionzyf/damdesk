//! L1 `engine-classic` — 纯 Rust 经典算法（无模型依赖）。
//! D4 落地主色/调色板/均值 hash；C1 落地合规四检测器 + 静态注册表
//! （进程内注册，D12 裁决；未知 detector id 显式报错不静默跳过）。

pub mod detectors;
pub mod registry;

/// RGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// 主色提取：输入任意像素数组（stride=3, RGB），返回按占比降序的调色板（≤k 色）。
/// D4 实现（中位切分或 kmeans-lite）；v1 接口先冻结。
pub fn dominant_colors(_pixels: &[u8], _k: usize) -> Vec<(Rgb, f32)> {
    // 骨架：D4 装中位切分
    Vec::new()
}

/// aHash（均值哈希）：输入 8x8 灰度，返回 64bit。与 pHash 互补的快速粗筛。
pub fn ahash64(luma8x8: &[u8]) -> u64 {
    debug_assert_eq!(luma8x8.len(), 64);
    let mean = luma8x8.iter().map(|&b| b as u32).sum::<u32>() as f64 / 64.0;
    let mut h = 0u64;
    for (i, &b) in luma8x8.iter().enumerate().take(64) {
        if b as f64 > mean {
            h |= 1 << i;
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ahash_flat_is_zero() {
        assert_eq!(ahash64(&[128u8; 64]), 0);
    }

    #[test]
    fn ahash_halfbright_halfset() {
        let mut g = [255u8; 64];
        g[32..].fill(0);
        let h = ahash64(&g);
        assert_eq!(h.count_ones(), 32);
    }
}
