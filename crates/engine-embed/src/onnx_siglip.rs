//! `OnnxSiglipProvider` — 默认内嵌 embedding provider（PLAN.md §9）。
//!
//! ort + SigLIP 2（siglip2-base-patch16-224，双塔 ONNX）：
//! - vision 塔：`pixel_values` [1,3,224,224]（resize 224×224 + rescale 1/255 + mean/std 0.5 归一）
//! - text 塔：`input_ids` [1,64]（Gemma tokenizer，256k 词表，原生中文；截断至 64）
//! - 出口统一 L2 归一 → dim 768（registry spec 锁定；改 preprocess 必须全量重嵌入）
//!
//! 模型文件不进安装包：首启/首次使用时由 [`crate::model_store`] 静默下载到
//! `{appData}/models/<spec.name>/`（vision.onnx + text.onnx + tokenizer.json）。
//! 本 provider 只负责「给目录就能推理」，下载/校验/进度归 model_store。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use core_config::ModelSpec;
use ort::session::Session;
use ort::value::{Tensor, ValueType};
use tokenizers::{Tokenizer, TruncationParams};

use crate::model_store::ModelManifest;
use crate::{l2_normalize, EmbedError, EmbedProvider, Result};

/// 图像预处理参数（从 registry spec.preprocess 解析；变更 = 重嵌入）。
#[derive(Debug, Clone, PartialEq)]
pub struct ImagePreprocess {
    pub image_size: u32,
    pub mean: [f32; 3],
    pub std: [f32; 3],
}

impl Default for ImagePreprocess {
    fn default() -> Self {
        Self {
            image_size: 224,
            mean: [0.5; 3],
            std: [0.5; 3],
        }
    }
}

impl ImagePreprocess {
    /// 从 ModelSpec.preprocess 读取（缺字段回落 SigLIP 默认值）。
    pub fn from_spec(spec: &ModelSpec) -> Self {
        let mut p = Self::default();
        if let Some(size) = spec.preprocess.get("image_size").and_then(|v| v.as_u64()) {
            p.image_size = size as u32;
        }
        let read3 = |key: &str| -> Option<[f32; 3]> {
            let seq = spec.preprocess.get(key)?.as_sequence()?;
            if seq.len() != 3 {
                return None;
            }
            let mut out = [0f32; 3];
            for (i, v) in seq.iter().enumerate() {
                out[i] = v.as_f64()? as f32;
            }
            Some(out)
        };
        if let Some(m) = read3("mean") {
            p.mean = m;
        }
        if let Some(s) = read3("std") {
            p.std = s;
        }
        p
    }

    /// 解码 → resize(image_size²) → RGB → CHW → rescale(1/255) → (x-mean)/std。
    /// 输出长度 = 3 * image_size²（batch 维由调用方补）。
    pub fn preprocess(&self, bytes: &[u8]) -> Result<Vec<f32>> {
        let img = image::load_from_memory(bytes).map_err(|e| EmbedError::Image(e.to_string()))?;
        let rgb = img.to_rgb8();
        let size = self.image_size;
        // SigLIP 官方预处理 resample=2（双线性）+ 无 center crop：直接缩放到 size×size
        let resized =
            image::imageops::resize(&rgb, size, size, image::imageops::FilterType::Triangle);
        let hw = (size * size) as usize;
        let mut out = vec![0f32; 3 * hw];
        for (x, y, px) in resized.enumerate_pixels() {
            let idx = (y * size + x) as usize;
            for c in 0..3 {
                let v = px.0[c] as f32 / 255.0;
                out[c * hw + idx] = (v - self.mean[c]) / self.std[c];
            }
        }
        Ok(out)
    }
}

/// 已加载的会话集合（vision/text 两塔 + tokenizer）。
struct Loaded {
    vision: Session,
    text: Session,
    tokenizer: Tokenizer,
    /// 视觉塔输入元素类型是否 f16（fp16 模型直接喂 fp16，避免运行期 Cast）
    vision_in_f16: bool,
    /// 实际取用的输出名（加载期从图里确定，避免运行期字符串猜测）
    vision_out: String,
    text_out: String,
}

