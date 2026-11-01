//! 局部重绘（C4 inpaint 插件底座）：big-lama ONNX FP32（Carve 导出，512² 固定输入）。
//!
//! I/O 契约（模型卡锁定）：`image` [1,3,512,512] f32 0..255 RGB；`mask` [1,1,512,512]
//! f32 {0,1}，**1 = 待补洞**；输出 [1,3,512,512] f32 0..255（部分导出为 0..1，
//! 输出量纲自适应：max ≤ 1.001 → ×255）。输入名按「含 mask 即 mask、否则第二个」
//! 分派（两种导出命名都拿得住，不赌顺序）。
//!
//! 流水线（IOPaint 同语义）：image+mask 等比缩到 512² → 推理 → 输出缩回原尺寸 →
//! **paste-back**（只把掩码区写回原图，掩码外逐字节保真）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use image::imageops::FilterType;
use ort::session::Session;
use ort::value::Tensor;

use crate::{RestoreError, LAMA_FP32, Result};

/// 模型固定输入边长（权重锁定）。
pub const LAMA_SIDE: u32 = 512;

pub struct Inpainter {
    session: Mutex<Session>,
    image_in: String,
    mask_in: String,
    out_name: String,
}

impl std::fmt::Debug for Inpainter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inpainter")
            .field("image", &self.image_in)
            .field("mask", &self.mask_in)
            .finish()
    }
}

/// 载入掩码文件（灰度/任意格式，>127 视为待补洞），返回 0/255 缓冲（len = w*h）。
pub fn load_mask_path(path: &Path, expect_len: usize) -> Result<Vec<u8>> {
    let img = image::ImageReader::open(path)
        .map_err(|e| RestoreError::Failed {
            plugin: "inpaint",
            msg: format!("掩码打开失败 {}: {e}", path.display()),
        })?
        .with_guessed_format()
        .map_err(|e| RestoreError::Failed {
            plugin: "inpaint",
            msg: format!("掩码格式识别失败 {}: {e}", path.display()),
        })?
        .decode()
        .map_err(|e| RestoreError::Failed {
            plugin: "inpaint",
            msg: format!("掩码解码失败 {}: {e}", path.display()),
        })?;
    let gray = img.to_luma8();
    let mut out = vec![0u8; gray.width() as usize * gray.height() as usize];
    for (i, &v) in gray.as_raw().iter().enumerate() {
        if v > 127 {
            out[i] = 255;
        }
    }
    if out.len() != expect_len {
        return Err(RestoreError::Failed {
            plugin: "inpaint",
            msg: format!(
                "掩码尺寸 {} 与图面 {expect_len} 像素不符（掩码必须与原图同宽高）",
                out.len()
            ),
        });
    }
    Ok(out)
}

/// 相对矩形 [x0,y0,x1,y1]（0..1）→ 0/255 掩码缓冲（矩形内 255）。
pub fn mask_from_box(w: u32, h: u32, b: [f64; 4]) -> Result<Vec<u8>> {
    let [x0, y0, x1, y1] = b;
    let ok = (0.0..=1.0).contains(&x0)
        && (0.0..=1.0).contains(&y0)
        && (0.0..=1.0).contains(&x1)
        && (0.0..=1.0).contains(&y1)
        && x0 < x1
        && y0 < y1;
    if !ok {
        return Err(RestoreError::BadParams {
            plugin: "inpaint",
            msg: format!("box 须 0 ≤ x0 < x1 ≤ 1 / 0 ≤ y0 < y1 ≤ 1，实际 {b:?}"),
        });
    }
    let (iw, ih) = (w as usize, h as usize);
    let mut m = vec![0u8; iw * ih];
    let (ax, ay) = ((x0 * w as f64) as usize, (y0 * h as f64) as usize);
    let (bx, by) = (
        ((x1 * w as f64).ceil() as usize).min(iw),
        ((y1 * h as f64).ceil() as usize).min(ih),
    );
    for y in ay..by {
        for x in ax..bx {
            m[y * iw + x] = 255;
        }
    }
    Ok(m)
}

