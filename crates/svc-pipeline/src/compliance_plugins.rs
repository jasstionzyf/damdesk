//! do 型变换 / 修复插件静态注册表（D12 裁决：进程内静态注册、编译进二进制）。
//!
//! 同一编排器、同一注册表：judge 步骤的 fixer 与 do 步骤的 plugin 都查这里；
//! 未知 id 显式报错（响亮失败铁律，与 engine-classic `DETECTORS` 同纪律）。
//! 加插件 = 实现 `fn(&Path, &Value, &Path, &PluginOrigin) -> Result<PluginOutput>`
//! + `PLUGINS` 加一条。
//!
//! C2 落地 `resize`；C3 落地 `smart_crop`（首个修复器，u2net 掩码裁剪，local-infer
//! 门控——web 容器未注册，修复器降级 fixer_not_available 转人工）。
//! C4 落地其余修复器 `replace_bg`（guaranteed）/`upscale`/`inpaint`（best_effort，
//! local-infer 门控，模型缺失 → ModelMissing → 编排器转人工/skip，不炸 job），
//! 以及 do 型变换五件 `convert`/`compress`/`watermark`/`rename`（classic 档）。
//!
//! 修复/变换产物只写 job outDir（不入库——D12 免费/付费分界沿用）。

use std::path::{Path, PathBuf};

/// 插件来源信息（编排器注入；重命名模板等需要资产级事实）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginOrigin {
    /// 原始资产文件名（去扩展名），如 "IMG_2043"
    pub original_stem: String,
    /// 资产 id（模板变量 {assetId}）
    pub asset_id: i64,
}

/// 插件产物（修复产物 / do 变换产物）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginOutput {
    pub path: PathBuf,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("未知插件: {id}（已知: {known}）")]
    Unknown { id: String, known: String },
    #[error("{plugin}: 参数错误: {msg}")]
    BadParams { plugin: &'static str, msg: String },
    #[error("{plugin}: 模型未下载: {model}（设置页「模型文件（可选下载）」里点下载，或随安装包分发）")]
    ModelMissing { plugin: &'static str, model: String },
    #[error("{plugin}: {msg}")]
    Failed { plugin: &'static str, msg: String },
}

/// 耗时档位（D12 三细节 2：job 队列按档位限并发——C2 仅声明元数据，消费在后续卡）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostTier {
    /// 毫秒级（classic 图像变换）
    Ms,
    /// 秒级内（local_model / 轻量修复）
    Sec,
    /// 分钟级（默认串行 + 进度条）
    Minute,
}

/// 单条插件注册项：(输入文件, YAML 参数, 目标文件, 来源) → 产物。
pub struct PluginEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub cost: CostTier,
    pub run: fn(&Path, &serde_yaml::Value, &Path, &PluginOrigin) -> Result<PluginOutput, PluginError>,
}

fn yaml_bad(plugin: &'static str, e: serde_yaml::Error) -> PluginError {
    PluginError::BadParams { plugin, msg: e.to_string() }
}

fn media_failed(plugin: &'static str, e: core_media::MediaError) -> PluginError {
    PluginError::Failed { plugin, msg: e.to_string() }
}

// ---------- classic 档：do 型变换（毫秒级，无模型依赖，双端可用） ----------

/// 等比缩放（长边 ≤ max_side，只缩不放）。
fn run_resize(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        max_side: i32,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("resize", e))?;
    if p.max_side <= 0 {
        return Err(PluginError::BadParams {
            plugin: "resize",
            msg: format!("max_side 必须 > 0，实际 {}", p.max_side),
        });
    }
    let (w, h) = core_media::resize_to(input, dest, p.max_side).map_err(|e| media_failed("resize", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 格式转换：{format: jpg|jpeg|png|webp, quality?: u8}。产物文件名 = dest 去扩展名
/// + 规范扩展名（jpeg 归一为 jpg）——do 链下一步以返回 path 为准。
fn run_convert(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        format: String,
        quality: Option<u8>,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("convert", e))?;
    let fmt = core_media::OutFormat::from_ext(&p.format)
        .map_err(|e| PluginError::BadParams { plugin: "convert", msg: e.to_string() })?;
    if let Some(q) = p.quality {
        if !(1..=100).contains(&q) {
            return Err(PluginError::BadParams { plugin: "convert", msg: format!("quality 必须 1..=100，实际 {q}") });
        }
    }
    let out = dest.with_extension(fmt.ext());
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("convert", e))?;
    let (w, h) = core_media::encode_rgb_to(w, h, &px, &out, p.quality)
        .map_err(|e| media_failed("convert", e))?;
    Ok(PluginOutput { path: out, width: w, height: h })
}