/// SigLIP2 内嵌 provider。
pub struct OnnxSiglipProvider {
    spec: ModelSpec,
    dir: PathBuf,
    preprocess: ImagePreprocess,
    text_max_length: usize,
    loaded: Mutex<Option<Loaded>>,
}

impl OnnxSiglipProvider {
    /// dir = `{appData}/models/<spec.name>`（由 model_store 负责把文件放好）。
    /// manifest 提供文本最大长度等模型级常量（名字必须与 spec.name 一致）。
    pub fn new(spec: ModelSpec, dir: impl Into<PathBuf>, manifest: Option<&ModelManifest>) -> Self {
        let text_max_length = manifest.map(|m| m.text_max_length).unwrap_or(64);
        let preprocess = ImagePreprocess::from_spec(&spec);
        Self {
            spec,
            dir: dir.into(),
            preprocess,
            text_max_length,
            loaded: Mutex::new(None),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn preprocess_spec(&self) -> &ImagePreprocess {
        &self.preprocess
    }

    pub fn text_max_length(&self) -> usize {
        self.text_max_length
    }

    /// 三件套路径（vision.onnx / text.onnx / tokenizer.json）。
    pub fn paths(&self) -> (PathBuf, PathBuf, PathBuf) {
        (
            self.dir.join("vision.onnx"),
            self.dir.join("text.onnx"),
            self.dir.join("tokenizer.json"),
        )
    }

    /// 模型文件齐备（不做 sha 校验，快路径）。
    pub fn files_present(&self) -> bool {
        let (v, t, k) = self.paths();
        v.is_file() && t.is_file() && k.is_file()
    }

    /// 单图字节 → 向量（预处理 → 视觉塔 → 归一 → 维度自检）。
    pub fn embed_image_bytes(&self, bytes: &[u8]) -> Result<Vec<f32>> {
        let data = self.preprocess.preprocess(bytes)?;
        let size = self.preprocess.image_size as usize;
        let dims = self.spec.dims;
        self.with_loaded(|l| {
            let v = run_vision(
                &mut l.vision,
                &l.vision_out,
                vec![1, 3, size, size],
                data,
                l.vision_in_f16,
            )?;
            finalize(v, dims)
        })
    }

    /// 文本 → 向量（Gemma tokenizer；截断至 text_max_length）。
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let dims = self.spec.dims;
        let max_len = self.text_max_length;
        self.with_loaded(|l| {
            let ids = tokenize(&l.tokenizer, text, max_len)?;
            let v = run_text(&mut l.text, &l.text_out, &ids)?;
            finalize(v, dims)
        })
    }

    /// 懒加载 + 可变借用会话（Session::run 需要 &mut）。
    fn with_loaded<R>(&self, f: impl FnOnce(&mut Loaded) -> Result<R>) -> Result<R> {
        self.load_if_needed()?;
        let mut guard = self.loaded.lock().unwrap();
        let l = guard
            .as_mut()
            .ok_or_else(|| EmbedError::NotLoaded(self.spec.name.clone()))?;
        f(l)
    }

    fn load_if_needed(&self) -> Result<()> {
        if self.loaded.lock().unwrap().is_some() {
            return Ok(());
        }
        let mut guard = self.loaded.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let (vision_p, text_p, tok_p) = self.paths();
        for p in [&vision_p, &text_p, &tok_p] {
            if !p.is_file() {
                return Err(EmbedError::ModelMissing(p.display().to_string()));
            }
        }
        let vision = Session::builder()
            .and_then(|mut b| b.commit_from_file(&vision_p))
            .map_err(|e| EmbedError::Onnx(format!("{}: {e}", vision_p.display())))?;
        let text = Session::builder()
            .and_then(|mut b| b.commit_from_file(&text_p))
            .map_err(|e| EmbedError::Onnx(format!("{}: {e}", text_p.display())))?;
        let mut tokenizer = Tokenizer::from_file(&tok_p)
            .map_err(|e| EmbedError::Tokenizer(format!("{}: {e}", tok_p.display())))?;
        // 截断在 tokenizer 层配置（长文本按字符边界安全截断）
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: self.text_max_length,
                ..Default::default()
            }))
            .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
        let vision_in_f16 = input_is_f16(&vision, "pixel_values");
        if !input_is_i64(&text, "input_ids") {
            return Err(EmbedError::Onnx(
                "text tower input_ids is not int64（模型契约不符）".to_string(),
            ));
        }
        let vision_out = pick_output(&vision)
            .ok_or_else(|| EmbedError::Onnx("vision tower has no outputs".to_string()))?;
        let text_out = pick_output(&text)
            .ok_or_else(|| EmbedError::Onnx("text tower has no outputs".to_string()))?;
        *guard = Some(Loaded {
            vision,
            text,
            tokenizer,
            vision_in_f16,
            vision_out,
            text_out,
        });
        Ok(())
    }
}

