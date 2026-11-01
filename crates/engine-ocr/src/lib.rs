//! L1 `engine-ocr` — 文字识别（OCR-E，PLAN.md「OCR 独立 det+rec + 文本检索加权融合」）。
//!
//! **卡内裁决（2026-10-16）：独立 crate，与 engine-segment/engine-restore 同模式**
//! （推理 L1 独立、分发统一在 engine-embed::model_store、`<root>/models/<model>/` 布局、
//! 模型缺失响亮报错、会话按路径缓存）。类型独立禁互引（分层铁律）。
//!
//! 模型：PP-OCRv6_medium 官方 ONNX（HuggingFace PaddlePaddle 官方仓，Apache-2.0）：
//! - `ppocrv6-medium-det`：inference.onnx（59.2MB，sha256 eb13b44b…），动态输入
//!   [1,3,H,W]（H/W 32 倍数，官方 yml 允许到 4000）；
//! - `ppocrv6-medium-rec`：inference.onnx（73.0MB，sha256 9c09abf0…）+
//!   `ppocrv6_dict.txt`（18,708 字符字典，官方 inference.yml PostProcess.character_dict
//!   原样落盘；CTC blank=idx0，space=末位，类别数 = 18708+2）。
//!
//! 预/后处理锁官方 PaddleX 3.x 口径（det_processors.py / rec_processors.py，2026-10-16
//! develop 分支源码逐行核对；difference 见 reports/OCR-ppocrv6-medium.md「已知简化」）：
//! - det：DetResizeForTest 默认 limit_side_len=736 + limit_type=min（小图放大、大图不缩），
//!   边长 round 到 32 的倍数；NormalizeImage scale 1/255 + ImageNet mean/std（**BGR 序**，
//!   与官方 DecodeImage img_mode=BGR 完全一致——mean 列表按 B,G,R 通道位次套用是官方
//!   历史行为，原样复刻）；DB 后处理 thresh=0.2 / box_thresh=0.45 / unclip_ratio=1.4 /
//!   max_candidates=3000（官方 inference.yml 数值锁定）。
//! - rec：行裁剪 → 高 48 等比缩（宽 = ceil(48*ratio)，钳 [3200,32] 语义同官方
//!   OCRReisizeNormImg）→ /255 偏 0.5 除 0.5（CHW），右 Padding 0.0，批量宽
//!   imgW = int(48 * max(320/48, w/h)) ≤ 3200；CTCLabelDecode greedy（blank 折叠），
//!   score = 保留字符概率均值。
//!
//! **已知简化（响亮声明，不做静默假设）**：DB 后处理取 8-连通域的轴对齐 bbox（官方是
//! minAreaRect 四边形 + pyclipper 外扩）；斜排/弯曲文本行的召回精度会低于官方四边形路径，
//! 横排文本（截图/文档/电商主图）与官方同口径。桌面检索场景以横排为主，四边形路径留待
//! 后续卡（见报告）。
//!
//! 输入统一 RGB 交错（与 core-media::rgb8 / engine-segment 同像素入口），crate 内部
//! 自行换 BGR（官方口径），调用方不感知。

#[cfg(feature = "local-infer")]
pub mod infer;
#[cfg(feature = "local-infer")]
pub use infer::OcrEngine;