/// 质量压缩：{quality: 1..=100}，同格式重编码。png/webp 无损或 v1 未支持 → 响亮拒绝
/// （提示先 convert 转 jpeg）。
fn run_compress(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        quality: u8,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("compress", e))?;
    if !(1..=100).contains(&p.quality) {
        return Err(PluginError::BadParams { plugin: "compress", msg: format!("quality 必须 1..=100，实际 {}", p.quality) });
    }
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => {}
        "png" => {
            return Err(PluginError::BadParams {
                plugin: "compress",
                msg: "png 是无损格式，无质量压缩语义；请先 convert 转 jpeg 再压缩".into(),
            })
        }
        "webp" => {
            return Err(PluginError::BadParams {
                plugin: "compress",
                msg: "webp 有损压缩 v1 未支持（纯 Rust 后端仅无损编码）；请先 convert 转 jpeg 再压缩".into(),
            })
        }
        other => {
            return Err(PluginError::BadParams {
                plugin: "compress",
                msg: format!("不支持的压缩源格式 .{other}（支持 jpg/jpeg）"),
            })
        }
    }
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("compress", e))?;
    let (w, h) = core_media::encode_rgb_to(w, h, &px, dest, Some(p.quality))
        .map_err(|e| media_failed("compress", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 文字水印：{text, position?, opacity?, font_size?, margin?, color?}。
/// 内嵌 Noto Sans SC（OFL），中英文同路径；单行文本；字形缺失响亮报错。
fn run_watermark(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        text: String,
        #[serde(default = "default_position")]
        position: String,
        #[serde(default = "default_opacity")]
        opacity: u8,
        #[serde(default = "default_font_size")]
        font_size: f32,
        #[serde(default = "default_margin")]
        margin: i32,
        #[serde(default = "default_color")]
        color: String,
    }
    fn default_position() -> String { "bottom_right".into() }
    fn default_opacity() -> u8 { 50 }
    fn default_font_size() -> f32 { 48.0 }
    fn default_margin() -> i32 { 16 }
    fn default_color() -> String { "ffffff".into() }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("watermark", e))?;
    let wm = core_media::TextWatermark {
        text: p.text,
        position: core_media::WatermarkPosition::parse(&p.position)
            .map_err(|e| PluginError::BadParams { plugin: "watermark", msg: e.to_string() })?,
        opacity: p.opacity,
        font_size: p.font_size,
        margin: p.margin,
        color: core_media::TextWatermark::parse_color(&p.color)
            .map_err(|e| PluginError::BadParams { plugin: "watermark", msg: e.to_string() })?,
    };
    let (w, h) = core_media::watermark_text(input, dest, &wm).map_err(|e| media_failed("watermark", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 批量重命名：{template}。模板变量 {origName}（原文件名）{assetId} {w} {h} {ext}；
/// 扩展名自动追加（模板只写词干）；渲染结果限当前 outDir 内、禁止路径穿越。
/// 目标已存在（同名冲突，如两资产原名相同且模板不含 {assetId}）→ 响亮报错。
fn run_rename(input: &Path, params: &serde_yaml::Value, dest: &Path, o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        template: String,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("rename", e))?;
    let (w, h) = core_media::image_size(input).map_err(|e| media_failed("rename", e))?;
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let mut stem = p.template.clone();
    for (var, val) in [
        ("{origName}", o.original_stem.as_str()),
        ("{assetId}", &o.asset_id.to_string()),
        ("{w}", &w.to_string()),
        ("{h}", &h.to_string()),
        ("{ext}", &ext),
    ] {
        stem = stem.replace(var, val);
    }
    if stem.contains('{') || stem.contains('}') {
        return Err(PluginError::BadParams {
            plugin: "rename",
            msg: format!(
                "模板含未知变量: '{stem}'（支持 {{origName}} {{assetId}} {{w}} {{h}} {{ext}}）"
            ),
        });
    }
    if stem.is_empty() || stem.starts_with('.') || stem.contains('/') || stem.contains('\\') || stem.contains("..") {
        return Err(PluginError::BadParams {
            plugin: "rename",
            msg: format!("模板渲染结果非法: '{stem}'（限当前产物目录内、禁止穿越）"),
        });
    }
    let target = dest
        .parent()
        .map(|d| d.join(format!("{stem}.{ext}")))
        .unwrap_or_else(|| PathBuf::from(format!("{stem}.{ext}")));
    if target.exists() {
        return Err(PluginError::Failed {
            plugin: "rename",
            msg: format!("目标已存在: {}（模板需含 {{assetId}}/{{origName}} 保唯一）", target.display()),
        });
    }
    std::fs::rename(input, &target)
        .map_err(|e| PluginError::Failed { plugin: "rename", msg: format!("重命名失败: {e}") })?;
    Ok(PluginOutput { path: target, width: w, height: h })
}

// ---------- local-infer 门控：修复器（模型缺失 → ModelMissing → 编排器降级） ----------

/// 将 engine-segment / engine-restore 的「模型不可用」错误映射为 ModelMissing
/// （编排器转人工/skip，不炸 job）；其余照常 Failed。
fn segment_err(e: engine_segment::SegmentError) -> PluginError {
    use engine_segment::SegmentError as E;
    match e {
        E::ModelMissing(m) => PluginError::ModelMissing { plugin: "replace_bg", model: m },
        E::NoModelsRoot => PluginError::ModelMissing {
            plugin: "replace_bg",
            model: engine_segment::U2NET.to_string(),
        },
        other => PluginError::Failed { plugin: "replace_bg", msg: other.to_string() },
    }
}

/// 抹主体铺白底（guaranteed 修复器）：u2net 掩码最近邻展开 → 掩码外铺纯色
/// （默认白 ffffff）。确定性：同输入同输出（同网格口径，检测/修复同源）。
#[cfg(feature = "local-infer")]
fn run_replace_bg(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        #[serde(default = "default_bg")]
        bg_color: String,
    }
    fn default_bg() -> String { "ffffff".into() }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("replace_bg", e))?;
    let bg = core_media::TextWatermark::parse_color(&p.bg_color)
        .map_err(|e| PluginError::BadParams { plugin: "replace_bg", msg: e.to_string() })?;
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("replace_bg", e))?;
    if w <= 0 || h <= 0 {
        return Err(PluginError::Failed { plugin: "replace_bg", msg: format!("非法图片尺寸 {w}x{h}") });
    }
    let seg = engine_segment::segmenter(engine_segment::U2NET).map_err(segment_err)?;
    let s = seg
        .run_rgb(w as u32, h as u32, &px)
        .map_err(segment_err)?;
    let mask = s.mask.expand_to(w as u32, h as u32);
    let mut out = px;
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            out[i * 3] = bg[0];
            out[i * 3 + 1] = bg[1];
            out[i * 3 + 2] = bg[2];
        }
    }
    let (w, h) = core_media::encode_rgb_to(w, h, &out, dest, None)
        .map_err(|e| media_failed("replace_bg", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 4× 超分（best_effort）：realesr-general-x4v3 轻量档（~4.7MB，随安装包内置）。
/// 独显检测 → cuda 档（cuda feature 构建时注册 CUDA EP，失败回退 CPU）。
#[cfg(feature = "local-infer")]
fn run_upscale(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        #[serde(default)]
        model: Option<String>,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("upscale", e))?;
    let model = p.model.unwrap_or_else(|| engine_restore::REALESR_GENERAL_X4V3.to_string());
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("upscale", e))?;
    let up = engine_restore::upscale::upscaler(&model).map_err(|e| match e {
        engine_restore::RestoreError::ModelMissing(m) => PluginError::ModelMissing { plugin: "upscale", model: m },
        other => PluginError::Failed { plugin: "upscale", msg: other.to_string() },
    })?;
    let (ow, oh, out) = up
        .run_rgb(w as u32, h as u32, &px)
        .map_err(|e| match e {
            engine_restore::RestoreError::ModelMissing(m) => PluginError::ModelMissing { plugin: "upscale", model: m },
            other => PluginError::Failed { plugin: "upscale", msg: other.to_string() },
        })?;
    let (w, h) = core_media::encode_rgb_to(ow as i32, oh as i32, &out, dest, None)
        .map_err(|e| media_failed("upscale", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 局部重绘（best_effort）：big-lama FP32（512²，paste-back 掩码外保真）。
/// 掩码二选一：{mask: <png/jpg 路径>} 或 {box: [x0,y0,x1,y1] 相对坐标}。
#[cfg(feature = "local-infer")]
fn run_inpaint(input: &Path, params: &serde_yaml::Value, dest: &Path, _o: &PluginOrigin) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        mask: Option<String>,
        #[serde(rename = "box")]
        box_: Option<[f64; 4]>,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("inpaint", e))?;
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("inpaint", e))?;
    let expect = (w as usize) * (h as usize);
    let mask = match (&p.mask, p.box_) {
        (Some(_), Some(_)) => {
            return Err(PluginError::BadParams {
                plugin: "inpaint",
                msg: "mask 与 box 二选一，不要同时给".into(),
            })
        }
        (Some(m), None) => engine_restore::inpaint::load_mask_path(Path::new(m), expect)
            .map_err(|e| PluginError::BadParams { plugin: "inpaint", msg: e.to_string() })?,
        (None, Some(b)) => engine_restore::inpaint::mask_from_box(w as u32, h as u32, b)
            .map_err(|e| PluginError::BadParams { plugin: "inpaint", msg: e.to_string() })?,
        (None, None) => {
            return Err(PluginError::BadParams {
                plugin: "inpaint",
                msg: "缺重绘区域：给 {mask: 路径} 或 {box: [x0,y0,x1,y1]}（相对坐标）".into(),
            })
        }
    };
    let ip = engine_restore::inpaint::inpainter(engine_restore::LAMA_FP32).map_err(|e| match e {
        engine_restore::RestoreError::ModelMissing(m) => PluginError::ModelMissing { plugin: "inpaint", model: m },
        other => PluginError::Failed { plugin: "inpaint", msg: other.to_string() },
    })?;
    let out = ip
        .run_rgb_mask(w as u32, h as u32, &px, &mask)
        .map_err(|e| match e {
            engine_restore::RestoreError::ModelMissing(m) => PluginError::ModelMissing { plugin: "inpaint", model: m },
            other => PluginError::Failed { plugin: "inpaint", msg: other.to_string() },
        })?;
    let (w, h) = core_media::encode_rgb_to(w, h, &out, dest, None)
        .map_err(|e| media_failed("inpaint", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: w, height: h })
}

