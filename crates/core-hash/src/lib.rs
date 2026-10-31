//! L0 `core-hash` — SHA-256 与 pHash（感知哈希，8x8 DCT-II）。
//!
//! 分层约定：本 crate 只做纯算法，不碰文件 IO / 图片解码 ——
//! 解码缩放出 32x32 灰度网格是 `core-media` 的职责。

use sha2::{Digest, Sha256};

/// 文件内容 SHA-256，十六进制小写。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// pHash 输入：32x32 灰度（0-255），行主序（row-major）。
pub const PHASH_SRC: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("pHash input must be {0}x{0} luminance grid, got {1} samples")]
    BadGrid(usize, usize),
}

/// 二维 DCT-II 低频块：8x8。
/// 标准正交定义 F(u,v) = (2/N)c(u)c(v)·Σ f(x,y)·cos((2x+1)uπ/2N)·cos((2y+1)vπ/2N)
/// 阈值化只关心相对大小，常数因子不影响结果。
fn dct8x8(luma: &[u8]) -> [[f64; 8]; 8] {
    const N: usize = PHASH_SRC;
    let mut cos_u = [[0f64; N]; 8];
    let mut cos_v = [[0f64; N]; 8];
    for (u, cu) in cos_u.iter_mut().enumerate() {
        for x in 0..N {
            cu[x] =
                ((u as f64) * std::f64::consts::FRAC_PI_2 * (2 * x + 1) as f64 / N as f64).cos();
        }
    }
    for (v, cv) in cos_v.iter_mut().enumerate() {
        for y in 0..N {
            cv[y] =
                ((v as f64) * std::f64::consts::FRAC_PI_2 * (2 * y + 1) as f64 / N as f64).cos();
        }
    }
    let mut out = [[0f64; 8]; 8];
    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0f64;
            for x in 0..N {
                let cx = cos_u[u][x];
                let mut row_sum = 0f64;
                let base = x * N;
                for y in 0..N {
                    row_sum += luma[base + y] as f64 * cos_v[v][y];
                }
                sum += cx * row_sum;
            }
            out[u][v] = sum;
        }
    }
    out
}

/// 8x8 DCT 低频块（除 DC）均值阈值 → 64bit pHash。
/// 汉明距离 ≤8 判近似重复（M5）；输出 MSB-first 位序。
pub fn phash(luma: &[u8]) -> Result<u64, HashError> {
    if luma.len() != PHASH_SRC * PHASH_SRC {
        return Err(HashError::BadGrid(PHASH_SRC, luma.len()));
    }
    let coefs = dct8x8(luma);
    let mut mean = 0f64;
    for u in 0..8 {
        for v in 0..8 {
            if !(u == 0 && v == 0) {
                mean += coefs[u][v];
            }
        }
    }
    mean /= 63.0;
    // 浮点容差：平坦图 AC 系数残差 ~1e-13，不能当有效信号
    const TOL: f64 = 1e-9;
    let mut hash: u64 = 0;
    let mut bit = 0;
    for u in 0..8 {
        for v in 0..8 {
            if !(u == 0 && v == 0) {
                if coefs[u][v] > mean + TOL {
                    hash |= 1 << bit;
                }
                bit += 1;
            }
        }
    }
    Ok(hash)
}

/// pHash 十六进制表示（16 字符）。
pub fn phash_hex(luma: &[u8]) -> Result<String, HashError> {
    Ok(format!("{:016x}", phash(luma)?))
}

/// 两个 pHash 的汉明距离。
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn flat(v: u8) -> Vec<u8> {
        vec![v; PHASH_SRC * PHASH_SRC]
    }

    /// 水平渐变：value = x * 8（0..255）
    fn hgrad() -> Vec<u8> {
        let mut v = vec![0u8; PHASH_SRC * PHASH_SRC];
        for x in 0..PHASH_SRC {
            for y in 0..PHASH_SRC {
                v[x * PHASH_SRC + y] = (x * 8) as u8;
            }
        }
        v
    }

    #[test]
    fn phash_flat_images_hash_zero() {
        // 平坦图所有 AC 系数为 0 → 无系数超过均值(0) → hash 0
        assert_eq!(phash(&flat(128)).unwrap(), 0);
        assert_eq!(phash(&flat(200)).unwrap(), 0);
        assert_eq!(
            hamming(phash(&flat(1)).unwrap(), phash(&flat(250)).unwrap()),
            0
        );
    }

    #[test]
    fn phash_gradient_differs_from_flat() {
        let g = phash(&hgrad()).unwrap();
        assert_ne!(g, 0);
        assert!(hamming(g, 0) > 8);
    }

    #[test]
    fn phash_brightness_shift_stays_close() {
        // 亮度整体平移（JPEG 亮度漂移）应保持近邻：汉明距离 ≤8
        let base = hgrad();
        let mut shifted = base.clone();
        for b in shifted.iter_mut() {
            *b = b.saturating_add(20);
        }
        assert!(hamming(phash(&base).unwrap(), phash(&shifted).unwrap()) <= 8);
    }

    #[test]
    fn phash_grid_size_validated() {
        assert!(phash(&[0u8; 31 * 31]).is_err());
    }

    #[test]
    fn phash_hex_is_16_chars() {
        assert_eq!(phash_hex(&flat(0)).unwrap().len(), 16);
    }
}
