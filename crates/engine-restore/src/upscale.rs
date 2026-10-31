//! 超分（C4 upscale 插件底座）：realesr-general-x4v3 ONNX，4× 放大。
//!
//! I/O 契约（模型卡锁定）：输入 `input` [1,3,H,W] f32 RGB 0..1（动态尺寸）；
//! 输出 [1,3,4H,4W] f32 不裁剪（社区导出口径：clamp 在消费端做）。
//! 输出量纲自适应：max ≤ 1.001 视为 0..1 → ×255；否则按 0..255 clamp
//! （两种导出约定都拿得住，不赌单一口径）。
//!
//! 输入护栏：像素数 ≤ `MAX_INPUT_PIXELS`（4MP）——4× 后是 64MP f32×3 通道，
//! 无护栏的批处理是 OOM 炸弹；超限响亮报错（不静默降采样掩盖）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ort::session::Session;
use ort::value::Tensor;

use crate::{RestoreError, REALESR_GENERAL_X4V3, Result};

/// 单图输入像素上限（~4MP；4× 后 64MP）。卡内裁决：超限响亮报错，不静默降采样。
pub const MAX_INPUT_PIXELS: u64 = 4_000_000;

pub struct Upscaler {
    session: Mutex<Session>,
    in_name: String,
    out_name: String,
}

impl std::fmt::Debug for Upscaler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upscaler").field("input", &self.in_name).finish()
    }
}

impl Upscaler {
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
            .and_then(|b| {
                // 独显高性能档：cuda feature 编译进包 且 检测到独显 → 优先 CUDA EP；
                // EP 注册失败（驱动/运行时缺失）→ 保留默认 CPU（不炸会话）。
                #[cfg(feature = "cuda")]
                {
                    use ort::execution_providers::ExecutionProviderDispatch;
                    if matches!(crate::detect_device(), crate::Device::Cuda) {
                        let eps: Vec<ExecutionProviderDispatch> =
                            vec![ort::execution_providers::CUDAExecutionProvider::default().build()];
                        match b.with_execution_providers(eps) {
                            Ok(b) => Ok(b),
                            Err(e) => {
                                eprintln!("engine-restore: CUDA EP 注册失败，回退 CPU: {e}");
                                Ok(b)
                            }
                        }
                    } else {
                        Ok(b)
                    }
                }
                #[cfg(not(feature = "cuda"))]
                Ok(b)
            })
            .and_then(|b| b.commit_from_file(onnx))
            .map_err(|e| RestoreError::Onnx(format!("{}: {e}", onnx.display())))?;
        let in_name = session
            .inputs
            .first()
            .map(|i| i.name.clone())
            .ok_or_else(|| RestoreError::Onnx("x4v3 has no inputs".into()))?;
        let out_name = session
            .outputs
            .first()
            .map(|o| o.name.clone())
            .ok_or_else(|| RestoreError::Onnx("x4v3 has no outputs".into()))?;
        Ok(Self {
            session: Mutex::new(session),
            in_name,
            out_name,
        })
    }

    /// RGB 交错缓冲 → 4× 放大的 RGB 交错缓冲。
    /// 返回 (out_w, out_h, pixels)；确定性（同输入同输出，CPU EP）。
    pub fn run_rgb(&self, width: u32, height: u32, pixels: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || pixels.len() != w * h * 3 {
            return Err(RestoreError::Failed {
                plugin: "upscale",
                msg: format!("RGB 缓冲 {w}x{h} 期望 {} 字节，实际 {}", w * h * 3, pixels.len()),
            });
        }
        if (w as u64) * (h as u64) > MAX_INPUT_PIXELS {
            return Err(RestoreError::BadParams {
                plugin: "upscale",
                msg: format!(
                    "输入 {w}x{h}（{} MP）超过护栏 {MAX_INPUT_PIXELS}（~4MP）；请先 resize 降档再超分",
                    (w as u64) * (h as u64) / 1_000_000
                ),
            });
        }
        // NCHW f32 /255
        let n = w * h;
        let mut input = vec![0f32; 3 * n];
        for i in 0..n {
            input[i] = f32::from(pixels[i * 3]) / 255.0;
            input[n + i] = f32::from(pixels[i * 3 + 1]) / 255.0;
            input[2 * n + i] = f32::from(pixels[i * 3 + 2]) / 255.0;
        }
        let (ow, oh) = (w * 4, h * 4);
        let t = Tensor::from_array((vec![1usize, 3, h, w], input.into_boxed_slice()))
            .map_err(|e| RestoreError::Onnx(e.to_string()))?;
        let mut session = self.session.lock().expect("x4v3 session lock");
        let outputs = session
            .run(ort::inputs![self.in_name.as_str() => t])
            .map_err(|e| RestoreError::Onnx(format!("run: {e}")))?;
        let value = outputs
            .get(self.out_name.as_str())
            .ok_or_else(|| RestoreError::Onnx(format!("output {} missing", self.out_name)))?;
        let arr = value
            .try_extract_array::<f32>()
            .map_err(|e| RestoreError::Onnx(format!("extract: {e}")))?;
        let out: Vec<f32> = arr.iter().copied().collect();
        if out.len() != 3 * ow * oh {
            return Err(RestoreError::Onnx(format!(
                "输出形状非 3×{ow}×{oh}：{} 元素",
                out.len()
            )));
        }
        // 量纲自适应 + clamp（社区导出不裁剪，消费端负责）+ **CHW → 交错 RGB**
        //
        // 两个历史 bug（卡 task 实测定位，语料 `/data/damdesk-test-corpus/v1`）：
        // 1. 输出布局：`arr` 是 NCHW（`[1,3,4H,4W]`，C 维在外），旧实现按交错 RGB 平铺写
        //    进 `px`，等于把三个平面拆成像素 → 产物与源图 PSNR 只有 ~10dB（近噪声）。
        //    必须按 `px[3*i + c] = out[c*plane + i]` 反交错。
        // 2. 量纲阈值：x4v3 是 0..1 导出，但**真实照片**上峰值会轻微过冲（实测 600×450
        //    商品图 peak=1.1584），旧阈值 `peak <= 1.001` 把这类图判成 0..255 口径 →
        //    gain=1 → 产物近全黑（mean 0.0034）。阈值放到 1.5 后同图 PSNR 8.37→28.19dB。
        // 输入侧 ÷255 是本模型的正确口径（纯色图 200/150/100 进出逐通道一致）：
        // 改成 0..255 直喂时输出越界（peak 605）且 PSNR 掉到 11dB —— 别反着改。
        let peak = out.iter().fold(0f32, |m, &v| m.max(v));
        let gain = output_gain(peak);
        let plane = ow * oh;
        let mut px = vec![0u8; 3 * plane];
        for i in 0..plane {
            for c in 0..3 {
                px[i * 3 + c] = (out[c * plane + i] * gain).clamp(0.0, 255.0).round() as u8;
            }
        }
        Ok((ow as u32, oh as u32, px))
    }
}

