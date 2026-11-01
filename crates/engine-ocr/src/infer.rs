//! ONNX 推理（local-infer 门控）：det 会话 + rec 会话 + 编排。
//!
//! 会话缓存与 engine-segment 同纪律：按文件路径缓存 `Arc<OcrEngine>`，批处理只加载
//! 一次；intra-op 线程 min(CPU, 8) 防队列并发超订。输入/输出名从图里读，不硬编码。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ort::session::Session;
use ort::value::Tensor;

use crate::{
    db_boxes, load_dict, rec_prepare, det_normalize, det_resize_plan, models_ready,
    resize_bilinear_rgb, model_file, DET_FILE, DET_MODEL, DICT_FILE, REC_FILE, REC_HEIGHT,
    REC_MODEL, Result, TextLine, OcrError,
};

/// det 会话（动态输入 [1,3,H,W]，H/W 为 32 倍数）。
pub struct DetSession {
    session: Mutex<Session>,
    in_name: String,
    out_name: String,
}

impl DetSession {
    pub fn load(onnx: &Path) -> Result<Self> {
        let (session, in_name, out_name) = open_session(onnx, "det")?;
        Ok(Self {
            session: Mutex::new(session),
            in_name,
            out_name,
        })
    }

    /// RGB 源图 → DB 概率图 (map, mw, mh)。内部：resize → BGR 归一 → 推理。
    pub fn run(&self, w: u32, h: u32, rgb: &[u8]) -> Result<(Vec<f32>, u32, u32)> {
        let (rw, rh, _, _) = det_resize_plan(w, h);
        let resized = resize_bilinear_rgb(rgb, w, h, rw, rh)?;
        let input = det_normalize(rw, rh, &resized)?;
        let t = Tensor::from_array((vec![1usize, 3, rh as usize, rw as usize], input.into_boxed_slice()))
            .map_err(|e| OcrError::Onnx(format!("det input: {e}")))?;
        let mut session = self.session.lock().expect("det session lock");
        let outputs = session
            .run(ort::inputs![self.in_name.as_str() => t])
            .map_err(|e| OcrError::Onnx(format!("det run: {e}")))?;
        let value = outputs
            .get(self.out_name.as_str())
            .ok_or_else(|| OcrError::Onnx(format!("det output {} missing", self.out_name)))?;
        let arr = value
            .try_extract_array::<f32>()
            .map_err(|e| OcrError::Onnx(format!("det extract: {e}")))?;
        let shape = arr.shape().to_vec();
        if shape.len() != 4 {
            return Err(OcrError::Onnx(format!("det 输出维度异常: {shape:?}")));
        }
        let (_, _, mh, mw) = (shape[0], shape[1], shape[2], shape[3]);
        let map: Vec<f32> = arr.iter().copied().collect();
        Ok((map, mw as u32, mh as u32))
    }
}

/// rec 会话（动态输入 [n,3,48,W]；字典随会话持有）。
pub struct RecSession {
    session: Mutex<Session>,
    in_name: String,
    out_name: String,
    dict: Vec<String>,
    n_classes: usize,
}

impl RecSession {
    pub fn load(onnx: &Path, dict_path: &Path) -> Result<Self> {
        let (session, in_name, out_name) = open_session(onnx, "rec")?;
        let dict = load_dict(dict_path)?;
        if dict.is_empty() {
            return Err(OcrError::Failed {
                step: "rec",
                msg: format!("字典为空: {}", dict_path.display()),
            });
        }
        let n_classes = dict.len() + 2; // blank + dict + space
        Ok(Self {
            session: Mutex::new(session),
            in_name,
            out_name,
            dict,
            n_classes,
        })
    }

    /// 裁剪窗 → (text, score)。输入为源图全图缓冲 + 窗口（窗内裁剪在预处理内完成）。
    pub fn run(&self, w: u32, h: u32, rgb: &[u8], x0: i64, y0: i64, x1: i64, y1: i64) -> Result<(String, f32)> {
        let (data, img_w, _resized_w) = rec_prepare(w, h, rgb, x0, y0, x1, y1)?;
        let t = Tensor::from_array((
            vec![1usize, 3, REC_HEIGHT as usize, img_w as usize],
            data.into_boxed_slice(),
        ))
        .map_err(|e| OcrError::Onnx(format!("rec input: {e}")))?;
        let mut session = self.session.lock().expect("rec session lock");
        let outputs = session
            .run(ort::inputs![self.in_name.as_str() => t])
            .map_err(|e| OcrError::Onnx(format!("rec run: {e}")))?;
        let value = outputs
            .get(self.out_name.as_str())
            .ok_or_else(|| OcrError::Onnx(format!("rec output {} missing", self.out_name)))?;
        let arr = value
            .try_extract_array::<f32>()
            .map_err(|e| OcrError::Onnx(format!("rec extract: {e}")))?;
        let shape = arr.shape().to_vec();
        if shape.len() != 3 {
            return Err(OcrError::Onnx(format!("rec 输出维度异常: {shape:?}")));
        }
        let steps = shape[1];
        if shape[2] != self.n_classes {
            return Err(OcrError::Onnx(format!(
                "rec 类别数 {} ≠ 字典推得 {}（字典/模型不匹配）",
                shape[2], self.n_classes
            )));
        }
        let probs: Vec<f32> = arr.iter().copied().collect();
        Ok(crate::ctc_decode(&probs, steps, self.n_classes, &self.dict))
    }
}