/// 静态注册表（编译进二进制）。
pub static PLUGINS: &[PluginEntry] = &[
    PluginEntry {
        id: "resize",
        name: "等比缩放（长边 ≤ max_side，只缩不放）",
        cost: CostTier::Ms,
        run: run_resize,
    },
    PluginEntry {
        id: "convert",
        name: "格式转换（{format: jpg|jpeg|png|webp, quality?}；webp 无损）",
        cost: CostTier::Ms,
        run: run_convert,
    },
    PluginEntry {
        id: "compress",
        name: "质量压缩（{quality: 1..=100}；仅 jpeg，png/webp 响亮拒绝）",
        cost: CostTier::Ms,
        run: run_compress,
    },
    PluginEntry {
        id: "watermark",
        name: "文字水印（{text, position, opacity, font_size, margin, color}；内嵌 Noto Sans SC）",
        cost: CostTier::Ms,
        run: run_watermark,
    },
    PluginEntry {
        id: "rename",
        name: "批量重命名（{template}；变量 {origName}/{assetId}/{w}/{h}/{ext}）",
        cost: CostTier::Ms,
        run: run_rename,
    },
    // C3 首个修复插件（guaranteed）：u2net 掩码 → 目标占比裁剪窗 → 像素精确裁剪。
    #[cfg(feature = "local-infer")]
    PluginEntry {
        id: "smart_crop",
        name: "智能裁剪（主体占比收敛到 fixer.params {min,max}，默认 0.65~0.85）",
        cost: CostTier::Sec,
        run: run_smart_crop,
    },
    // C4 修复器：replace_bg（guaranteed）/ upscale、inpaint（best_effort）。
    // 模型缺失 → ModelMissing → 编排器按 fixer_not_available 同语义转人工，不炸 job。
    #[cfg(feature = "local-infer")]
    PluginEntry {
        id: "replace_bg",
        name: "抹主体铺白底（{bg_color: RRGGBB}，默认白；u2net 掩码外铺色）",
        cost: CostTier::Sec,
        run: run_replace_bg,
    },
    #[cfg(feature = "local-infer")]
    PluginEntry {
        id: "upscale",
        name: "4× 超分（realesr-general-x4v3 轻量档；独显检测 → cuda 档）",
        cost: CostTier::Sec,
        run: run_upscale,
    },
    #[cfg(feature = "local-infer")]
    PluginEntry {
        id: "inpaint",
        name: "局部重绘（big-lama FP32 512²；{mask: 路径} 或 {box: 相对矩形}）",
        cost: CostTier::Sec,
        run: run_inpaint,
    },
];