use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("模型根未设置：set_models_root() 或环境变量 DAM_MODELS_ROOT")]
    NoModelsRoot,
    #[error("model files missing: {0}")]
    ModelMissing(String),
    #[error("onnx: {0}")]
    Onnx(String),
    #[error("{step}: 输入不符: {msg}")]
    InputMismatch { step: &'static str, msg: String },
    #[error("{step}: {msg}")]
    Failed { step: &'static str, msg: String },
}

pub type Result<T> = std::result::Result<T, OcrError>;

/// PP-OCRv6_medium_det（registry/models.yaml `kind: ocr` 条目名 = manifest 名 = 目录名）。
pub const DET_MODEL: &str = "ppocrv6-medium-det";
/// PP-OCRv6_medium_rec（同上；目录内含 ppocrv6_dict.txt）。
pub const REC_MODEL: &str = "ppocrv6-medium-rec";
/// det onnx 文件名（官方 ONNX 仓原文件名，manifest url_path 同名）。
pub const DET_FILE: &str = "inference.onnx";
/// rec onnx 文件名。
pub const REC_FILE: &str = "inference.onnx";
/// rec 字典文件名（官方 inference.yml character_dict 落盘）。
pub const DICT_FILE: &str = "ppocrv6_dict.txt";

// ---------- 官方口径参数（inference.yml / PaddleX 默认值锁定，改 = 偏离官方） ----------

/// det resize：limit_side_len=736 + limit_type=min（PaddleX DetResizeForTest 默认）。
pub const DET_LIMIT_SIDE: u32 = 736;
/// det 最大边上限（官方 max_side_limit=4000）。
pub const DET_MAX_SIDE: u32 = 4000;
/// DB 二值化阈值（inference.yml thresh）。
pub const DB_THRESH: f32 = 0.2;
/// DB box 得分阈值（inference.yml box_thresh）。
pub const DB_BOX_THRESH: f32 = 0.45;
/// DB 外扩系数（inference.yml unclip_ratio）。
pub const DB_UNCLIP_RATIO: f32 = 1.4;
/// DB 候选连通域上限（inference.yml max_candidates）。
pub const DB_MAX_CANDIDATES: usize = 3000;
/// DB 最小边过滤（官方 min_size=3）。
pub const DB_MIN_SIZE: f32 = 3.0;
/// rec 输入高（官方 rec_image_shape [3,48,320]）。
pub const REC_HEIGHT: u32 = 48;
/// rec 基准宽比（320/48：批量宽下限）。
pub const REC_BASE_WH_RATIO: f64 = 320.0 / 48.0;
/// rec 最大宽（官方 max_imgW=3200）。
pub const REC_MAX_WIDTH: u32 = 3200;

// ---------- 模型根解析（与 engine-segment / engine-embed factory 契约一致） ----------

static MODELS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// 显式注入模型根（桌面壳启动时调用；验收/容器用 DAM_MODELS_ROOT）。
pub fn set_models_root(dir: impl Into<PathBuf>) {
    *MODELS_ROOT.write().expect("models root lock") = Some(dir.into());
}

/// 清除注入（测试隔离用）。
pub fn clear_models_root() {
    *MODELS_ROOT.write().expect("models root lock") = None;
}

/// 生效模型根：显式注入 > `DAM_MODELS_ROOT` > 报错（响亮，不猜默认路径）。
pub fn models_root() -> Result<PathBuf> {
    if let Some(p) = MODELS_ROOT.read().expect("models root lock").clone() {
        return Ok(p);
    }
    if let Ok(env) = std::env::var("DAM_MODELS_ROOT") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    Err(OcrError::NoModelsRoot)
}

/// 模型根下某文件路径：`<root>/models/<model>/<file>`（core_config 布局契约）。
pub fn model_file(model: &str, file: &str) -> Result<PathBuf> {
    let root = models_root()?;
    Ok(core_config::model_dir(&root, model).join(file))
}

/// det/rec/dict 三件是否在盘（快路径，供编排器做 skipped(model_not_downloaded) 预检）。
pub fn models_ready() -> bool {
    matches!(
        (
            model_file(DET_MODEL, DET_FILE),
            model_file(REC_MODEL, REC_FILE),
            model_file(REC_MODEL, DICT_FILE),
        ),
        (Ok(a), Ok(b), Ok(c)) if a.is_file() && b.is_file() && c.is_file()
    )
}

// ---------- 纯文本处理（无 ort；单测全覆盖） ----------

/// bilinear resize（对齐 cv2.INTER_LINEAR：src = (dst+0.5)*scale-0.5，边缘 clamp）。
/// RGB 交错 u8 进出；w/h 任一为 0 响亮报错。
pub fn resize_bilinear_rgb(
    pixels: &[u8],
    w: u32,
    h: u32,
    tw: u32,
    th: u32,
) -> Result<Vec<u8>> {
    let (w, h, tw, th) = (w as usize, h as usize, tw as usize, th as usize);
    if w == 0 || h == 0 || tw == 0 || th == 0 {
        return Err(OcrError::InputMismatch {
            step: "resize",
            msg: format!("非法尺寸 {w}x{h} → {tw}x{th}"),
        });
    }
    if pixels.len() != w * h * 3 {
        return Err(OcrError::InputMismatch {
            step: "resize",
            msg: format!("RGB 缓冲 {w}x{h} 期望 {} 字节，实际 {}", w * h * 3, pixels.len()),
        });
    }
    if tw == w && th == h {
        return Ok(pixels.to_vec());
    }
    let sx = w as f64 / tw as f64;
    let sy = h as f64 / th as f64;
    let mut out = vec![0u8; tw * th * 3];
    for dy in 0..th {
        let fy = ((dy as f64 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy.floor() as usize).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        let ly = (fy - y0 as f64) as f32;
        for dx in 0..tw {
            let fx = ((dx as f64 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx.floor() as usize).min(w - 1);
            let x1 = (x0 + 1).min(w - 1);
            let lx = (fx - x0 as f64) as f32;
            for c in 0..3 {
                let p00 = pixels[(y0 * w + x0) * 3 + c] as f32;
                let p01 = pixels[(y0 * w + x1) * 3 + c] as f32;
                let p10 = pixels[(y1 * w + x0) * 3 + c] as f32;
                let p11 = pixels[(y1 * w + x1) * 3 + c] as f32;
                let top = p00 + (p01 - p00) * lx;
                let bot = p10 + (p11 - p10) * lx;
                out[(dy * tw + dx) * 3 + c] = (top + (bot - top) * ly).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(out)
}

/// det 尺寸决策（官方 resize_image_type0，limit_type=min）：min 边 < 736 → 等比放大；
/// 否则 ratio=1；边长 round 到 32 倍数（≥32）；超 4000 上限再压。
/// 返回 (rw, rh, ratio_w, ratio_h)。
pub fn det_resize_plan(w: u32, h: u32) -> (u32, u32, f64, f64) {
    let (wf, hf) = (w as f64, h as f64);
    let mut ratio = if wf.min(hf) < DET_LIMIT_SIDE as f64 {
        if hf < wf {
            DET_LIMIT_SIDE as f64 / hf
        } else {
            DET_LIMIT_SIDE as f64 / wf
        }
    } else {
        1.0
    };
    let mut rw = (wf * ratio) as u32;
    let mut rh = (hf * ratio) as u32;
    if rw.max(rh) > DET_MAX_SIDE {
        ratio = DET_MAX_SIDE as f64 / rw.max(rh) as f64;
        rw = (rw as f64 * ratio) as u32;
        rh = (rh as f64 * ratio) as u32;
    }
    let round32 = |v: u32| ((v as f64 / 32.0).round() as u32 * 32).max(32);
    let (rw, rh) = (round32(rw), round32(rh));
    (rw, rh, rw as f64 / wf, rh as f64 / hf)
}

/// det 归一化：RGB → **BGR** CHW，/255，ImageNet mean/std（官方 NormalizeImage：
/// mean[0] 套通道 0（B），历史行为原样复刻）。
pub fn det_normalize(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<f32>> {
    let n = (w as usize) * (h as usize);
    if rgb.len() != n * 3 {
        return Err(OcrError::InputMismatch {
            step: "det_normalize",
            msg: format!("期望 {} 字节，实际 {}", n * 3, rgb.len()),
        });
    }
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406]; // 官方列表序（B 通道位次见模块注释）
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let mut out = vec![0f32; 3 * n];
    for i in 0..n {
        let (r, g, b) = (rgb[i * 3] as f32 / 255.0, rgb[i * 3 + 1] as f32 / 255.0, rgb[i * 3 + 2] as f32 / 255.0);
        out[i] = (b - MEAN[0]) / STD[0];
        out[n + i] = (g - MEAN[1]) / STD[1];
        out[2 * n + i] = (r - MEAN[2]) / STD[2];
    }
    Ok(out)
}

/// 检出行（源图坐标，含 score）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TextLine {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
    /// box 平均概率（box_score_fast 口径：bbox 区域均值）
    pub box_score: f32,
    /// 识别文本（空 = rec 未跑或无字符）
    pub text: String,
    /// CTC 平均字符置信度
    pub text_score: f32,
}

/// DB 后处理（轴对齐简化，见模块注释）：map > thresh 二值 → 8-连通域 → bbox →
/// 域内均值分 → box_thresh 过滤 → unclip 外扩（d = area*ratio/perim，轴对齐展开）→
/// min_size 过滤 → 映射回源图坐标（clamp）。
pub fn db_boxes(
    map: &[f32],
    mw: u32,
    mh: u32,
    src_w: u32,
    src_h: u32,
    box_limit: usize,
) -> Result<Vec<(i64, i64, i64, i64, f32)>> {
    let (mw, mh) = (mw as usize, mh as usize);
    if map.len() != mw * mh {
        return Err(OcrError::InputMismatch {
            step: "db_boxes",
            msg: format!("map {} 元素 ≠ {}x{}", map.len(), mw, mh),
        });
    }
    let bin = |i: usize| map[i] > DB_THRESH;
    // 8-连通域标记（迭代 flood fill，raster 种子序 = 确定性）
    let mut comp = vec![0u32; mw * mh];
    let mut n_comp = 0u32;
    let mut stack: Vec<usize> = Vec::new();
    for seed in 0..mw * mh {
        if !bin(seed) || comp[seed] != 0 {
            continue;
        }
        n_comp += 1;
        stack.clear();
        stack.push(seed);
        comp[seed] = n_comp;
        while let Some(cur) = stack.pop() {
            let cy = cur / mw;
            let cx = cur % mw;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = cx as i64 + dx;
                    let ny = cy as i64 + dy;
                    if nx < 0 || ny < 0 || nx >= mw as i64 || ny >= mh as i64 {
                        continue;
                    }
                    let ni = ny as usize * mw + nx as usize;
                    if bin(ni) && comp[ni] == 0 {
                        comp[ni] = n_comp;
                        stack.push(ni);
                    }
                }
            }
        }
    }
    // 每域 bbox + 概率和（单遍）
    let mut bboxes = vec![(usize::MAX, usize::MAX, 0usize, 0usize, 0f32, 0u32); n_comp as usize];
    for i in 0..mw * mh {
        let c = comp[i];
        if c == 0 {
            continue;
        }
        let e = &mut bboxes[c as usize - 1];
        let (x, y) = (i % mw, i / mw);
        e.0 = e.0.min(x);
        e.1 = e.1.min(y);
        e.2 = e.2.max(x);
        e.3 = e.3.max(y);
        e.4 += map[i];
        e.5 += 1;
    }
    let width_scale = src_w as f64 / mw as f64;
    let height_scale = src_h as f64 / mh as f64;
    let mut boxes = Vec::new();
    for e in bboxes {
        if e.5 == 0 {
            continue;
        }
        if boxes.len() >= box_limit {
            break;
        }
        let (x0, y0, x1, y1, psum, cnt) = e;
        let bw = (x1 - x0 + 1) as f32;
        let bh = (y1 - y0 + 1) as f32;
        if bw.min(bh) < DB_MIN_SIZE {
            continue;
        }
        let score = psum / cnt as f32;
        if score < DB_BOX_THRESH {
            continue;
        }
        // unclip（轴对齐）：d = area*ratio/perim，四边外扩；外扩后 min 边 < min_size+2 丢弃
        let area = bw * bh;
        let perim = 2.0 * (bw + bh);
        let d = area * DB_UNCLIP_RATIO / perim;
        let ex0 = (x0 as f32 - d).max(0.0);
        let ey0 = (y0 as f32 - d).max(0.0);
        let ex1 = (x1 as f32 + 1.0 - 1.0 + d).min(mw as f32 - 1.0);
        let ey1 = (y1 as f32 + d).min(mh as f32 - 1.0);
        if (ex1 - ex0).min(ey1 - ey0) < DB_MIN_SIZE + 2.0 {
            continue;
        }
        let sx0 = (ex0 as f64 * width_scale).round().clamp(0.0, src_w as f64) as i64;
        let sy0 = (ey0 as f64 * height_scale).round().clamp(0.0, src_h as f64) as i64;
        let sx1 = (ex1 as f64 * width_scale).round().clamp(0.0, src_w as f64) as i64;
        let sy1 = (ey1 as f64 * height_scale).round().clamp(0.0, src_h as f64) as i64;
        boxes.push((sx0, sy0, sx1, sy1, score));
    }
    // 阅读序：按 y0（行）再 x0（列），确定性
    boxes.sort_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)));
    Ok(boxes)
}