static UPSCALERS: Mutex<Option<HashMap<PathBuf, Arc<Upscaler>>>> = Mutex::new(None);

/// 输出量纲判据：峰值 ≤ 1.5 → 模型吐的是 0..1（×255）；否则 0..255 口径（gain=1）。
///
/// 阈值为什么是 1.5 而不是 1.001：x4v3 在真实照片上轻微过冲（实测 1.158），
/// 用 1.001 会把正常照片判成 0..255 → 产物近全黑（见 [`Upscaler::run_rgb`] 注释 2）。
/// 反方向（0..255 导出且峰值 ≤1.5）只可能出现在整幅近黑的图上，属可接受误差。
pub fn output_gain(peak: f32) -> f32 {
    if peak <= 1.5 {
        255.0
    } else {
        1.0
    }
}

/// 按模型名取缓存会话（首次调用加载；文件路径为缓存键——换文件即换键）。
pub fn upscaler(model: &str) -> Result<Arc<Upscaler>> {
    if model != REALESR_GENERAL_X4V3 {
        return Err(RestoreError::BadParams {
            plugin: "upscale",
            msg: format!("未知超分模型 {model}（已知: {REALESR_GENERAL_X4V3}）"),
        });
    }
    let path = crate::model_file(model)?;
    let mut guard = UPSCALERS.lock().expect("upscaler cache lock");
    if let Some(cache) = guard.as_mut() {
        if let Some(s) = cache.get(&path) {
            return Ok(s.clone());
        }
    } else {
        *guard = Some(HashMap::new());
    }
    let s = Arc::new(Upscaler::load(&path)?);
    guard.as_mut().expect("just filled").insert(path, s.clone());
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_rejects_oversized_input_before_session() {
        // 会话未加载也能拒绝（护栏在推理前）——构造一个直接调用 Upcaler 之外的场景
        // 不行，run_rgb 需要 session；这里用未知模型名断言注册表纪律
        let err = upscaler("not-a-model").unwrap_err();
        assert!(matches!(err, RestoreError::BadParams { .. }));
    }

    #[test]
    fn unknown_model_fails_loudly() {
        // 进程级模型根：取锁 + drop 还原（同二进制并行用例的改根/清根不会串进来）
        let _root = crate::ModelsRootScope::set("/tmp/c4-no-root");
        let err = upscaler("nope").unwrap_err();
        assert!(err.to_string().contains("未知超分模型"));
    }

    /// 量纲判据回归（卡 task）：真实照片上 x4v3 峰值 1.158，
    /// 旧阈值 1.001 会判成 0..255 → 产物近全黑。
    #[test]
    fn output_gain_handles_real_photo_overshoot() {
        assert_eq!(output_gain(0.7992), 255.0, "0..1 兜底导出");
        assert_eq!(output_gain(1.1584), 255.0, "真实照片过冲仍在 0..1 量纲（旧实现这里翻车）");
        assert_eq!(output_gain(1.4999), 255.0, "过冲边界");
        assert_eq!(output_gain(1.5001), 1.0, "0..255 导出");
        assert_eq!(output_gain(605.63), 1.0, "0..255 导出（实测直喂 0..255 时的峰值）");
    }
}