/// 全量插件 id（**编译期清单，不随 `local-infer` 门控变化**）——装载期 lint 用。
///
/// 为什么需要它：web 容器（`local-infer` 关）里 `smart_crop`/`replace_bg`/`upscale`/`inpaint`
/// **不进** `PLUGINS`（模型推理依赖被裁掉）；但「容器没编这个插件」是**部署态能力缺失**，
/// 不是 pack 作者的配置错误 —— 引擎运行时会按 `fixer_not_available` 转人工（不炸 job）。
/// lint 若只看 `PLUGINS`，会把内置电商包（R1 upscale / R2 replace_bg / R3 inpaint /
/// R4 smart_crop）判成「未知插件」→ 界面门禁把默认包拒了（C8 验收抓到过这个假阳性）。
pub const ALL_PLUGIN_IDS: &[&str] = &[
    "resize",
    "convert",
    "compress",
    "watermark",
    "rename",
    "smart_crop",
    "replace_bg",
    "upscale",
    "inpaint",
];

/// 插件 id 是否属于「全量清单」（lint 判据；不看当前构建是否编了该插件）。
pub fn plugin_id_known(id: &str) -> bool {
    ALL_PLUGIN_IDS.contains(&id)
}

/// 全量插件 id 列表（错误信息用）。
pub fn known_ids_all() -> String {
    ALL_PLUGIN_IDS.join(", ")
}