/// rec 预处理（官方 OCRReisizeNormImg 口径）：源图裁剪 [x0..x1, y0..y1] → 高 48 等比
/// 双线性缩 → /255 偏 0.5 除 0.5（CHW）→ 右 Padding 0.0 到 imgW =
/// min(int(48 * max(320/48, w/h)), 3200)。返回 (data, imgW, resized_w)。
pub fn rec_prepare(w: u32, h: u32, rgb: &[u8], x0: i64, y0: i64, x1: i64, y1: i64) -> Result<(Vec<f32>, u32, u32)> {
    let (w, h) = (w as i64, h as i64);
    if rgb.len() != (w * h * 3) as usize {
        return Err(OcrError::InputMismatch {
            step: "rec_prepare",
            msg: format!("RGB 缓冲 {}x{h} 期望 {} 字节，实际 {}", w, (w * h * 3), rgb.len()),
        });
    }
    if x1 < 0 || y1 < 0 || x0 >= w || y0 >= h {
        return Err(OcrError::InputMismatch {
            step: "rec_prepare",
            msg: format!("裁剪窗 [{x0},{y0},{x1},{y1}] 完全在图外 {}x{}", w, h),
        });
    }
    let (cx0, cy0) = (x0.clamp(0, w - 1), y0.clamp(0, h - 1));
    let (cx1, cy1) = (x1.clamp(cx0, w - 1), y1.clamp(cy0, h - 1));
    let cw = (cx1 - cx0 + 1) as u32;
    let ch = (cy1 - cy0 + 1) as u32;
    if cw == 0 || ch == 0 {
        return Err(OcrError::InputMismatch {
            step: "rec_prepare",
            msg: format!("空裁剪窗 [{cx0},{cy0},{cx1},{cy1}]"),
        });
    }
    // 裁剪（逐行拷贝，无整图复制）
    let mut crop = vec![0u8; (cw * ch * 3) as usize];
    for r in 0..ch as i64 {
        let src = ((cy0 + r) * w + cx0) as usize * 3;
        let dst = (r * cw as i64) as usize * 3;
        crop[dst..dst + (cw as usize) * 3].copy_from_slice(&rgb[src..src + (cw as usize) * 3]);
    }
    let ratio = cw as f64 / ch as f64;
    let img_w = ((REC_HEIGHT as f64 * ratio.max(REC_BASE_WH_RATIO)) as u32).min(REC_MAX_WIDTH).max(REC_HEIGHT);
    let resized_w = ((REC_HEIGHT as f64 * ratio).ceil() as u32)
        .clamp(32, img_w)
        .min(REC_MAX_WIDTH);
    let resized = resize_bilinear_rgb(&crop, cw, ch, resized_w, REC_HEIGHT)?;
    // 归一化 CHW + 右 Padding 0.0
    let n = (REC_HEIGHT as usize) * (img_w as usize);
    let mut out = vec![0f32; 3 * n];
    for y in 0..REC_HEIGHT as usize {
        for x in 0..resized_w as usize {
            let si = (y * resized_w as usize + x) * 3;
            let di = y * img_w as usize + x;
            out[di] = resized[si] as f32 / 255.0 / 0.5 - 1.0;
            out[n + di] = resized[si + 1] as f32 / 255.0 / 0.5 - 1.0;
            out[2 * n + di] = resized[si + 2] as f32 / 255.0 / 0.5 - 1.0;
        }
    }
    Ok((out, img_w, resized_w))
}