impl Inpainter {
    pub fn load(onnx: &std::path::Path) -> Result<Self> {
        if !onnx.is_file() {
            return Err(RestoreError::ModelMissing(onnx.display().to_string()));
        }
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        let session = Session::builder()
            .and_then(|b| b.with_intra_threads(threads))
            .and_then(|b| b.commit_from_file(onnx))
            .map_err(|e| RestoreError::Onnx(format!("{}: {e}", onnx.display())))?;
        if session.inputs.len() < 2 {
            return Err(RestoreError::Onnx(format!(
                "lama 需要 image+mask 两个输入，实际 {}",
                session.inputs.len()
            )));
        }
        // 输入分派：名字含 mask → mask；否则默认 [image, mask] 顺序
        let (image_in, mask_in) = match (session.inputs[0].name.contains("mask"), session.inputs[1].name.contains("mask")) {
            (false, true) => (session.inputs[0].name.clone(), session.inputs[1].name.clone()),
            (true, false) => (session.inputs[1].name.clone(), session.inputs[0].name.clone()),
            _ => (session.inputs[0].name.clone(), session.inputs[1].name.clone()),
        };
        let out_name = session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .ok_or_else(|| RestoreError::Onnx("lama has no outputs".into()))?;
        Ok(Self {
            session: Mutex::new(session),
            image_in,
            mask_in,
            out_name,
        })
    }

    /// RGB + 掩码 → 补绘后的 RGB（掩码外逐字节保真）。确定性（CPU EP）。
    pub fn run_rgb_mask(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
        mask: &[u8],
    ) -> Result<Vec<u8>> {
        const P: &str = "inpaint";
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || pixels.len() != w * h * 3 {
            return Err(RestoreError::Failed {
                plugin: P,
                msg: format!("RGB 缓冲 {w}x{h} 期望 {} 字节，实际 {}", w * h * 3, pixels.len()),
            });
        }
        if mask.len() != w * h {
            return Err(RestoreError::Failed {
                plugin: P,
                msg: format!("掩码 {} 字节与 {w}x{h} 不符", mask.len()),
            });
        }
        if mask.iter().all(|&v| v == 0) {
            return Err(RestoreError::BadParams {
                plugin: P,
                msg: "掩码全空：无待补洞区域（拒绝无意义推理）".into(),
            });
        }
        // 缩到 512²（图 Triangle / 掩码 Nearest）
        let side = LAMA_SIDE as usize;
        let src = image::RgbImage::from_raw(width, height, pixels.to_vec())
            .ok_or_else(|| RestoreError::Failed { plugin: P, msg: "RGB 缓冲非法".into() })?;
        let small = image::imageops::resize(&src, LAMA_SIDE, LAMA_SIDE, FilterType::Triangle);
        let mask_img = image::GrayImage::from_raw(width, height, mask.to_vec())
            .ok_or_else(|| RestoreError::Failed { plugin: P, msg: "掩码缓冲非法".into() })?;
        let mask_small = image::imageops::resize(&mask_img, LAMA_SIDE, LAMA_SIDE, FilterType::Nearest);

        // NCHW 输入
        let mut img_t = vec![0f32; 3 * side * side];
        let mut msk_t = vec![0f32; side * side];
        let n = side * side;
        for i in 0..n {
            let p = small.as_raw();
            img_t[i] = f32::from(p[i * 3]);
            img_t[n + i] = f32::from(p[i * 3 + 1]);
            img_t[2 * n + i] = f32::from(p[i * 3 + 2]);
            msk_t[i] = if mask_small.as_raw()[i] > 127 { 1.0 } else { 0.0 };
        }
        let ti = Tensor::from_array((vec![1usize, 3, side, side], img_t.into_boxed_slice()))
            .map_err(|e| RestoreError::Onnx(e.to_string()))?;
        let tm = Tensor::from_array((vec![1usize, 1, side, side], msk_t.into_boxed_slice()))
            .map_err(|e| RestoreError::Onnx(e.to_string()))?;
        let mut session = self.session.lock().expect("lama session lock");
        let outputs = session
            .run(ort::inputs![
                self.image_in.as_str() => ti,
                self.mask_in.as_str() => tm,
            ])
            .map_err(|e| RestoreError::Onnx(format!("run: {e}")))?;
        let value = outputs
            .get(self.out_name.as_str())
            .ok_or_else(|| RestoreError::Onnx(format!("output {} missing", self.out_name)))?;
        let arr = value
            .try_extract_array::<f32>()
            .map_err(|e| RestoreError::Onnx(format!("extract: {e}")))?;
        let out: Vec<f32> = arr.iter().copied().collect();
        if out.len() != 3 * n {
            return Err(RestoreError::Onnx(format!(
                "输出形状非 3×{side}²：{} 元素",
                out.len()
            )));
        }
        let peak = out.iter().fold(0f32, |m, &v| m.max(v));
        let gain = if peak <= 1.001 { 255.0f32 } else { 1.0 };
        let mut small_u8 = vec![0u8; 3 * n];
        for (i, &v) in out.iter().enumerate() {
            small_u8[i] = (v * gain).clamp(0.0, 255.0).round() as u8;
        }
        let filled = image::RgbImage::from_raw(LAMA_SIDE, LAMA_SIDE, small_u8)
            .ok_or_else(|| RestoreError::Failed { plugin: P, msg: "输出缓冲非法".into() })?;
        let back = image::imageops::resize(&filled, width, height, FilterType::Triangle);

        // paste-back：掩码区写补绘，掩码外逐字节保真
        let mut px = pixels.to_vec();
        for (i, &m) in mask.iter().enumerate() {
            if m > 127 {
                px[i * 3] = back.as_raw()[i * 3];
                px[i * 3 + 1] = back.as_raw()[i * 3 + 1];
                px[i * 3 + 2] = back.as_raw()[i * 3 + 2];
            }
        }
        Ok(px)
    }
}