/// C3 smart_crop（u2net 掩码裁剪；local-infer 门控）。
#[cfg(feature = "local-infer")]
fn run_smart_crop(
    input: &Path,
    params: &serde_yaml::Value,
    dest: &Path,
    _o: &PluginOrigin,
) -> Result<PluginOutput, PluginError> {
    #[derive(serde::Deserialize)]
    struct P {
        min: Option<f64>,
        max: Option<f64>,
    }
    let p: P = serde_yaml::from_value(params.clone()).map_err(|e| yaml_bad("smart_crop", e))?;
    let min = p.min.unwrap_or(0.65);
    let max = p.max.unwrap_or(0.85);
    if !(min >= 0.0 && max <= 1.0 && min < max) {
        return Err(PluginError::BadParams {
            plugin: "smart_crop",
            msg: format!("要求 0 ≤ min < max ≤ 1，实际 min={min} max={max}"),
        });
    }
    let (w, h, px) = core_media::rgb8(input).map_err(|e| media_failed("smart_crop", e))?;
    if w <= 0 || h <= 0 {
        return Err(PluginError::Failed { plugin: "smart_crop", msg: format!("非法图片尺寸 {w}x{h}") });
    }
    let seg = engine_segment::segmenter(engine_segment::U2NET).map_err(segment_err)?;
    let s = seg
        .run_rgb(w as u32, h as u32, &px)
        .map_err(segment_err)?;
    let Some((win, _predicted)) = s.crop_window((min + max) / 2.0) else {
        return Err(PluginError::Failed {
            plugin: "smart_crop",
            msg: "无前景（掩码全空）：无法确定主体，裁剪拒执行".into(),
        });
    };
    // 网格（320²，inclusive）→ 像素矩形：x 取格起点 floor、x2 取格终点 ceil，
    // 双向钳进 [0, iw] 且保证 cw ≥ 1（窄于 320 的图窗口可能落在末格，映射不得越界）
    let side = engine_segment::SIDE as i32;
    let (iw, ih) = (w, h);
    let x = ((win[0] as i32) * iw / side).clamp(0, iw - 1);
    let y = ((win[1] as i32) * ih / side).clamp(0, ih - 1);
    let x2 = ((((win[2] as i32) + 1) * iw + side - 1) / side).clamp(x + 1, iw);
    let y2 = ((((win[3] as i32) + 1) * ih + side - 1) / side).clamp(y + 1, ih);
    let (cw, ch) = (x2 - x, y2 - y);
    let (ow, oh) = core_media::crop_to(input, dest, x, y, cw, ch)
        .map_err(|e| media_failed("smart_crop", e))?;
    Ok(PluginOutput { path: dest.to_path_buf(), width: ow, height: oh })
}