/// CTCLabelDecode greedy：逐帧 argmax → 折叠重复 → 去 blank(0)；space = 末位类别；
/// 其余 idx-1 查字典。score = 保留字符概率均值（官方 decode 口径）。
/// 无有效字符 → (String::new(), 0.0)。
pub fn ctc_decode(probs: &[f32], steps: usize, n_classes: usize, dict: &[String]) -> (String, f32) {
    if steps == 0 || n_classes == 0 || probs.len() < steps * n_classes {
        return (String::new(), 0.0);
    }
    let space_idx = dict.len() + 1; // ['blank'] + dict + [' ']
    let mut text = String::new();
    let mut score_sum = 0f32;
    let mut kept = 0usize;
    let mut prev = usize::MAX;
    for t in 0..steps {
        let row = &probs[t * n_classes..(t + 1) * n_classes];
        let (mut idx, mut best) = (0usize, f32::NEG_INFINITY);
        for (i, &p) in row.iter().enumerate() {
            if p > best {
                best = p;
                idx = i;
            }
        }
        if idx != 0 && idx != prev {
            if idx == space_idx {
                text.push(' ');
            } else if let Some(c) = dict.get(idx - 1) {
                text.push_str(c);
            }
            score_sum += best;
            kept += 1;
        }
        prev = idx;
    }
    let score = if kept == 0 { 0.0 } else { score_sum / kept as f32 };
    (text.trim().to_string(), score)
}