static INPAINTERS: Mutex<Option<HashMap<PathBuf, Arc<Inpainter>>>> = Mutex::new(None);

/// 按模型名取缓存会话（首次调用加载；文件路径为缓存键）。
pub fn inpainter(model: &str) -> Result<Arc<Inpainter>> {
    if model != LAMA_FP32 {
        return Err(RestoreError::BadParams {
            plugin: "inpaint",
            msg: format!("未知重绘模型 {model}（已知: {LAMA_FP32}）"),
        });
    }
    let path = crate::model_file(model)?;
    let mut guard = INPAINTERS.lock().expect("inpainter cache lock");
    if let Some(cache) = guard.as_mut() {
        if let Some(s) = cache.get(&path) {
            return Ok(s.clone());
        }
    } else {
        *guard = Some(HashMap::new());
    }
    let s = Arc::new(Inpainter::load(&path)?);
    guard.as_mut().expect("just filled").insert(path, s.clone());
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_from_box_rect_and_validation() {
        let m = mask_from_box(100, 80, [0.1, 0.25, 0.5, 0.75]).unwrap();
        assert_eq!(m.len(), 100 * 80);
        let x0 = 10usize;
        let y0 = 20usize;
        let x1 = 50usize;
        let y1 = 60usize;
        assert_eq!(m[y0 * 100 + x0], 255);
        assert_eq!(m[(y1 - 1) * 100 + (x1 - 1)], 255);
        assert_eq!(m[y0 * 100 + x1], 0, "矩形右边界外");
        assert_eq!(m[(y1) * 100 + x0], 0, "矩形下边界外");
        for bad in [
            [0.5, 0.0, 0.5, 0.5],  // x0 == x1
            [0.5, 0.5, 0.2, 0.8],  // x1 < x0
            [-0.1, 0.0, 0.5, 0.5], // 越界
            [0.0, 0.0, 1.2, 0.5],
        ] {
            assert!(mask_from_box(100, 80, bad).is_err(), "{bad:?} 应响亮拒绝");
        }
    }

    #[test]
    fn unknown_model_fails_loudly() {
        let err = inpainter("nope").unwrap_err();
        assert!(err.to_string().contains("未知重绘模型"));
    }

    /// 真模型 smoke（独立验收路径，调试直达引擎层）：DAM_TEST_MODELS_DIR=/models。
    #[test]
    fn real_lama_inpaint_smoke() {
        let Ok(src) = std::env::var("DAM_TEST_MODELS_DIR") else {
            eprintln!("skip: DAM_TEST_MODELS_DIR 未设置");
            return;
        };
        let root = std::env::temp_dir().join(format!("dam-c4ip-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("models")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            std::path::Path::new(&src).join("lama-fp32"),
            root.join("models").join("lama-fp32"),
        )
        .unwrap();
        // 进程级模型根：取锁 + drop 还原（持有到用例结束 —— 同二进制并行的
        // upscale::tests::unknown_model_fails_loudly 曾会把本用例的根清掉）
        let _root_scope = crate::ModelsRootScope::set(&root);
        let n = 64usize;
        let mut px = vec![235u8; n * n * 3];
        for y in 24..40 {
            for x in 24..40 {
                let i = (y * n + x) * 3;
                px[i] = 200;
                px[i + 1] = 30;
                px[i + 2] = 30;
            }
        }
        let mut mask = vec![0u8; n * n];
        for y in 24..40 {
            for x in 24..40 {
                mask[y * n + x] = 255;
            }
        }
        let ip = inpainter(LAMA_FP32).expect("lama 加载");
        match ip.run_rgb_mask(n as u32, n as u32, &px, &mask) {
            Ok(out) => {
                // 掩码外保真（原图 = 全 235 均匀底）
                assert_eq!((out[0], out[1], out[2]), (235, 235, 235));
                let c = (32 * n + 32) * 3;
                eprintln!("LAMA_SMOKE_OK center=({}, {}, {})", out[c], out[c + 1], out[c + 2]);
            }
            Err(e) => panic!("lama 推理失败: {e}"),
        }
    }
}