/// 查表（id → 条目）。
pub fn plugin_lookup(id: &str) -> Option<&'static PluginEntry> {
    PLUGINS.iter().find(|e| e.id == id)
}

/// 已知插件 id 列表（错误信息用）。
pub fn plugin_known_ids() -> String {
    PLUGINS.iter().map(|e| e.id).collect::<Vec<_>>().join(", ")
}

/// 顶层分发：未知 id 显式报 Unknown（不静默跳过）。
pub fn plugin_run(
    id: &str,
    input: &Path,
    params: &serde_yaml::Value,
    dest: &Path,
    origin: &PluginOrigin,
) -> Result<PluginOutput, PluginError> {
    let entry = plugin_lookup(id).ok_or_else(|| PluginError::Unknown {
        id: id.to_string(),
        known: plugin_known_ids(),
    })?;
    (entry.run)(input, params, dest, origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C8：装载期 lint 用全量插件清单判「未知插件」——注册表里有的 id 必须在清单里；
    /// local-infer 开（桌面态）时两者应完全一致（清单不许漏也不许多）。
    #[test]
    fn all_plugin_ids_covers_registry() {
        for p in PLUGINS {
            assert!(plugin_id_known(p.id), "插件 {} 不在 ALL_PLUGIN_IDS 清单里", p.id);
        }
        #[cfg(feature = "local-infer")]
        {
            let mut reg: Vec<&str> = PLUGINS.iter().map(|p| p.id).collect();
            let mut all: Vec<&str> = ALL_PLUGIN_IDS.to_vec();
            reg.sort_unstable();
            all.sort_unstable();
            assert_eq!(reg, all, "全量清单与注册表（local-infer 开）不一致");
        }
        assert!(plugin_id_known("upscale") && !plugin_id_known("no_such_plugin"));
    }

    fn origin() -> PluginOrigin {
        PluginOrigin { original_stem: "IMG_2043".into(), asset_id: 42 }
    }

    fn jpg_bytes(w: u8, h: u8) -> Vec<u8> {
        // 8x8 JPEG via image crate（dev-deps）
        let img = image::RgbImage::from_fn(w as u32, h as u32, |x, y| {
            image::Rgb([x as u8, y as u8, 128])
        });
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut buf)
            .encode_image(&img)
            .unwrap();
        buf
    }

    #[test]
    fn unknown_plugin_fails_loudly() {
        let err = plugin_run(
            "no_such_plugin",
            std::path::Path::new("/x.jpg"),
            &serde_yaml::Value::Null,
            std::path::Path::new("/y.jpg"),
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Unknown { .. }));
        assert!(err.to_string().contains("未知插件"));
        assert!(err.to_string().contains("resize"));
        // C3/C4：修复器已注册（local-infer 构建）
        #[cfg(feature = "local-infer")]
        for id in ["smart_crop", "replace_bg", "upscale", "inpaint"] {
            assert!(plugin_lookup(id).is_some(), "{id} 应已注册");
        }
        // C4 do 型五件（classic 档，双端注册）
        for id in ["resize", "convert", "compress", "watermark", "rename"] {
            assert!(plugin_lookup(id).is_some(), "{id} 应已注册");
        }
    }

    #[test]
    fn resize_plugin_scales_and_keeps_format() {
        let dir = std::env::temp_dir().join(format!("damplug-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(64, 40)).unwrap();
        let dest = dir.join("out.jpg");
        let out = plugin_run(
            "resize",
            &src,
            &serde_yaml::from_str("{max_side: 32}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap();
        assert_eq!(dest, out.path);
        assert!(out.width <= 32 && out.height <= 32, "{out:?}");
        assert_eq!(out.width.max(out.height), 32, "长边 = max_side");
        // 只缩不放：小图原样输出尺寸
        let small = dir.join("small.jpg");
        std::fs::write(&small, jpg_bytes(8, 8)).unwrap();
        let out2 = plugin_run(
            "resize",
            &small,
            &serde_yaml::from_str("{max_side: 64}").unwrap(),
            &dir.join("small_out.jpg"),
            &origin(),
        )
        .unwrap();
        assert_eq!((out2.width, out2.height), (8, 8));
        // 坏参数响亮失败
        let err = plugin_run(
            "resize",
            &src,
            &serde_yaml::from_str("{max_side: 0}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        let err = plugin_run(
            "resize",
            &src,
            &serde_yaml::from_str("{}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_plugin_switches_extension() {
        let dir = std::env::temp_dir().join(format!("damplug-cv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(48, 32)).unwrap();
        // 编排器按当前链 ext 预命名（.jpg），convert 产物应为 .webp/.png
        let dest = dir.join("42_conv.jpg");
        let out = plugin_run(
            "convert",
            &src,
            &serde_yaml::from_str("{format: webp}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap();
        assert_eq!(out.path, dir.join("42_conv.webp"));
        assert_eq!(core_media::image_size(&out.path).unwrap(), (48, 32));
        let out2 = plugin_run(
            "convert",
            &src,
            &serde_yaml::from_str("{format: png}").unwrap(),
            &dir.join("42_conv2.jpg"),
            &origin(),
        )
        .unwrap();
        assert_eq!(out2.path, dir.join("42_conv2.png"));
        // 坏格式响亮
        let err = plugin_run(
            "convert",
            &src,
            &serde_yaml::from_str("{format: tiff}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compress_plugin_jpeg_only_and_quality_meaningful() {
        let dir = std::env::temp_dir().join(format!("damplug-cp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(64, 64)).unwrap();
        let q95 = dir.join("42_q95.jpg");
        let q30 = dir.join("42_q30.jpg");
        let o95 = plugin_run("compress", &src, &serde_yaml::from_str("{quality: 95}").unwrap(), &q95, &origin()).unwrap();
        let o30 = plugin_run("compress", &src, &serde_yaml::from_str("{quality: 30}").unwrap(), &q30, &origin()).unwrap();
        assert_eq!((o95.width, o95.height), (64, 64));
        let (s95, s30) = (
            std::fs::metadata(&q95).unwrap().len(),
            std::fs::metadata(&q30).unwrap().len(),
        );
        assert!(s30 < s95, "q30({s30}) 应小于 q95({s95})");
        // png 源响亮拒绝（无损无质量语义）
        let png = dir.join("in.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nbroken").unwrap();
        let err = plugin_run("compress", &png, &serde_yaml::from_str("{quality: 80}").unwrap(), &dir.join("o.png"), &origin()).unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        assert!(err.to_string().contains("无损"));
        // quality 越界
        let err = plugin_run("compress", &src, &serde_yaml::from_str("{quality: 101}").unwrap(), &q95, &origin()).unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watermark_plugin_draws_and_validates() {
        let dir = std::env::temp_dir().join(format!("damplug-wm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(120, 80)).unwrap();
        let dest = dir.join("42_wm.jpg");
        let out = plugin_run(
            "watermark",
            &src,
            &serde_yaml::from_str("{text: \"店名Shop\", opacity: 90, font_size: 24, color: \"ff0000\"}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap();
        assert_eq!(out.path, dest);
        assert_eq!((out.width, out.height), (120, 80));
        let (_w, _h, px) = core_media::rgb8(&dest).unwrap();
        assert!(px.iter().any(|&c| c < 200), "水印应落墨");
        // 缺 text 响亮
        let err = plugin_run("watermark", &src, &serde_yaml::Value::Null, &dest, &origin()).unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        // emoji 缺字形响亮（内容校验在媒体层，Failed 也响亮）
        let err = plugin_run(
            "watermark",
            &src,
            &serde_yaml::from_str("{text: \"😀\"}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Failed { .. }), "{err}");
        assert!(err.to_string().contains("不支持"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_plugin_template_and_guards() {
        let dir = std::env::temp_dir().join(format!("damplug-rn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("42_step.jpg");
        std::fs::write(&src, jpg_bytes(64, 32)).unwrap();
        let dest = dir.join("42_step.jpg"); // 编排器名 = {assetId}_{sid}.{ext}
        let out = plugin_run(
            "rename",
            &src,
            &serde_yaml::from_str("{template: \"{origName}_{w}x{h}\"}").unwrap(),
            &dest,
            &origin(),
        )
        .unwrap();
        assert_eq!(out.path, dir.join("IMG_2043_64x32.jpg"));
        assert!(out.path.is_file());
        // 未知变量响亮
        std::fs::write(&dir.join("43_step.jpg"), jpg_bytes(8, 8)).unwrap();
        let err = plugin_run(
            "rename",
            &dir.join("43_step.jpg"),
            &serde_yaml::from_str("{template: \"{bogus}\"}").unwrap(),
            &dir.join("43_step.jpg"),
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        assert!(err.to_string().contains("未知变量"));
        // 路径穿越响亮
        let err = plugin_run(
            "rename",
            &dir.join("43_step.jpg"),
            &serde_yaml::from_str("{template: \"../{origName}\"}").unwrap(),
            &dir.join("43_step.jpg"),
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::BadParams { .. }));
        // 目标冲突响亮（已存在的产物名）
        std::fs::write(&dir.join("IMG_2043_dup.jpg"), jpg_bytes(8, 8)).unwrap();
        let err = plugin_run(
            "rename",
            &dir.join("43_step.jpg"),
            &serde_yaml::from_str("{template: \"IMG_2043_dup\"}").unwrap(),
            &dir.join("43_step.jpg"),
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::Failed { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "local-infer")]
    #[test]
    fn replace_bg_missing_model_maps_to_model_missing() {
        // 模型根指向空目录 → ModelMissing（编排器转人工），不是 Failed/panic。
        // 进程级全局态：**必须**走 ModelsRootScope —— 原先裸 set_models_root 无锁，
        // 被同二进制并行的 C3 真模型用例改根 → 本断言假失败（卡 t_39b4891a）。
        let _seg_scope = engine_segment::ModelsRootScope::set("/tmp/c4-empty-root");
        let dir = std::env::temp_dir().join(format!("damplug-rb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(16, 16)).unwrap();
        let err = plugin_run(
            "replace_bg",
            &src,
            &serde_yaml::Value::Null,
            &dir.join("out.jpg"),
            &origin(),
        )
        .unwrap_err();
        assert!(matches!(err, PluginError::ModelMissing { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "local-infer")]
    #[test]
    fn inpaint_requires_exactly_one_region() {
        let dir = std::env::temp_dir().join(format!("damplug-ip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.jpg");
        std::fs::write(&src, jpg_bytes(16, 16)).unwrap();
        for params in ["{}", "{mask: \"/a.png\", box: [0,0,0.5,0.5]}"] {
            let err = plugin_run(
                "inpaint",
                &src,
                &serde_yaml::from_str(params).unwrap(),
                &dir.join("out.jpg"),
                &origin(),
            )
            .unwrap_err();
            assert!(matches!(err, PluginError::BadParams { .. }), "{params} → {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