impl EmbedProvider for OnnxSiglipProvider {
    fn load(&mut self) -> Result<()> {
        self.load_if_needed()
    }

    fn embed_images(&self, paths: &[PathBuf]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let bytes = std::fs::read(p)?;
            out.push(self.embed_image_bytes(&bytes)?);
        }
        Ok(out)
    }

    fn embed_texts(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed_text(t)?);
        }
        Ok(out)
    }

    fn ready(&self) -> bool {
        self.loaded.lock().unwrap().is_some()
    }

    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

/// 输入张量元素类型是否 Float16。
fn input_is_f16(session: &Session, name: &str) -> bool {
    session
        .inputs
        .iter()
        .find(|i| i.name == name)
        .map(|i| match &i.input_type {
            ValueType::Tensor { ty, .. } => format!("{ty:?}").contains("Float16"),
            _ => false,
        })
        .unwrap_or(false)
}

/// 输入张量元素类型是否 Int64。
fn input_is_i64(session: &Session, name: &str) -> bool {
    session
        .inputs
        .iter()
        .find(|i| i.name == name)
        .map(|i| match &i.input_type {
            ValueType::Tensor { ty, .. } => format!("{ty:?}").contains("Int64"),
            _ => false,
        })
        .unwrap_or(false)
}

/// 维度自检 + L2 归一（出口统一契约）。
fn finalize(mut v: Vec<f32>, dims: u32) -> Result<Vec<f32>> {
    if v.len() as u32 != dims {
        return Err(EmbedError::Dim {
            expect: dims,
            got: v.len() as u32,
        });
    }
    l2_normalize(&mut v);
    Ok(v)
}

/// 跑一次视觉塔（f32 或 f16 输入张量）。
fn run_vision(
    session: &mut Session,
    out_name: &str,
    shape: Vec<usize>,
    data: Vec<f32>,
    in_f16: bool,
) -> Result<Vec<f32>> {
    if in_f16 {
        let half_data: Vec<half::f16> = data.into_iter().map(half::f16::from_f32).collect();
        let t = Tensor::from_array((shape, half_data.into_boxed_slice()))
            .map_err(|e| EmbedError::Onnx(e.to_string()))?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => t])
            .map_err(|e| EmbedError::Onnx(format!("run pixel_values: {e}")))?;
        let value = outputs
            .get(out_name)
            .ok_or_else(|| EmbedError::Onnx(format!("output {out_name} missing")))?;
        extract_flat(value)
    } else {
        let t = Tensor::from_array((shape, data.into_boxed_slice()))
            .map_err(|e| EmbedError::Onnx(e.to_string()))?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => t])
            .map_err(|e| EmbedError::Onnx(format!("run pixel_values: {e}")))?;
        let value = outputs
            .get(out_name)
            .ok_or_else(|| EmbedError::Onnx(format!("output {out_name} missing")))?;
        extract_flat(value)
    }
}

/// 跑一次文本塔（int64 input_ids；fp16 模型的 Cast 节点在图内）。
fn run_text(session: &mut Session, out_name: &str, ids: &[i64]) -> Result<Vec<f32>> {
    let t = Tensor::from_array((vec![1usize, ids.len()], ids.to_vec().into_boxed_slice()))
        .map_err(|e| EmbedError::Onnx(e.to_string()))?;
    let outputs = session
        .run(ort::inputs!["input_ids" => t])
        .map_err(|e| EmbedError::Onnx(format!("run input_ids: {e}")))?;
    let value = outputs
        .get(out_name)
        .ok_or_else(|| EmbedError::Onnx(format!("output {out_name} missing")))?;
    extract_flat(value)
}