/// 完整 OCR 引擎：det → boxes → rec（每框一次）→ 行文本（阅读序）。
pub struct OcrEngine {
    pub det: DetSession,
    pub rec: RecSession,
}

impl OcrEngine {
    /// 从模型根加载（模型根解析与缺文件报错语义同 engine-segment，响亮不猜路径）。
    pub fn load() -> Result<Self> {
        let det = DetSession::load(&model_file(DET_MODEL, DET_FILE)?)?;
        let rec = RecSession::load(
            &model_file(REC_MODEL, REC_FILE)?,
            &model_file(REC_MODEL, DICT_FILE)?,
        )?;
        Ok(Self { det, rec })
    }

    /// RGB 交错源图 → 行文本（阅读序；每行含框/box 分/CTC 分）。
    pub fn run_rgb(&self, w: u32, h: u32, rgb: &[u8]) -> Result<Vec<TextLine>> {
        if rgb.len() != (w as usize) * (h as usize) * 3 {
            return Err(OcrError::InputMismatch {
                step: "ocr",
                msg: format!("RGB 缓冲 {w}x{h} 期望 {} 字节，实际 {}", (w as usize) * (h as usize) * 3, rgb.len()),
            });
        }
        let (map, mw, mh) = self.det.run(w, h, rgb)?;
        let boxes = db_boxes(&map, mw, mh, w, h, crate::DB_MAX_CANDIDATES)?;
        let mut lines = Vec::with_capacity(boxes.len());
        for (x0, y0, x1, y1, box_score) in boxes {
            let (text, text_score) = if x1 > x0 && y1 > y0 {
                self.rec.run(w, h, rgb, x0, y0, x1, y1)?
            } else {
                (String::new(), 0.0)
            };
            lines.push(TextLine {
                x0,
                y0,
                x1,
                y1,
                box_score,
                text,
                text_score,
            });
        }
        Ok(lines)
    }
}

fn open_session(onnx: &Path, tag: &'static str) -> Result<(Session, String, String)> {
    if !onnx.is_file() {
        return Err(OcrError::ModelMissing(onnx.display().to_string()));
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let session = Session::builder()
        .and_then(|b| b.with_intra_threads(threads))
        .and_then(|b| b.commit_from_file(onnx))
        .map_err(|e| OcrError::Onnx(format!("{}: {e}", onnx.display())))?;
    let in_name = session
        .inputs
        .first()
        .map(|i| i.name.clone())
        .ok_or_else(|| OcrError::Onnx(format!("{tag} has no inputs")))?;
    let out_name = session
        .outputs
        .first()
        .map(|o| o.name.clone())
        .ok_or_else(|| OcrError::Onnx(format!("{tag} has no outputs")))?;
    Ok((session, in_name, out_name))
}

// ---------- 会话缓存（批处理 138MB 权重只加载一次） ----------

static ENGINES: Mutex<Option<HashMap<(PathBuf, PathBuf, PathBuf), Arc<OcrEngine>>>> =
    Mutex::new(None);

/// 按当前模型根取缓存引擎（首次调用加载；三件路径为缓存键）。
pub fn engine() -> Result<Arc<OcrEngine>> {
    let det = model_file(DET_MODEL, DET_FILE)?;
    let rec = model_file(REC_MODEL, REC_FILE)?;
    let dict = model_file(REC_MODEL, DICT_FILE)?;
    let key = (det, rec, dict);
    let mut guard = ENGINES.lock().expect("ocr engine cache lock");
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(e) = cache.get(&key) {
        return Ok(e.clone());
    }
    let e = Arc::new(OcrEngine::load()?);
    cache.insert(key, e.clone());
    Ok(e)
}

/// 快路径预检（供编排器做 skipped(model_not_downloaded)）。
pub fn ready() -> bool {
    models_ready()
}

/// 无 local-infer 构建时（web 容器）：响亮报错，OCR 走 L2 降级（skipped）。
#[cfg(not(feature = "local-infer"))]
pub fn engine() -> Result<std::convert::Infallible> {
    Err(OcrError::Failed {
        step: "engine-ocr",
        msg: "构建未启用 local-infer（web 容器无 ONNX 运行时），OCR 不可用".into(),
    })
}