/// 从文件加载字典（每行一个字符；UTF-8；末行换行容忍）。
pub fn load_dict(path: &std::path::Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| OcrError::ModelMissing(format!("{}: {e}", path.display())))?;
    Ok(raw
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h * 3) as usize]
    }

    #[test]
    fn models_root_contract_matches_layout() {
        let dir = std::env::temp_dir().join(format!("dam-ocr-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::remove_var("DAM_MODELS_ROOT");
        set_models_root(&dir);
        assert_eq!(
            model_file(DET_MODEL, DET_FILE).unwrap(),
            dir.join("models").join(DET_MODEL).join(DET_FILE)
        );
        assert!(!models_ready());
        for (m, f) in [
            (DET_MODEL, DET_FILE),
            (REC_MODEL, REC_FILE),
            (REC_MODEL, DICT_FILE),
        ] {
            let p = model_file(m, f).unwrap();
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"stub").unwrap();
        }
        assert!(models_ready());
        clear_models_root();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bilinear_matches_cv2_semantics() {
        // 1x1 → 3x3：全部 clamp 到同色
        let px = vec![10u8, 20, 30];
        let out = resize_bilinear_rgb(&px, 1, 1, 3, 3).unwrap();
        assert_eq!(out, vec![10u8, 20, 30].repeat(9));
        // 2x2 灰阶放大 2x：角点保持，中心为均值
        let mut px = vec![0u8; 2 * 2 * 3];
        for (i, v) in [0u8, 100].iter().enumerate() {
            for c in 0..3 {
                px[(i * 2) * 3 + c] = *v;
                px[(i * 2 + 1) * 3 + c] = *v;
            }
        }
        let out = resize_bilinear_rgb(&px, 2, 2, 4, 4).unwrap();
        // 左列 0，右列 100，中间双线性
        assert_eq!(out[0], 0);
        assert_eq!(out[((3 * 4) + 3) * 3], 100, "右下角像素应保持 100");
        let mid = out[(1 * 4 + 1) * 3] as f32;
        assert!((25.0..=40.0).contains(&mid), "mid={mid}");
        // 缓冲长度不符 → 响亮
        assert!(resize_bilinear_rgb(&[0u8; 3], 2, 2, 4, 4).is_err());
    }

    #[test]
    fn det_resize_plan_official_semantics() {
        // 小图（min 边 < 736）等比放大到 min 边 ≈736，round 到 32 倍数
        let (rw, rh, rw_ratio, _rh) = det_resize_plan(100, 50);
        assert_eq!(rh, 736, "min 边 50 → 放大到 ~736");
        assert_eq!(rw, 1472, "等比 ×14.72 → 1472");
        assert!((rw_ratio - 14.72).abs() < 1e-6);
        // 大图（min 边 ≥ 736）不缩放，round 到 32 倍数
        let (rw, rh, rwr, rhr) = det_resize_plan(1920, 1080);
        assert_eq!((rw, rh), (1920, 1088)); // 1080 → round(33.75)=34 → 1088
        assert!((rwr - 1.0).abs() < 1e-9);
        assert!((rhr - 1088.0 / 1080.0).abs() < 1e-9);
        // 极小图钳 32
        let (rw, rh, _, _) = det_resize_plan(10, 10);
        assert_eq!((rw, rh), (736, 736));
        // 超大图压 4000 上限
        let (rw, rh, _, _) = det_resize_plan(9000, 3000);
        assert!(rw <= 4000 && rh <= 4000);
    }

    #[test]
    fn det_normalize_is_bgr_imagenet() {
        let rgb = vec![255u8, 0, 0]; // 纯红
        let out = det_normalize(1, 1, &rgb).unwrap();
        // 通道 0 = B（像素 0）套 mean[0]=0.485 —— 官方「列表序套通道位次」历史口径
        assert!((out[0] - (0.0 - 0.485) / 0.229).abs() < 1e-6);
        // 通道 1 = G（0）套 mean[1]
        assert!((out[1] - (0.0 - 0.456) / 0.224).abs() < 1e-6);
        // 通道 2 = R（1.0）套 mean[2]=0.406
        assert!((out[2] - (1.0 - 0.406) / 0.225).abs() < 1e-6);
        assert!(det_normalize(1, 1, &[0u8; 6]).is_err());
    }

    #[test]
    fn db_boxes_finds_two_blobs_in_reading_order() {
        // 100x50 map，两块亮区：左上 (10..30, 5..15)，右下 (60..90, 30..45)
        let (mw, mh) = (100u32, 50u32);
        let mut map = vec![0f32; (mw * mh) as usize];
        for y in 5..15 {
            for x in 10..30 {
                map[y * mw as usize + x] = 0.9;
            }
        }
        for y in 30..45 {
            for x in 60..90 {
                map[y * mw as usize + x] = 0.8;
            }
        }
        let boxes = db_boxes(&map, mw, mh, 1000, 500, DB_MAX_CANDIDATES).unwrap();
        assert_eq!(boxes.len(), 2, "{boxes:?}");
        // 阅读序：y0 小者在前
        assert!(boxes[0].1 <= boxes[1].1);
        // 坐标映射到源图（×10/×10）+ unclip 外扩后仍覆盖原块
        assert!(boxes[0].0 <= 100 && boxes[0].2 >= 290);
        assert!(boxes[1].0 <= 600 && boxes[1].2 >= 900);
        // 得分 = 域内均值
        assert!((boxes[0].4 - 0.9).abs() < 1e-6);
        // 弱块被 box_thresh 过滤
        let mut map2 = map.clone();
        for y in 0..50 {
            for x in 0..100 {
                map2[y * 100 + x] = 0.0;
            }
        }
        for y in 5..15 {
            for x in 10..30 {
                map2[y * 100 + x] = 0.5; // 域均 0.5 < 0.45? 不过滤；改 0.44 域均
            }
        }
        // 单独一块 0.44 均值 → 低于 box_thresh 0.45 → 无框
        let mut weak = vec![0f32; 100 * 50];
        for y in 5..15 {
            for x in 10..30 {
                weak[y * 100 + x] = 0.44;
            }
        }
        assert!(db_boxes(&weak, 100, 50, 1000, 500, DB_MAX_CANDIDATES).unwrap().is_empty());
        // box_limit 截断
        let limited = db_boxes(&map, mw, mh, 1000, 500, 1).unwrap();
        assert_eq!(limited.len(), 1);
        let _ = map2;
    }

    #[test]
    fn db_boxes_min_size_filters_tiny_specks() {
        let mut map = vec![0f32; 100 * 50];
        map[10 * 100 + 10] = 0.99; // 单像素亮点
        map[10 * 100 + 11] = 0.99;
        map[11 * 100 + 10] = 0.99;
        map[11 * 100 + 11] = 0.99; // 2x2 → sside 2 < 3 丢弃
        assert!(db_boxes(&map, 100, 50, 1000, 500, DB_MAX_CANDIDATES).unwrap().is_empty());
    }

    #[test]
    fn rec_prepare_pads_right_and_normalizes() {
        // 16x8 纯白图：ratio 2.0 → imgW = int(48*max(6.667,2)) = 320，resized_w = ceil(48*2)=96
        let (data, img_w, resized_w) = rec_prepare(16, 8, &gray(16, 8, 255), 0, 0, 15, 7).unwrap();
        assert_eq!(img_w, 320);
        assert_eq!(resized_w, 96);
        assert_eq!(data.len(), 3 * 48 * 320);
        // 白像素归一 = 1.0
        assert!((data[0] - 1.0).abs() < 1e-6);
        // padding 区（x ≥ 96）= 0.0
        assert_eq!(data[100], 0.0);
        assert_eq!(data[3 * 48 * 320 - 1], 0.0);
        // 宽行（ratio > base）：imgW 随行宽，超 3200 钳
        let (data, img_w, _) = rec_prepare(800, 8, &gray(800, 8, 255), 0, 0, 799, 7).unwrap();
        assert_eq!(img_w, 3200);
        assert_eq!(data.len(), 3 * 48 * 3200);
        // 空窗响亮
        assert!(rec_prepare(16, 8, &gray(16, 8, 255), 20, 0, 30, 7).is_err());
    }

    #[test]
    fn ctc_decode_collapses_and_skips_blank() {
        let dict: Vec<String> = vec!["A".into(), "B".into()];
        // 类别：0=blank, 1=A, 2=B, 3=space
        let n = 4usize;
        let mut probs = vec![0f32; 5 * n];
        let set = |probs: &mut Vec<f32>, t: usize, idx: usize| {
            for j in 0..n {
                probs[t * n + j] = if j == idx { 0.9 } else { 0.1 / 3.0 };
            }
        };
        set(&mut probs, 0, 0); // blank
        set(&mut probs, 1, 1); // A
        set(&mut probs, 2, 1); // A（折叠）
        set(&mut probs, 3, 0); // blank
        set(&mut probs, 4, 3); // space
        let (text, score) = ctc_decode(&probs, 5, n, &dict);
        assert_eq!(text, "A");
        assert!((score - 0.9).abs() < 1e-6, "{score}");
        // 全 blank → 空文本 0 分
        let mut blank_only = vec![0f32; 3 * n];
        for t in 0..3 {
            set(&mut blank_only, t, 0);
        }
        assert_eq!(ctc_decode(&blank_only, 3, n, &dict), (String::new(), 0.0));
    }

    #[test]
    fn dict_loader_reports_missing_loudly() {
        let err = load_dict(std::path::Path::new("/no/such/dict.txt")).unwrap_err();
        assert!(matches!(err, OcrError::ModelMissing(_)));
        let dir = std::env::temp_dir().join(format!("dam-ocr-dict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.txt");
        std::fs::write(&p, "A\nB\n").unwrap();
        assert_eq!(load_dict(&p).unwrap(), vec!["A".to_string(), "B".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