/// 选输出名：优先 pooler_output / image_embeds / text_embeds，退化取第 0 个输出（加载期确定）。
pub(crate) fn pick_output(session: &Session) -> Option<String> {
    let names: Vec<String> = session.outputs.iter().map(|o| o.name.clone()).collect();
    for pref in ["pooler_output", "image_embeds", "text_embeds"] {
        if names.iter().any(|n| n == pref) {
            return Some(pref.to_string());
        }
    }
    names.first().cloned()
}

/// 输出 → 展平 f32（取 dim1 最后一行以兼容 [1,L,dims]）。
fn extract_flat(value: &ort::value::Value) -> Result<Vec<f32>> {
    let arr = value
        .try_extract_array::<f32>()
        .map_err(|e| EmbedError::Onnx(format!("extract output: {e}")))?;
    let shape = arr.shape().to_vec();
    let flat: Vec<f32> = arr.iter().copied().collect();
    match shape.len() {
        0 => Err(EmbedError::Onnx("scalar output".into())),
        1 | 2 => Ok(flat),
        _ => {
            // [1, L, dims] → 取 dim1 最后一行（文本塔 EOS 位置 / 退化路径）
            let dims = *shape.last().unwrap();
            let l = shape[shape.len() - 2];
            if l == 0 || dims == 0 {
                return Err(EmbedError::Onnx(format!("bad output shape {shape:?}")));
            }
            Ok(flat[(l - 1) * dims..l * dims].to_vec())
        }
    }
}

/// Gemma tokenizer 编码 → **左填充**定长序列（[pad…, 内容…, <eos>]）。
///
/// 根因（TEXT-EMBED-1，2026-10-16 定位）：本分发 text.onnx（onnx-community
/// siglip2-base-patch16-224 导出）没有 attention_mask 输入，且 pooler_output
/// 用 Gather(index=-1) 固定取隐状态**最后一位**。tokenizer.json 对短文本做
/// **右填充**（[内容…, <eos>, <pad>… ]），直接送入会把最后一位留给 <pad>，
/// 池化读到 pad 行 → 所有查询向量近乎同一常数（词间 cos 0.74~0.88，跨模态
/// ≈0.02，排序=噪声）。重排为左填充后 <eos> 落在第 63 位（= Gather -1 位），
/// 实测跨模态恢复对齐（匹配词 ≈0.10~0.14，无关词 ≤0.066，见
/// reports/TEXT-EMBED-1.md）。fp16/fp32 同样受影响——与执行精度无关。
pub fn tokenize(tk: &Tokenizer, text: &str, max_len: usize) -> Result<Vec<i64>> {
    // Gemma tokenizer 对空串会整段 pad（BOS+PAD×63），编码结果非空，
    // 因此必须在编码前显式拦截空输入，不能依赖 empty-encoding 检查。
    if text.trim().is_empty() {
        return Err(EmbedError::Tokenizer("empty text input".into()));
    }
    let enc = tk
        .encode(text, true)
        .map_err(|e| EmbedError::Tokenizer(e.to_string()))?;
    let mut ids: Vec<i64> = enc.get_ids().iter().map(|i| *i as i64).collect();
    if ids.is_empty() {
        return Err(EmbedError::Tokenizer("empty encoding".into()));
    }
    ids.truncate(max_len.max(1));
    let pad_id = tk.get_padding().map(|p| p.pad_id as i64).unwrap_or(0);
    let eos_id = tk.token_to_id("<eos>").map(|i| i as i64).unwrap_or(1);
    Ok(left_pad_reorder(&ids, pad_id, eos_id, max_len.max(1)))
}

/// 右填充（[内容…, <eos>, <pad>…]）→ 左填充（[<pad>…, 内容…, <eos>]）。
///
/// - 内容里的 `<pad>` 只可能来自 tokenizer 的填充段（pad id 不参与正文词表），
///   剥离后保持原有相对顺序；
/// - 超长截断可能把尾部 `<eos>` 截掉，此时删掉一个 token 补回 `<eos>`，
///   保证池化位（最后一位）永远是 `<eos>`；
/// - 结果定长 `total`；全 pad 输入（无内容）至少保留一个 `<eos>`。
fn left_pad_reorder(ids: &[i64], pad_id: i64, eos_id: i64, total: usize) -> Vec<i64> {
    let mut content: Vec<i64> = ids.iter().copied().filter(|i| *i != pad_id).collect();
    if content.is_empty() {
        content.push(eos_id);
    }
    if content.len() > total {
        content.truncate(total);
    }
    if *content.last().expect("content 非空") != eos_id {
        if content.len() == total {
            content.pop();
        }
        content.push(eos_id);
    }
    let mut out = vec![pad_id; total - content.len()];
    out.extend_from_slice(&content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_config::{load_registry, SpecKind};

    fn spec() -> ModelSpec {
        load_registry(None)
            .unwrap()
            .by_name(core_config::DEFAULT_EMBEDDER)
            .unwrap()
            .clone()
    }

    #[test]
    fn preprocess_from_registry_locks_siglip_params() {
        let p = ImagePreprocess::from_spec(&spec());
        assert_eq!(p.image_size, 224);
        assert_eq!(p.mean, [0.5, 0.5, 0.5]);
        assert_eq!(p.std, [0.5, 0.5, 0.5]);
        // 空 preprocess → 默认值（SigLIP 常量）
        let raw = ModelSpec {
            name: "x".into(),
            kind: SpecKind::Embedding,
            dims: 3,
            modalities: vec![],
            preprocess: serde_yaml::Value::Null,
            normalized: true,
            provider: "onnx_siglip".into(),
        };
        assert_eq!(ImagePreprocess::from_spec(&raw), ImagePreprocess::default());
    }

    #[test]
    fn preprocess_produces_chw_normalized_tensor() {
        // 造一张 8×4 纯白图 → 缩放到 224×224，白像素归一后 (1.0-0.5)/0.5 = 1.0
        let img = image::RgbImage::from_pixel(8, 4, image::Rgb([255, 255, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let p = ImagePreprocess::default();
        let v = p.preprocess(buf.get_ref()).unwrap();
        assert_eq!(v.len(), 3 * 224 * 224);
        assert!(
            v.iter().all(|x| (x - 1.0).abs() < 1e-3),
            "白像素应归一为 1.0"
        );
        // CHW 布局：三个通道都填满
        let hw = 224 * 224;
        assert!(v[..hw].iter().all(|x| (x - 1.0).abs() < 1e-3));
        assert!(v[2 * hw..].iter().all(|x| (x - 1.0).abs() < 1e-3));
    }

    #[test]
    fn missing_files_report_clearly() {
        let dir = std::env::temp_dir().join("dam-embed-missing-model");
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = OnnxSiglipProvider::new(spec(), &dir, None);
        assert!(!p.files_present());
        let err = p.load().unwrap_err();
        assert!(matches!(err, EmbedError::ModelMissing(_)), "got {err:?}");
        assert!(!p.ready());
    }

    // ---------- TEXT-EMBED-1：文本塔左填充契约 ----------

    /// 最小 Gemma 形状 tokenizer：WordLevel + TemplateProcessing(<eos>) + 定长右填充。
    /// 词表：pad=0, eos=1, dog=5, cat=6；padding Fixed(N) 由 json 里给。
    fn mini_tokenizer(fixed_len: usize) -> Tokenizer {
        use std::str::FromStr;
        let json = format!(
            r#"{{
              "version": "1.0",
              "truncation": {{"direction":"Right","max_length":64,"strategy":"LongestFirst","stride":0}},
              "padding": {{"strategy":{{"Fixed":{fixed_len}}},"direction":"Right","pad_to_multiple_of":null,"pad_id":0,"pad_type_id":0,"pad_token":"<pad>"}},
              "added_tokens": [
                {{"id":0,"content":"<pad>","single_word":false,"lstrip":false,"rstrip":false,"normalized":false,"special":true}},
                {{"id":1,"content":"<eos>","single_word":false,"lstrip":false,"rstrip":false,"normalized":false,"special":true}}
              ],
              "normalizer": null,
              "pre_tokenizer": {{"type":"Whitespace"}},
              "post_processor": {{
                "type":"TemplateProcessing",
                "single":[{{"Sequence":{{"id":"A","type_id":0}}}},{{"SpecialToken":{{"id":"eos","type_id":0}}}}],
                "pair":[],
                "special_tokens":{{"eos":{{"id":"<eos>","ids":[1],"tokens":["<eos>"]}}}}
              }},
              "decoder": null,
              "model": {{"type":"WordLevel","vocab":{{"<pad>":0,"<eos>":1,"dog":5,"cat":6}},"unk_token":"<eos>"}}
            }}"#
        );
        Tokenizer::from_str(&json).expect("mini tokenizer")
    }

    #[test]
    fn left_pad_reorder_moves_eos_to_last() {
        // 右填充 [dog, eos, pad, pad] → 左填充 [pad, pad, dog, eos]
        let out = left_pad_reorder(&[5, 1, 0, 0], 0, 1, 4);
        assert_eq!(out, vec![0, 0, 5, 1]);
    }

    #[test]
    fn left_pad_reorder_reclaims_truncated_eos() {
        // 超长截断把 eos 截掉：内容 4 个占满 total → 删 1 个补回 eos
        let out = left_pad_reorder(&[5, 5, 5, 5], 0, 1, 4);
        assert_eq!(out, vec![5, 5, 5, 1]);
    }

    #[test]
    fn left_pad_reorder_all_pad_input_keeps_eos() {
        // 全 pad（空内容）→ 至少保留一个 eos 在末位
        let out = left_pad_reorder(&[0, 0, 0, 0], 0, 1, 4);
        assert_eq!(out, vec![0, 0, 0, 1]);
    }

    #[test]
    fn left_pad_reorder_no_pad_input_unchanged() {
        // 无填充且已以 eos 结尾 → 原样
        let out = left_pad_reorder(&[5, 6, 1], 0, 1, 3);
        assert_eq!(out, vec![5, 6, 1]);
    }

    #[test]
    fn tokenize_reorders_right_pad_to_left() {
        let tk = mini_tokenizer(8);
        let ids = tokenize(&tk, "dog cat", 8).expect("tokenize");
        // [dog=5, cat=6, eos=1, pad×5] → [pad×5, 5, 6, 1]
        assert_eq!(ids, vec![0, 0, 0, 0, 0, 5, 6, 1]);
        assert_eq!(*ids.last().unwrap(), 1, "末位必须是 <eos>（池化位）");
    }

    #[test]
    fn tokenize_truncates_and_keeps_eos_last() {
        let tk = mini_tokenizer(16);
        let long = std::iter::repeat("dog cat").take(20).collect::<Vec<_>>().join(" ");
        let ids = tokenize(&tk, &long, 16).expect("tokenize");
        assert_eq!(ids.len(), 16);
        assert_eq!(*ids.last().unwrap(), 1, "截断后末位仍要补回 <eos>");
        assert!(ids[..15].iter().all(|i| *i != 1), "eos 只出现在末位");
    }

    #[test]
    fn tokenize_max_len_smaller_than_encoding() {
        let tk = mini_tokenizer(8);
        let ids = tokenize(&tk, "dog cat", 2).expect("tokenize");
        // 内容截到 2 → [5, 1]，total=2 无 pad 位
        assert_eq!(ids, vec![5, 1]);
    }

    #[test]
    fn tokenize_rejects_empty_and_whitespace_only() {
        let tk = mini_tokenizer(8);
        assert!(tokenize(&tk, "", 8).is_err());
        assert!(tokenize(&tk, "   ", 8).is_err());
    }

    #[test]
    fn tokenize_handles_special_chars_via_wordlevel_unk() {
        // 特殊字符/emoji 在 WordLevel 下统一落到 unk（这里 = <eos> id 1），
        // 不得 panic、不得产生空编码；末位仍是 eos（unk 恰为 eos 时压缩去重）。
        let tk = mini_tokenizer(8);
        let ids = tokenize(&tk, "🙂🎉 中国语", 8).expect("tokenize");
        assert_eq!(ids.len(), 8);
        assert_eq!(*ids.last().unwrap(), 1);
    }
}
